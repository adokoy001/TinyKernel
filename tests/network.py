#!/usr/bin/env python3
"""Exercise the real RTL8139 driver and IPv4/IPv6 stacks in QEMU.

The peer below exchanges Ethernet frames through an inherited socket pair. It
never connects to an external host and deliberately returns damaged packets in
some cases. A separate QEMU user-network test checks the ordinary launch setup.
Only the Python standard library is required. QEMU/QEMU_DATADIR select QEMU;
all other environment variables, including LD_LIBRARY_PATH, are inherited.
"""
import ipaddress
import os
from pathlib import Path
import select
import shutil
import socket
import struct
import subprocess
import sys
import tempfile
import time

ROOT = Path(__file__).resolve().parents[1]
OUT = ROOT / "build"
PROMPT = b"tane> "
USER_PROMPT = b"tane$ "
TIME_LIMIT = 60.0
GUEST_MAC = bytes.fromhex("525400123456")
PEER_MAC = bytes.fromhex("020000000002")
GUEST4 = ipaddress.IPv4Address("10.0.2.15").packed
PEER4 = ipaddress.IPv4Address("10.0.2.2").packed
GUEST6 = ipaddress.IPv6Address("fd00::15").packed
PEER6 = ipaddress.IPv6Address("fd00::2").packed


def checksum(data):
    if len(data) & 1:
        data += b"\0"
    total = sum(struct.unpack(f"!{len(data) // 2}H", data))
    while total >> 16:
        total = (total & 65535) + (total >> 16)
    return (~total) & 65535


def icmp6_checksum(source, target, payload):
    return checksum(source + target + struct.pack("!I3xB", len(payload), 58) + payload)


def ethernet(destination, kind, payload):
    # Hardware sends at least 60 bytes before the four-byte Ethernet FCS.
    return (destination + PEER_MAC + struct.pack("!H", kind) + payload).ljust(60, b"\0")


def ipv4(source, target, payload):
    header = struct.pack("!BBHHHBBH4s4s", 0x45, 0, 20 + len(payload), 1, 0, 64, 1, 0, source, target)
    header = header[:10] + struct.pack("!H", checksum(header)) + header[12:]
    return header + payload


def ipv6(source, target, payload, hops=64):
    return struct.pack("!IHBB16s16s", 6 << 28, len(payload), 58, hops, source, target) + payload


