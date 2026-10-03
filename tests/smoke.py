#!/usr/bin/env python3
"""Exercise the boot image and real serial/PS/2 drivers in QEMU.

Python standard library only. Run from any directory after build.py.
QEMU selects the executable; QEMU_DATADIR optionally supplies QEMU's -L path.
All other environment variables, including LD_LIBRARY_PATH, are inherited.
Unix sockets are preferred; restricted hosts automatically use local pipes.
"""
import json
import os
from pathlib import Path
import select
import socket
import subprocess
import sys
import tempfile
import time

ROOT = Path(__file__).resolve().parents[1]
OUT = ROOT / "build"
PROMPT = b"tane> "
TIME_LIMIT = 30.0


class Machine:
    def __init__(self, name, deadline):
        self.name = name
        self.deadline = deadline
        self.output = bytearray()
        self.process = None
        self.serial = None
        self.qmp = None
        self.serial_read_fd = None
        self.serial_write_fd = None
        self.qmp_read_fd = None
        self.qmp_write_fd = None
        self.qmp_buffer = bytearray()
        self.pipe_fds = []
        self.transport = "unix sockets"
        self.stderr = None
        self.qmp_id = 0
        self.temporary = tempfile.TemporaryDirectory(prefix="tane-smoke-")

    def remaining(self, limit=3.0):
        remaining = self.deadline - time.monotonic()
        if remaining <= 0:
            raise AssertionError("Integration test exceeded its 30 second deadline")
        return min(limit, remaining)

    def connect(self, path):
        stop = time.monotonic() + self.remaining(5.0)
        while time.monotonic() < stop:
            self.ensure_alive()
            client = socket.socket(socket.AF_UNIX, socket.SOCK_STREAM)
            try:
                client.connect(str(path))
                return client
            except (FileNotFoundError, ConnectionRefusedError):
                client.close()
                time.sleep(0.01)
        raise AssertionError(f"QEMU did not open {path.name}")

    def ensure_alive(self):
        if self.process is not None and self.process.poll() is not None:
            raise AssertionError(f"QEMU exited unexpectedly ({self.process.returncode}); see smoke-qemu-{self.name}.stderr")

    def start(self):
        directory = Path(self.temporary.name)
        serial_path = directory / "serial.sock"
        qmp_path = directory / "qmp.sock"
        use_pipes = os.environ.get("TANE_TEST_TRANSPORT") == "pipe"
        if not use_pipes:
            try:
                probe = socket.socket(socket.AF_UNIX, socket.SOCK_STREAM)
                probe.close()
            except PermissionError:
                use_pipes = True
        command = [os.environ.get("QEMU", "qemu-system-x86_64")]
        if os.environ.get("QEMU_DATADIR"):
            command += ["-L", os.environ["QEMU_DATADIR"]]
        command += [
            "-machine", "pc,accel=tcg", "-m", "64M",
            "-drive", f"file={OUT / 'tane-os.img'},format=raw,if=floppy",
            "-boot", "order=a", "-display", "none", "-net", "none",
        ]
        if use_pipes:
            self.transport = "local pipes (Unix sockets unavailable or disabled)"
            for suffix in (".in", ".out"):
                os.mkfifo(str(qmp_path) + suffix, 0o600)
            self.qmp_write_fd = os.open(str(qmp_path) + ".in", os.O_RDWR | os.O_NONBLOCK)
            self.qmp_read_fd = os.open(str(qmp_path) + ".out", os.O_RDWR | os.O_NONBLOCK)
            self.pipe_fds = [self.qmp_write_fd, self.qmp_read_fd]
            command += ["-serial", "stdio", "-qmp", f"pipe:{qmp_path}"]
        else:
            command += ["-serial", f"unix:{serial_path},server=on,wait=off",
                        "-qmp", f"unix:{qmp_path},server=on,wait=off"]
        self.stderr = (OUT / f"smoke-qemu-{self.name}.stderr").open("wb")
        self.process = subprocess.Popen(command, cwd=ROOT,
                                        stdin=subprocess.PIPE if use_pipes else subprocess.DEVNULL,
                                        stdout=subprocess.PIPE if use_pipes else subprocess.DEVNULL,
                                        stderr=self.stderr)
        if use_pipes:
            self.serial_read_fd = self.process.stdout.fileno()
            self.serial_write_fd = self.process.stdin.fileno()
            os.set_blocking(self.serial_read_fd, False)
        else:
            self.serial = self.connect(serial_path)
            self.serial.setblocking(False)
            self.serial_read_fd = self.serial_write_fd = self.serial.fileno()
            self.qmp = self.connect(qmp_path)
            self.qmp.setblocking(False)
            self.qmp_read_fd = self.qmp_write_fd = self.qmp.fileno()
        greeting = json.loads(self.qmp_line())
        if "QMP" not in greeting:
            raise AssertionError("Missing QMP greeting")
        self.monitor("qmp_capabilities")
        self.wait_for(b"READY\r\n", timeout=6.0)
        self.wait_for(PROMPT, timeout=2.0)
        return self

    def drain(self, timeout=0.0):
        self.ensure_alive()
        ready, _, _ = select.select([self.serial_read_fd], [], [], timeout)
        if ready:
            while True:
                try:
                    chunk = os.read(self.serial_read_fd, 4096)
                except BlockingIOError:
                    break
                if not chunk:
                    raise AssertionError("QEMU serial socket closed unexpectedly")
                self.output.extend(chunk)

    def wait_for(self, expected, start=0, timeout=3.0):
        stop = time.monotonic() + self.remaining(timeout)
        while expected not in self.output[start:]:
            self.ensure_alive()
            if time.monotonic() >= stop:
                tail = bytes(self.output[-700:]).decode("ascii", errors="backslashreplace")
                raise AssertionError(f"Timed out waiting for {expected!r}; serial tail:\n{tail}")
            self.drain(max(0.0, min(0.02, stop - time.monotonic())))
        return bytes(self.output[start:])

    def send(self, data):
        # Even the overlong input test sends one byte at a time. Flooding a
        # hardware UART tests FIFO overflow rather than the kernel's line limit.
        for byte in data:
            self.remaining()
            os.write(self.serial_write_fd, bytes((byte,)))
            time.sleep(0.002)
            self.drain()

    def command(self, data):
        start = len(self.output)
        self.send(data)
        return self.wait_for(PROMPT, start=start)

    def monitor(self, command, arguments=None):
        self.qmp_id += 1
        identifier = self.qmp_id
        request = {"execute": command, "id": identifier}
        if arguments is not None:
            request["arguments"] = arguments
        data = json.dumps(request).encode("ascii") + b"\n"
        while data:
            data = data[os.write(self.qmp_write_fd, data):]
        while True:
            self.remaining()
            raw = self.qmp_line()
            if not raw:
                raise AssertionError("QMP connection closed")
            reply = json.loads(raw)
            if reply.get("id") != identifier:
                continue  # Ignore asynchronous RESET and other events.
            if "error" in reply:
                raise AssertionError(f"QMP {command}: {reply['error']}")
            return reply.get("return")

    def qmp_line(self):
        stop = time.monotonic() + self.remaining(3.0)
        while b"\n" not in self.qmp_buffer:
            self.ensure_alive()
            if time.monotonic() >= stop:
                raise AssertionError("Timed out waiting for QMP response")
            ready, _, _ = select.select([self.qmp_read_fd], [], [],
                                        max(0.0, min(0.02, stop - time.monotonic())))
            if ready:
                chunk = os.read(self.qmp_read_fd, 4096)
                if not chunk:
                    raise AssertionError("QMP connection closed")
                self.qmp_buffer.extend(chunk)
        line, _, remainder = self.qmp_buffer.partition(b"\n")
        self.qmp_buffer = bytearray(remainder)
        return line

    def keyboard(self, keys):
        start = len(self.output)
        for key in keys:
            self.monitor("human-monitor-command", {"command-line": f"sendkey {key} 20"})
            time.sleep(0.04)  # Let each press and its release reach the 8042.
            self.drain()
        return self.wait_for(PROMPT, start=start)

    def close(self):
        if self.process is not None and self.process.poll() is None:
            self.process.terminate()
            try:
                self.process.wait(timeout=2)
            except subprocess.TimeoutExpired:
                self.process.kill()
                self.process.wait(timeout=2)
        if self.qmp is not None:
            self.qmp.close()
        if self.serial is not None:
            self.serial.close()
        if self.process is not None:
            if self.process.stdin is not None:
                self.process.stdin.close()
            if self.process.stdout is not None:
                self.process.stdout.close()
        for descriptor in self.pipe_fds:
            os.close(descriptor)
        self.pipe_fds = []
        if self.stderr is not None:
            self.stderr.close()
        self.temporary.cleanup()