class Peer:
    """Small validating ARP/ND/echo responder, independent of the Rust stack."""
    def __init__(self, connection):
        self.connection = connection
        connection.setblocking(False)
        self.input = bytearray()
        self.output = bytearray()
        self.mode = "reply"  # reply, corrupt, forged, silent
        self.arp = 0
        self.nd = 0
        self.echo4 = 0
        self.echo6 = 0
        self.frames = 0
        self.answered_arp = 0
        self.answered_nd = 0
        self.answered4 = 0
        self.answered6 = 0

    def queue(self, frame):
        self.output.extend(struct.pack("!I", len(frame)) + frame)

    def read(self):
        while True:
            try:
                data = self.connection.recv(65536)
            except BlockingIOError:
                break
            if not data:
                raise AssertionError("QEMU closed the emulated Ethernet connection")
            self.input.extend(data)
        while len(self.input) >= 4:
            size = struct.unpack("!I", self.input[:4])[0]
            if size > 65536:
                raise AssertionError(f"Invalid QEMU Ethernet frame length {size}")
            if len(self.input) < 4 + size:
                break
            frame = bytes(self.input[4:4 + size])
            del self.input[:4 + size]
            self.receive(frame)

    def write(self):
        if self.output:
            try:
                count = self.connection.send(self.output)
                del self.output[:count]
            except BlockingIOError:
                pass

    def receive(self, frame):
        self.frames += 1
        if len(frame) < 14:
            raise AssertionError("Guest sent a truncated Ethernet frame")
        if frame[6:12] != GUEST_MAC:
            raise AssertionError(f"Unexpected guest MAC {frame[6:12].hex()}")
        kind = struct.unpack("!H", frame[12:14])[0]
        packet = frame[14:]
        if kind == 0x0806:
            self.receive_arp(frame[6:12], packet)
        elif kind == 0x0800:
            self.receive_ipv4(frame[6:12], packet)
        elif kind == 0x86dd:
            self.receive_ipv6(frame[6:12], packet)

    def receive_arp(self, source_mac, packet):
        if len(packet) < 28:
            return
        if packet[:8] == bytes.fromhex("0001080006040002"):
            if packet[8:14] != GUEST_MAC or packet[14:18] != GUEST4 or packet[18:24] != PEER_MAC or packet[24:28] != PEER4:
                raise AssertionError("Guest ARP reply fields are incorrect")
            self.answered_arp += 1
            return
        if packet[:8] != bytes.fromhex("0001080006040001"):
            return
        if packet[24:28] != PEER4:
            return
        if packet[8:14] != source_mac or packet[14:18] != GUEST4:
            raise AssertionError("Guest sent inconsistent ARP sender fields")
        self.arp += 1
        reply = bytes.fromhex("0001080006040002") + PEER_MAC + PEER4 + source_mac + GUEST4
        self.queue(ethernet(source_mac, 0x0806, reply))

    def receive_ipv4(self, source_mac, packet):
        if len(packet) < 20 or packet[0] >> 4 != 4:
            raise AssertionError("Guest sent malformed IPv4")
        header_size = (packet[0] & 15) * 4
        length = struct.unpack("!H", packet[2:4])[0]
        if header_size < 20 or length < header_size or length > len(packet):
            raise AssertionError("Guest IPv4 length fields are inconsistent")
        if checksum(packet[:header_size]) != 0:
            raise AssertionError("Guest IPv4 header checksum is incorrect")
        if packet[9] != 1 or packet[16:20] != PEER4:
            return
        payload = packet[header_size:length]
        if len(payload) < 8:
            return
        if packet[12:16] != GUEST4 or checksum(payload) != 0:
            raise AssertionError("Guest ICMPv4 source/checksum is incorrect")
        if payload[0] == 0:
            if payload[4:] != self.request_body():
                raise AssertionError("Guest did not preserve the incoming ICMPv4 echo contents")
            self.answered4 += 1
            return
        if payload[0] != 8:
            return
        self.echo4 += 1
        if self.mode == "silent":
            return
        reply = b"\0" + payload[1:2] + b"\0\0" + payload[4:]
        if self.mode == "forged":
            reply = reply[:5] + bytes((reply[5] ^ 1,)) + reply[6:]
        reply = reply[:2] + struct.pack("!H", checksum(reply)) + reply[4:]
        if self.mode == "corrupt":
            reply = reply[:2] + bytes((reply[2] ^ 1,)) + reply[3:]
        self.queue(ethernet(source_mac, 0x0800, ipv4(PEER4, GUEST4, reply)))

    def receive_ipv6(self, source_mac, packet):
        if len(packet) < 40 or packet[0] >> 4 != 6:
            raise AssertionError("Guest sent malformed IPv6")
        length = struct.unpack("!H", packet[4:6])[0]
        if 40 + length > len(packet):
            raise AssertionError("Guest IPv6 payload length is inconsistent")
        if packet[6] != 58:
            return
        source, target, payload = packet[8:24], packet[24:40], packet[40:40 + length]
        if len(payload) < 8:
            raise AssertionError("Guest sent truncated ICMPv6")
        if icmp6_checksum(source, target, payload) != 0:
            raise AssertionError("Guest ICMPv6 pseudoheader checksum is incorrect")
        if payload[0] == 135 and len(payload) >= 24 and payload[8:24] == PEER6:
            if packet[7] != 255:
                raise AssertionError("Neighbor solicitation must use hop limit 255")
            self.nd += 1
            reply = bytes((136, 0, 0, 0)) + struct.pack("!I", 0x60000000) + PEER6 + bytes((2, 1)) + PEER_MAC
            reply = reply[:2] + struct.pack("!H", icmp6_checksum(PEER6, source, reply)) + reply[4:]
            self.queue(ethernet(source_mac, 0x86dd, ipv6(PEER6, source, reply, 255)))
        elif payload[0] == 136 and target == PEER6:
            if source != GUEST6 or packet[7] != 255 or len(payload) < 32 or payload[8:24] != GUEST6:
                raise AssertionError("Guest neighbor advertisement fields are incorrect")
            if payload[24:32] != bytes((2, 1)) + GUEST_MAC:
                raise AssertionError("Guest neighbor advertisement must include its MAC")
            self.answered_nd += 1
        elif payload[0] == 129 and target == PEER6:
            if source != GUEST6 or payload[4:] != self.request_body():
                raise AssertionError("Guest did not preserve the incoming ICMPv6 echo contents")
            self.answered6 += 1
        elif payload[0] == 128 and target == PEER6:
            if source != GUEST6:
                raise AssertionError("Guest ICMPv6 source is incorrect")
            self.echo6 += 1
            if self.mode == "silent":
                return
            reply = bytes((129, 0, 0, 0)) + payload[4:]
            if self.mode == "forged":
                reply = reply[:5] + bytes((reply[5] ^ 1,)) + reply[6:]
            reply = reply[:2] + struct.pack("!H", icmp6_checksum(PEER6, source, reply)) + reply[4:]
            if self.mode == "corrupt":
                reply = reply[:2] + bytes((reply[2] ^ 1,)) + reply[3:]
            self.queue(ethernet(source_mac, 0x86dd, ipv6(PEER6, source, reply)))

    @staticmethod
    def request_body():
        return struct.pack("!HH", 0x8abc, 0x1234) + b"peer request 123"

    def request_arp(self):
        request = bytes.fromhex("0001080006040001") + PEER_MAC + PEER4 + bytes(6) + GUEST4
        self.queue(ethernet(bytes((255,)) * 6, 0x0806, request))

    def request_nd(self):
        target = bytes.fromhex("ff0200000000000000000001ff000015")
        request = bytes((135, 0, 0, 0)) + bytes(4) + GUEST6 + bytes((1, 1)) + PEER_MAC
        request = request[:2] + struct.pack("!H", icmp6_checksum(PEER6, target, request)) + request[4:]
        self.queue(ethernet(bytes.fromhex("3333ff000015"), 0x86dd, ipv6(PEER6, target, request, 255)))

    def request_echo(self, version):
        request = bytes((8 if version == 4 else 128, 0, 0, 0)) + self.request_body()
        if version == 4:
            request = request[:2] + struct.pack("!H", checksum(request)) + request[4:]
            self.queue(ethernet(GUEST_MAC, 0x0800, ipv4(PEER4, GUEST4, request)))
        else:
            request = request[:2] + struct.pack("!H", icmp6_checksum(PEER6, GUEST6, request)) + request[4:]
            self.queue(ethernet(GUEST_MAC, 0x86dd, ipv6(PEER6, GUEST6, request)))

    def malformed(self):
        # The real NIC receives these, so this also checks receive-ring recovery.
        self.queue(b"\0" * 7)
        self.queue(ethernet(GUEST_MAC, 0x0806, b"\0" * 27))
        self.queue(ethernet(GUEST_MAC, 0x0800, b"\x45" + b"\0" * 18))
        self.queue(ethernet(GUEST_MAC, 0x0800, b"\x4f" + b"\0" * 59))
        self.queue(ethernet(GUEST_MAC, 0x86dd, b"\x60" + b"\0" * 38))
        self.queue(ethernet(GUEST_MAC, 0x86dd, ipv6(PEER6, GUEST6, b"\x81\0\0\0")))