def contains(result, expected):
    if expected not in result:
        raise AssertionError(f"Missing {expected!r} in command response {result!r}")


def main():
    OUT.mkdir(exist_ok=True)
    if not (OUT / "tane-os.img").is_file():
        sys.exit("Missing build/tane-os.img. Run python3 build.py first.")
    beginning = time.monotonic()
    deadline = beginning + TIME_LIMIT
    checks = []
    machines = []
    failure = None

    def checked(name):
        checks.append(name)
        print(f"PASS {name}", flush=True)

    try:
        machine = Machine("main", deadline)
        machines.append(machine)
        machine.start()
        checked("BIOS floppy boots into the Rust shell")

        contains(machine.command(b"help\r"), b"calc A OP B")
        contains(machine.command(b"about\r"), b"No process isolation, filesystem, network, or exception handlers.")
        result = machine.command(b"mem\r")
        contains(result, b"First 1 GiB identity mapped; this is NOT detected RAM size.")
        contains(result, b"Command buffer: 128 bytes (max 127 input).")
        checked("help/about/mem describe the running kernel")

        contains(machine.command(b"echo Rust works\r"), b"\r\nRust works\r\n")
        contains(machine.command(b"echo  a   b\r"), b"\r\n a   b\r\n")
        checked("serial input and echo preserve text")

        for command, expected in [
            (b"calc 12 * 3\r", b"\r\n= 36\r\n"),
            (b"calc -7 / 3\r", b"\r\n= -2\r\n"),
            (b"calc 9 / 0\r", b"error: division by zero"),
            (b"calc 9223372036854775807 + 1\r", b"error: i64 overflow"),
            (b"calc -9223372036854775808 + 0\r", b"\r\n= -9223372036854775808\r\n"),
            (b"calc -9223372036854775808 / -1\r", b"error: i64 overflow"),
        ]:
            contains(machine.command(command), expected)
        checked("arithmetic handles signed limits, overflow and zero division")

        contains(machine.command(b"nonsense\r"), b"Unknown command. Type help.")
        result = machine.command(b"\r")
        if result != b"\r\n" + PROMPT:
            raise AssertionError(f"Empty command emitted unexpected text: {result!r}")
        for backspace in (b"\x7f", b"\x08"):
            contains(machine.command(b"echo baX" + backspace + b"ck\r"), b"\r\nback\r\n")
        result = machine.command(b"echo crlf\r\n")
        contains(result, b"\r\ncrlf\r\n")
        if result.count(PROMPT) != 1:
            raise AssertionError("CRLF executed more than one command")
        checked("unknown/empty commands, both backspaces and CRLF work")

        result = machine.command(b"echo " + b"X" * 160 + b"\r")
        contains(result, b"\r\n" + b"X" * 122 + b"\r\n")
        if b"X" * 123 in result:
            raise AssertionError("Input was accepted beyond the 127 byte line limit")
        contains(machine.command(b"echo recovered\r"), b"\r\nrecovered\r\n")
        checked("overlong input is bounded and the next command recovers")

        for index in range(30):
            text = f"scroll-{index:02d}".encode("ascii")
            contains(machine.command(b"echo " + text + b"\r"), b"\r\n" + text + b"\r\n")
        checked("VGA scrolling keeps the shell responsive")

        contains(machine.keyboard(["e", "c", "h", "o", "spc", "k", "b", "d", "ret"]), b"\r\nkbd\r\n")
        contains(machine.keyboard(["e", "c", "h", "o", "spc", "shift-a", "shift-1", "ret"]), b"\r\nA!\r\n")
        checked("PS/2 keyboard path works, including Shift letters and symbols")

        contains(machine.command(b"clear\r"), b"\x1b[2J\x1b[H")
        machine.command(b"about\r")
        machine.command(b"help\r")
        machine.command(b"echo Rust kernel / BIOS + serial + PS2 verified\r")
        machine.command(b"calc 12 * 3\r")
        screenshot = OUT / "tane-os.ppm"
        machine.monitor("screendump", {"filename": str(screenshot)})
        if not screenshot.is_file() or screenshot.stat().st_size < 1000:
            raise AssertionError("QEMU did not capture the VGA screenshot")
        checked("clear works and VGA screenshot is captured")

        start = len(machine.output)
        machine.send(b"halt\r")
        machine.wait_for(b"HALTED\r\n", start=start)
        machine.drain(0.1)
        machine.ensure_alive()
        if PROMPT in machine.output[start:]:
            raise AssertionError("Halt returned to the command loop")
        if not machine.monitor("query-status").get("running"):
            raise AssertionError("Halt unexpectedly stopped the virtual machine")
        checked("halt stops the guest CPU while QEMU stays alive")
        machine.close()

        reboot = Machine("reboot", deadline)
        machines.append(reboot)
        reboot.start()
        start = len(reboot.output)
        reboot.send(b"reboot\r")
        reboot.wait_for(b"REBOOTING\r\n", start=start)
        reboot.wait_for(b"READY\r\n", start=start, timeout=6.0)
        reboot.wait_for(PROMPT, start=start)
        if reboot.output.count(b"READY\r\n") != 2:
            raise AssertionError("Reboot did not produce exactly a second boot banner")
        contains(reboot.command(b"echo restarted\r"), b"\r\nrestarted\r\n")
        checked("8042 reboot boots again and accepts a fresh command")
    except Exception as error:
        failure = str(error)
    finally:
        for machine in machines:
            machine.close()
        transcript = bytearray()
        for machine in machines:
            transcript += f"\n--- QEMU {machine.name} ---\n".encode("ascii")
            transcript += machine.output
        (OUT / "smoke-serial.txt").write_bytes(transcript)
        elapsed = time.monotonic() - beginning
        report = ["Tane OS QEMU integration test", f"Elapsed: {elapsed:.2f}s"]
        report += [f"Transport ({machine.name}): {machine.transport}" for machine in machines]
        report += [f"PASS: {name}" for name in checks]
        report.append(f"FAIL: {failure}" if failure else "RESULT: PASS")
        (OUT / "smoke-summary.txt").write_text("\n".join(report) + "\n", encoding="utf-8")
    if failure:
        print(f"FAIL {failure}", file=sys.stderr)
        return 1
    print(f"All {len(checks)} integration checks passed in {elapsed:.2f}s.")
    return 0


if __name__ == "__main__":
    sys.exit(main())