class Machine:
    def __init__(self, name, deadline, backend="peer"):
        self.name = name
        self.deadline = deadline
        self.backend = backend
        self.prompt = PROMPT
        self.output = bytearray()
        self.process = None
        self.peer = None
        self.stderr = None
        self.temporary = tempfile.TemporaryDirectory(prefix="tane-network-")

    def remaining(self, limit):
        left = self.deadline - time.monotonic()
        if left <= 0:
            raise AssertionError(f"Network test exceeded its {TIME_LIMIT:g} second deadline")
        return min(left, limit)

    def start(self):
        image = Path(self.temporary.name) / "tane-os.img"
        shutil.copyfile(OUT / "tane-os.img", image)
        command = [os.environ.get("QEMU", "qemu-system-x86_64")]
        if os.environ.get("QEMU_DATADIR"):
            command += ["-L", os.environ["QEMU_DATADIR"]]
        command += ["-machine", "pc,accel=tcg", "-m", "64M", "-display", "none",
                    "-drive", f"file={image},format=raw,if=floppy,readonly=on",
                    "-boot", "order=a", "-serial", "stdio"]
        passed = ()
        other = None
        if self.backend == "peer":
            local, other = socket.socketpair()
            self.peer = Peer(local)
            passed = (other.fileno(),)
            command += ["-netdev", f"socket,id=wire,fd={other.fileno()}"]
        elif self.backend == "user":
            command += ["-netdev", "user,id=wire,ipv6-net=fd00::/64"]
        else:
            command += ["-net", "none"]
        if self.backend in ("peer", "user"):
            command += ["-device", "rtl8139,netdev=wire,mac=52:54:00:12:34:56,romfile="]
        self.stderr = (OUT / f"network-qemu-{self.name}.stderr").open("wb")
        self.process = subprocess.Popen(command, cwd=ROOT, stdin=subprocess.PIPE,
                                        stdout=subprocess.PIPE, stderr=self.stderr,
                                        pass_fds=passed)
        if other is not None:
            other.close()
        os.set_blocking(self.process.stdout.fileno(), False)
        self.wait_for(b"READY\r\n", timeout=6)
        self.wait_for(self.prompt)
        return self

    def drain(self, timeout=0):
        if self.process.poll() is not None:
            raise AssertionError(f"QEMU exited ({self.process.returncode}); see network-qemu-{self.name}.stderr")
        inputs = [self.process.stdout.fileno()]
        outputs = []
        if self.peer is not None:
            inputs.append(self.peer.connection)
            if self.peer.output:
                outputs.append(self.peer.connection)
        readable, writable, _ = select.select(inputs, outputs, [], timeout)
        if self.process.stdout.fileno() in readable:
            data = os.read(self.process.stdout.fileno(), 65536)
            if not data:
                raise AssertionError("QEMU closed its serial output")
            self.output.extend(data)
        if self.peer is not None:
            if self.peer.connection in readable:
                self.peer.read()
            if self.peer.connection in writable or self.peer.output:
                self.peer.write()

    def wait_for(self, expected, start=0, timeout=3):
        stop = time.monotonic() + self.remaining(timeout)
        while expected not in self.output[start:]:
            if time.monotonic() >= stop:
                tail = bytes(self.output[-1200:]).decode("ascii", errors="backslashreplace")
                raise AssertionError(f"Timed out waiting for {expected!r}; serial tail:\n{tail}")
            self.drain(min(0.01, max(0, stop - time.monotonic())))
        return bytes(self.output[start:])

    def send(self, data):
        for byte in data:
            self.remaining(1)
            os.write(self.process.stdin.fileno(), bytes((byte,)))
            self.drain(0.002)

    def command(self, text, timeout=3):
        start = len(self.output)
        self.send(text.encode("ascii") + b"\r")
        return self.wait_for(self.prompt, start, timeout)

    def wait_peer(self, predicate, timeout=2):
        stop = time.monotonic() + self.remaining(timeout)
        while not predicate(self.peer):
            if time.monotonic() >= stop:
                raise AssertionError("The idle guest did not answer an incoming network request")
            self.drain(min(0.01, max(0, stop - time.monotonic())))

    def close(self):
        if self.process is not None:
            if self.process.poll() is None:
                self.process.terminate()
                try:
                    self.process.wait(timeout=2)
                except subprocess.TimeoutExpired:
                    self.process.kill()
                    self.process.wait(timeout=2)
            self.process.stdin.close()
            self.process.stdout.close()
            self.process = None
        if self.peer is not None:
            self.peer.connection.close()
            self.peer = None
        if self.stderr is not None:
            self.stderr.close()
            self.stderr = None
        if self.temporary is not None:
            self.temporary.cleanup()
            self.temporary = None
        (OUT / f"network-qemu-{self.name}.serial").write_bytes(self.output)


def contains(result, expected):
    if expected not in result:
        raise AssertionError(f"Missing {expected!r} in response {result!r}")


def main():
    if not (OUT / "tane-os.img").is_file():
        sys.exit("Missing build/tane-os.img. Run python3 build.py first.")
    deadline = time.monotonic() + TIME_LIMIT
    machines = []
    checks = []

    def checked(name):
        checks.append(name)
        print(f"PASS {name}", flush=True)

    def machine(name, backend="peer"):
        instance = Machine(name, deadline, backend)
        machines.append(instance)
        return instance.start()

    try:
        guest = machine("peer")
        status = guest.command("net")
        contains(status, b"10.0.2.15")
        contains(status, b"fd00::15")
        checked("RTL8139 is detected with the configured IPv4 and IPv6 addresses")

        result = guest.command("net ping 10.0.2.2 --count 3 --timeout 500")
        contains(result, b"3 sent, 3 received")
        if guest.peer.arp < 1 or guest.peer.echo4 != 3:
            raise AssertionError("IPv4 ping must perform real ARP and send three valid Ethernet packets")
        checked("IPv4 ARP discovery and three checksummed ICMP echo exchanges")

        result = guest.command("ping fd00::2 --count 2 --timeout 500")
        contains(result, b"2 sent, 2 received")
        if guest.peer.nd < 1 or guest.peer.echo6 != 2:
            raise AssertionError("IPv6 ping must perform real Neighbor Discovery and send two valid packets")
        checked("IPv6 Neighbor Discovery and checksummed ICMPv6 echo exchanges")

        guest.peer.request_arp()
        guest.wait_peer(lambda peer: peer.answered_arp == 1)
        guest.peer.request_echo(4)
        guest.wait_peer(lambda peer: peer.answered4 == 1)
        guest.peer.request_nd()
        guest.wait_peer(lambda peer: peer.answered_nd == 1)
        guest.peer.request_echo(6)
        guest.wait_peer(lambda peer: peer.answered6 == 1)
        checked("The idle shell answers ARP, multicast Neighbor Discovery and incoming IPv4/IPv6 echo")

        guest.peer.malformed()
        guest.drain(0.05)
        for address in ("10.0.2.2", "fd00::2"):
            contains(guest.command(f"ping {address} --count 1 --timeout 500"), b"1 sent, 1 received")
        checked("Short and malformed frames are ignored and the receive ring remains usable")

        # Each packet includes receive status and Ethernet padding; 120 replies
        # force even an 8 KiB RTL8139 ring to wrap while the four TX slots cycle.
        for number in range(12):
            address = "10.0.2.2" if number % 2 == 0 else "fd00::2"
            contains(guest.command(f"ping {address} --count 10 --timeout 500", timeout=6),
                     b"10 sent, 10 received")
        checked("Receive-ring wrap and transmit-slot reuse preserve 120 echo replies")

        guest.peer.mode = "corrupt"
        for address in ("10.0.2.2", "fd00::2"):
            result = guest.command(f"ping {address} --count 1 --timeout 100")
            contains(result, b"1 sent, 0 received")
            if b"reply from" in result:
                raise AssertionError("A damaged checksum was accepted as an echo reply")
        checked("Corrupt ICMPv4 and ICMPv6 replies cannot satisfy a ping")

        guest.peer.mode = "forged"
        for address in ("10.0.2.2", "fd00::2"):
            result = guest.command(f"ping {address} --count 1 --timeout 100")
            contains(result, b"1 sent, 0 received")
            if b"reply from" in result:
                raise AssertionError("A checksummed reply for another ping identifier was accepted")
        checked("Valid packets for another ping identifier cannot satisfy the active job")

        guest.peer.mode = "silent"
        before = time.monotonic()
        contains(guest.command("ping 10.0.2.2 --count 2 --timeout 100"), b"2 sent, 0 received")
        if time.monotonic() - before > 1.5:
            raise AssertionError("A 100 ms ping timeout failed to bound the job")
        checked("Unanswered requests stop within the configured timeout")

        start = len(guest.output)
        guest.send(b"ping 10.0.2.2 --count 10 --timeout 1000\r")
        guest.drain(0.1)
        guest.send(b"\x03")
        result = guest.wait_for(guest.prompt, start, timeout=1)
        contains(result.lower(), b"cancel")
        checked("Ctrl-C cancels a running ping and restores the shell promptly")

        guest.peer.mode = "reply"
        contains(guest.command("ping 10.0.2.2 --count 1 --timeout 500"), b"1 sent, 1 received")
        checked("A new job succeeds after timeout and cancellation")

        before = guest.peer.frames
        guest.send(b"drop\r")
        guest.prompt = USER_PROMPT
        guest.wait_for(USER_PROMPT)
        contains(guest.command("net"), b"10.0.2.15")
        # Fresh targets require resolution if the authorization gate is
        # accidentally moved after transmit, catching ARP/NS leaks as well.
        for target in ("10.0.2.99", "fd00::99"):
            result = guest.command(f"ping {target} --count 1 --timeout 100")
            contains(result.lower(), b"denied")
        guest.drain(0.05)
        if guest.peer.frames != before:
            raise AssertionError("A denied user-domain ping still transmitted an Ethernet packet")
        checked("User domain can inspect status but cannot transmit a privileged ping")
        guest.close()

        absent = machine("no-nic", "none")
        status = absent.command("net").lower()
        if not any(word in status for word in (b"unavailable", b"not found", b"no rtl8139", b"not detected")):
            raise AssertionError(f"Missing clear no-NIC status: {status!r}")
        result = absent.command("ping 10.0.2.2 --count 1 --timeout 100").lower()
        if b"reply from" in result or b"1 sent, 1 received" in result:
            raise AssertionError("Ping incorrectly succeeded with no network device")
        contains(absent.command("calc 12 * 3"), b"36")
        checked("Booting without a NIC leaves the shell usable and reports unavailable networking")
        absent.close()

        ordinary = machine("slirp", "user")
        contains(ordinary.command("ping 10.0.2.2 --count 2 --timeout 500"), b"2 sent, 2 received")
        checked("The ordinary QEMU user-network gateway answers IPv4 ping")
        contains(ordinary.command("ping fd00::2 --count 2 --timeout 500"), b"2 sent, 2 received")
        checked("The configured QEMU user-network gateway answers IPv6 ping")
        ordinary.close()
    finally:
        for instance in machines:
            instance.close()
    print(f"{len(checks)} network integration checks passed", flush=True)


if __name__ == "__main__":
    try:
        main()
    except (AssertionError, OSError) as error:
        print(f"FAIL {error}", file=sys.stderr, flush=True)
        sys.exit(1)
