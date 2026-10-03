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
import re
import select
import socket
import subprocess
import sys
import tempfile
import time

ROOT = Path(__file__).resolve().parents[1]
OUT = ROOT / "build"
PROMPT = b"tane> "
USER_PROMPT = b"tane$ "
FRAME = 4096
LOW_MEMORY_END = 1 << 20
FRAME_LIMIT = 1 << 30
TIME_LIMIT = 60.0


class Machine:
    def __init__(self, name, deadline, serial=True, memory="64M", disk=None):
        self.name = name
        self.with_serial = serial
        self.memory = memory
        self.prompt = PROMPT  # USER_PROMPT after `drop`
        self.disk = disk
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
            raise AssertionError(f"Integration test exceeded its {TIME_LIMIT:.0f} second deadline")
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
            "-machine", "pc,accel=tcg", "-m", self.memory,
            "-drive", f"file={OUT / 'tane-os.img'},format=raw,if=floppy,readonly=on",
            "-boot", "order=a", "-display", "none", "-net", "none",
        ]
        if self.disk is not None:
            command += ["-drive", f"file={self.disk},format=raw,if=ide"]
        if use_pipes:
            self.transport = "local pipes (Unix sockets unavailable or disabled)"
            for suffix in (".in", ".out"):
                os.mkfifo(str(qmp_path) + suffix, 0o600)
            self.qmp_write_fd = os.open(str(qmp_path) + ".in", os.O_RDWR | os.O_NONBLOCK)
            self.qmp_read_fd = os.open(str(qmp_path) + ".out", os.O_RDWR | os.O_NONBLOCK)
            self.pipe_fds = [self.qmp_write_fd, self.qmp_read_fd]
            command += ["-serial", "stdio" if self.with_serial else "none", "-qmp", f"pipe:{qmp_path}"]
        else:
            command += ["-serial", f"unix:{serial_path},server=on,wait=off" if self.with_serial else "none",
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
            if self.with_serial:
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
        if not self.with_serial:
            self.wait_for_screen(b"READY", timeout=6.0)
            return self
        self.wait_for(b"READY\r\n", timeout=6.0)
        self.wait_for(PROMPT, timeout=2.0)
        return self

    def screen(self):
        """Read the 80x25 VGA text buffer from guest memory as text lines."""
        dump = Path(self.temporary.name) / "vga.bin"
        self.monitor("pmemsave", {"val": 0xb8000, "size": 80 * 25 * 2, "filename": str(dump)})
        cells = dump.read_bytes()[::2]
        return [cells[row * 80:(row + 1) * 80].rstrip() for row in range(25)]

    def wait_for_screen(self, expected, timeout=3.0):
        stop = time.monotonic() + self.remaining(timeout)
        while True:
            lines = self.screen()
            if expected in lines:
                return lines
            if time.monotonic() >= stop:
                text = b"\n".join(lines).decode("ascii", errors="backslashreplace")
                raise AssertionError(f"Timed out waiting for {expected!r} on VGA:\n{text}")
            time.sleep(0.05)

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
        return self.wait_for(self.prompt, start=start)

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
        return self.wait_for(self.prompt, start=start)

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


def fatal_fault(kind, deadline, machines, expected):
    machine = Machine(f"fault-{kind}", deadline)
    machines.append(machine)
    machine.start()
    start = len(machine.output)
    machine.send(f"fault {kind}\r".encode("ascii"))
    result = machine.wait_for(b"HALTED\r\n", start=start)
    for text in expected:
        contains(result, text)
    for register in (b"task: pid 1 (shell)", b"rip=", b"rsp=", b"rax=", b"r15="):
        contains(result, register)
    machine.drain(0.1)
    if PROMPT in machine.output[start:]:
        raise AssertionError(f"fault {kind} returned to the command loop")
    machine.close()


def checked_frames(mem_output):
    """Recompute the usable frame count from the printed E820 map."""
    usable = set()
    reserved = []
    for base, end, kind in re.findall(rb"^  0x([0-9a-f]+)\.\.0x([0-9a-f]+) +\d+ KiB (.+?)\r$", mem_output, re.M):
        base, end = int(base, 16), int(end, 16)
        if kind.endswith(b"(ignored)"):
            continue
        if kind == b"usable":
            start = -(-max(base, LOW_MEMORY_END) // FRAME)
            usable.update(range(start, min(end, FRAME_LIMIT) // FRAME))
        else:
            reserved.append(range(min(base, FRAME_LIMIT) // FRAME, -(-min(end, FRAME_LIMIT) // FRAME)))
    for frames in reserved:
        usable.difference_update(frames)
    match = re.search(rb"Frames: (\d+) usable, (\d+) free, (\d+) in use", mem_output)
    if not match:
        raise AssertionError(f"Missing frame summary in {mem_output!r}")
    if not usable or int(match.group(1)) != len(usable):
        raise AssertionError(f"Kernel reports {match.group(1)!r} usable frames; E820 map gives {len(usable)}")
    return [int(value) for value in match.groups()]


def frames_free(result):
    match = re.search(rb"(\d+) frames free", result)
    if not match:
        raise AssertionError(f"Missing free frame count in {result!r}")
    return int(match.group(1))


def tasks(machine):
    """Parse `ps` into {pid: (name, state, cpu_hundredths, counter, domain)}."""
    table = {}
    for pid, name, domain, state, seconds, hundredths, counter in re.findall(
            rb"^ *(\d+) (\w+) +(\w+) +(\w+) +(\d+)\.(\d\d) +(\d+|-)  0x[0-9a-f]+ \(", machine.command(b"ps\r"), re.M):
        table[int(pid)] = (name, state, int(seconds) * 100 + int(hundredths),
                           None if counter == b"-" else int(counter), domain)
    if table.get(1, (None,))[:2] != (b"shell", b"running") or table.get(0, (None,))[0] != b"idle":
        raise AssertionError(f"Unexpected task table {table!r}")
    if table[0][4] != b"kernel":
        raise AssertionError(f"idle must be labelled kernel: {table[0]!r}")
    return table


def spawn(machine, kind):
    result = machine.command(f"spawn {kind}\r".encode("ascii"))
    match = re.search(rb"started " + kind.encode("ascii") + rb" as pid (\d+)", result)
    if not match:
        raise AssertionError(f"spawn {kind} failed: {result!r}")
    return int(match.group(1))


def new_disk(directory, name):
    path = Path(directory) / name
    with path.open("wb") as disk:
        disk.truncate(1 << 20)
    return path


def cpu_percent(top_output, domain):
    match = re.search(rb"^" + domain + rb" .* (\d+)% \( *\d+ ticks\) ", top_output, re.M)
    if not match:
        raise AssertionError(f"No CPU figure for {domain!r} in {top_output!r}")
    return int(match.group(1))


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
    disks = tempfile.TemporaryDirectory(prefix="tane-disks-")

    def checked(name):
        checks.append(name)
        print(f"PASS {name}", flush=True)

    try:
        machine = Machine("main", deadline, disk=new_disk(disks.name, "main.img"))
        machines.append(machine)
        machine.start()
        checked("BIOS floppy boots into the Rust shell")

        result = machine.output
        contains(result, b"IDT: 32 exception handlers, #DF on IST1 | PIT timer 100 Hz")
        contains(result, b"Protection: NX data | kernel text read-only | null page unmapped | CR0.WP")
        contains(result, b"MAC: enforcing compiled-in policy | shell domain admin (drop lowers it)")
        if b"COM1 not detected" in result:
            raise AssertionError("COM1 was not detected although QEMU provides it")
        contains(machine.command(b"help\r"), b"calc I64 (+|-|*|/) I64")
        about = machine.command(b"about\r")
        contains(about, b"CPU exceptions print registers; all tasks share ring 0 and one address space.")
        contains(about, b"RTL8139, ARP/IPv4/ICMP, NDP/IPv6/ICMPv6; static network configuration.")
        result = machine.command(b"mem\r")
        contains(result, b"First 1 GiB identity mapped: 4 KiB pages below 2 MiB, then 2 MiB pages.")
        contains(result, b"Command buffer: 255 bytes (max 255 input).")
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

        contains(machine.command(b"nonsense\r"), b"error: unknown operation; type help")
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

        def uptime_ticks():
            result = machine.command(b"uptime\r")
            contains(result, b" timer ticks at 100 Hz)")
            return int(result.split(b"(")[1].split(b" ")[0])

        first = uptime_ticks()
        began = time.monotonic()
        contains(machine.command(b"sleep 500\r"), b"\r\nslept 500 ms\r\n")
        waited = time.monotonic() - began
        second = uptime_ticks()
        if waited < 0.45:
            raise AssertionError(f"sleep 500 returned after only {waited:.3f}s")
        if second - first < 50:
            raise AssertionError(f"Timer advanced {second - first} ticks across sleep 500")
        contains(machine.command(b"sleep 60001\r"), b"error: usage: sleep MS (0-60000)")
        checked("PIT timer interrupts drive uptime and sleep")

        result = machine.command(b"fault bp\r")
        for text in (b"CPU EXCEPTION 3 #BP: Breakpoint", b"Breakpoint handled; resuming.",
                     b"returned to the shell after the exception"):
            contains(result, text)
        contains(machine.command(b"echo after breakpoint\r"), b"\r\nafter breakpoint\r\n")
        contains(machine.command(b"fault nmi\r"), b"error: usage: fault bp|de|ud|gp|pf|df")
        checked("#BP is reported and execution resumes")

        contains(machine.output, b"Disk: ATA found; disk is not formatted (admin: format)")
        contains(machine.command(b"ls\r"), b"error: disk is not formatted (admin: format)")
        contains(machine.command(b"format\r"), b"formatted: empty TaneFS, 32 files of up to 4096 bytes")
        contains(machine.command(b"disk\r"), b"TaneFS mounted, 0 of 32 files used")
        contains(machine.command(b"write notes.txt hello  disk\r"), b"created notes.txt (11 bytes, label admin)")
        contains(machine.command(b"append notes.txt , again\r"), b"appended to notes.txt (7 bytes)")
        contains(machine.command(b"cat notes.txt\r"), b"\r\nhello  disk, again\r\n")
        contains(machine.command(b"write notes.txt replaced\r"), b"wrote notes.txt (8 bytes)")
        result = machine.command(b"ls\r")
        contains(result, b"   0 admin       8    3 notes.txt\r\n")
        contains(result, b"1 file(s) readable by domain admin")
        contains(machine.command(b"cat missing\r"), b"error: no such file")
        contains(machine.command(b"write a/b x\r"), b"error: invalid TaneFS name")
        contains(machine.command(b"rm notes.txt\r"), b"removed notes.txt")
        contains(machine.command(b"ls\r"), b"0 file(s) readable by domain admin")
        checked("ATA disk: format, create, append, overwrite, cat, ls, rm")

        usable, free, used = checked_frames(machine.command(b"mem\r"))
        if usable < 15000 or free != usable or used != 0:
            raise AssertionError(f"Unexpected frame summary for 64 MiB: {usable} {free} {used}")
        contains(machine.output, f"| {usable} free 4 KiB frames".encode("ascii"))
        first, second = machine.command(b"alloc\r"), machine.command(b"alloc\r")
        a, b = (int(re.search(rb"allocated 0x([0-9a-f]+) \(zeroed\)", r).group(1), 16) for r in (first, second))
        if a == b or a % FRAME or b % FRAME or min(a, b) < LOW_MEMORY_END or frames_free(second) != usable - 2:
            raise AssertionError(f"Bad allocations 0x{a:x}, 0x{b:x}: {second!r}")
        contains(machine.command(f"free 0x{a:x}\r".encode("ascii")), f"freed 0x{a:x}; {usable - 1} frames free".encode("ascii"))
        contains(machine.command(f"free 0x{a:x}\r".encode("ascii")), b"frame is already free")
        contains(machine.command(b"free 0x1000\r"), b"error: 0x1000: not a managed RAM frame")
        contains(machine.command(f"free {b + 1}\r".encode("ascii")), b"address is not 4 KiB aligned")
        contains(machine.command(b"free 0x\r"), b"error: invalid hexadecimal address")
        contains(machine.command(f"free {b}\r".encode("ascii")), f"; {usable} frames free".encode("ascii"))
        checked("E820 RAM becomes 4 KiB frames; alloc/free validate addresses")

        spins = [spawn(machine, "spin"), spawn(machine, "spin")]
        beat = spawn(machine, "beat")
        before = tasks(machine)
        contains(machine.keyboard(["e", "c", "h", "o", "spc", "b", "u", "s", "y", "ret"]), b"\r\nbusy\r\n")
        began = time.monotonic()
        contains(machine.command(b"echo responsive\r"), b"\r\nresponsive\r\n")
        if time.monotonic() - began > 1.5:
            raise AssertionError("Shell input stalled while CPU-bound tasks ran")
        machine.command(b"sleep 500\r")
        after = tasks(machine)
        for pid in spins:
            if after[pid][2] <= before[pid][2] or after[pid][3] <= before[pid][3]:
                raise AssertionError(f"spin pid {pid} did not keep running: {before[pid]} -> {after[pid]}")
        # The sample can land just after the timer wakes beat, before its next
        # dispatch. Both ready and sleeping are valid for this periodic task.
        if after[beat][3] - before[beat][3] < 3 or after[beat][1] not in (b"ready", b"sleeping") or after[beat][2] > 5:
            raise AssertionError(f"beat should sleep between beats: {before[beat]} -> {after[beat]}")
        checked("timer preempts CPU-bound tasks; shell and sleepers stay responsive")

        stack = int(re.search(rb"^ *" + str(spins[0]).encode("ascii") + rb" spin .*  0x([0-9a-f]+) \(4 frames\)",
                              machine.command(b"ps\r"), re.M).group(1), 16)
        contains(machine.command(f"free 0x{stack + 2 * FRAME:x}\r".encode("ascii")),
                 f"is in the stack of pid {spins[0]}; use kill".encode("ascii"))
        busy = checked_frames(machine.command(b"mem\r"))[1]
        if busy != usable - 12:
            raise AssertionError(f"Three 16 KiB task stacks should use 12 frames; {usable - busy} in use")
        once = spawn(machine, "once")
        # It may not have run yet while the two spin tasks share the CPU.
        if tasks(machine)[once][:2] not in ((b"once", b"ready"), (b"once", b"sleeping")):
            raise AssertionError("once task was not created")
        machine.command(b"sleep 600\r")
        if once in tasks(machine):
            raise AssertionError("Exited task was not reaped")
        if checked_frames(machine.command(b"mem\r"))[1] != busy:
            raise AssertionError("Exited task did not return its stack frames")
        contains(machine.command(b"kill 1\r"), b"error: a task cannot kill itself")
        contains(machine.command(b"kill 0\r"), b"denied: admin may not kill objects of domain kernel (MAC policy; audited)")
        contains(machine.command(b"kill 9999\r"), b"error: no such task")
        contains(machine.command(b"spawn idle\r"), b"error: usage: spawn spin|beat|once")
        extra = []
        while True:
            result = machine.command(b"spawn beat\r")
            if b"error: task table is full (8 tasks)" in result:
                break
            extra.append(int(re.search(rb"as pid (\d+)", result).group(1)))
            if len(extra) > 8:
                raise AssertionError("Task table never filled")
        if len(tasks(machine)) != 8:
            raise AssertionError("Task table should hold exactly 8 tasks")
        for pid in spins + [beat] + extra:
            contains(machine.command(f"kill {pid}\r".encode("ascii")), f"killed pid {pid} (".encode("ascii"))
        if set(tasks(machine)) != {0, 1} or checked_frames(machine.command(b"mem\r"))[1:] != [usable, 0]:
            raise AssertionError("Killing every task did not restore the frame pool")
        checked("exited and killed tasks return stacks; table limit and kill errors")

        result = machine.command(b"echo " + b"X" * 300 + b"\r")
        contains(result, b"error: input line too long")
        if b"\r\n" + b"X" * 250 + b"\r\n" in result:
            raise AssertionError("An overflowing input executed its truncated prefix")
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
        contains(machine.command(b"sec\r"), b"Paging:  NX on | text 0x10000..")
        contains(machine.command(b"top\r"), b"Shell and idle are not charged.")
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

        reboot_disk = new_disk(disks.name, "reboot.img")
        reboot = Machine("reboot", deadline, memory="128M", disk=reboot_disk)
        machines.append(reboot)
        reboot.start()
        reboot.command(b"format\r")
        contains(reboot.command(b"write keep.txt survives reboot\r"), b"created keep.txt")
        start = len(reboot.output)
        reboot.send(b"reboot\r")
        reboot.wait_for(b"REBOOTING\r\n", start=start)
        reboot.wait_for(b"READY\r\n", start=start, timeout=6.0)
        reboot.wait_for(PROMPT, start=start)
        if reboot.output.count(b"READY\r\n") != 2:
            raise AssertionError("Reboot did not produce exactly a second boot banner")
        contains(reboot.command(b"echo restarted\r"), b"\r\nrestarted\r\n")
        checked("8042 reboot boots again and accepts a fresh command")
        contains(reboot.output[start:], b"Disk: TaneFS mounted, 1 files")
        contains(reboot.command(b"cat keep.txt\r"), b"\r\nsurvives reboot\r\n")
        keep_slot = int(re.search(rb"^ +(\d+) admin .* keep\.txt\r$", reboot.command(b"ls\r"), re.M).group(1))
        checked("files and their labels persist across a reboot")
        larger = checked_frames(reboot.command(b"mem\r"))[0]
        if larger - usable < 16000:
            raise AssertionError(f"128 MiB gives {larger} frames, 64 MiB gave {usable}")
        checked("E820 detects the RAM size (64 MiB vs 128 MiB)")

        admin_frame = int(re.search(rb"allocated 0x([0-9a-f]+)", reboot.command(b"alloc\r")).group(1), 16)
        admin_spin = spawn(reboot, "spin")
        contains(reboot.command(b"audit\r"), b"MAC denials: 0 since boot")
        contains(reboot.command(b"kill 0\r"), b"denied: admin may not kill objects of domain kernel")
        contains(reboot.command(b"audit\r"), b"pid 1 admin kill kernel object (pid 0) DENIED")
        reboot.prompt = USER_PROMPT
        contains(reboot.command(b"drop\r"), b"domain admin -> user; this cannot be undone until reboot")
        for command in (b"halt", b"reboot", b"fault bp", b"audit"):
            contains(reboot.command(command + b"\r"),
                     b"denied: user may not " + command.split()[0] + b" (MAC policy; audited)")
        contains(reboot.command(b"echo still running\r"), b"\r\nstill running\r\n")
        contains(reboot.command(f"kill {admin_spin}\r".encode("ascii")),
                 b"denied: user may not kill objects of domain admin")
        contains(reboot.command(f"free 0x{admin_frame:x}\r".encode("ascii")),
                 b"denied: user may not free objects of domain admin")
        user_frame = int(re.search(rb"allocated 0x([0-9a-f]+)", reboot.command(b"alloc\r")).group(1), 16)
        contains(reboot.command(f"free 0x{user_frame:x}\r".encode("ascii")), f"freed 0x{user_frame:x}".encode("ascii"))
        contains(reboot.command(b"free 0x1000\r"), b"error: 0x1000: not a managed RAM frame")
        result = reboot.command(b"spawn beat\r")
        contains(result, b" in domain user")
        user_beat = int(re.search(rb"as pid (\d+)", result).group(1))
        table = tasks(reboot)
        if table[1][4] != b"user" or table[admin_spin][4] != b"admin" or table[user_beat][4] != b"user":
            raise AssertionError(f"Wrong MAC labels in {table!r}")
        contains(reboot.command(f"kill {user_beat}\r".encode("ascii")), f"killed pid {user_beat} (beat)".encode("ascii"))

        # Files: user sees and touches only user-labelled files.
        contains(reboot.command(b"ls\r"), b"0 file(s) readable by domain user")
        contains(reboot.command(b"cat keep.txt\r"), b"denied: user may not read objects of domain admin")
        contains(reboot.command(b"rm keep.txt\r"), b"denied: user may not delete objects of domain admin")
        contains(reboot.command(b"write keep.txt overwrite\r"), b"denied: user may not write objects of domain admin")
        for index in range(8):
            contains(reboot.command(f"write u{index} data\r".encode("ascii")), f"created u{index} (4 bytes, label user)".encode("ascii"))
        contains(reboot.command(b"write u8 data\r"), b"denied: user files limit reached (8/8) for create (quota; audited)")
        contains(reboot.command(b"cat u3\r"), b"\r\ndata\r\n")
        contains(reboot.command(b"rm u3\r"), b"removed u3")
        contains(reboot.command(b"ls\r"), b"7 file(s) readable by domain user")

        # Tasks and frames: per-domain limits.
        user_spin = spawn(reboot, "spin")
        spawn(reboot, "beat")
        spawn(reboot, "beat")
        contains(reboot.command(b"spawn beat\r"), b"denied: user tasks limit reached (3/3) for spawn (quota; audited)")
        allocated = 0
        while True:
            result = reboot.command(b"alloc\r")
            if b"denied: user frames limit reached (24/24) for alloc (quota; audited)" in result:
                break
            contains(result, b"allocated 0x")
            allocated += 1
            if allocated > 24:
                raise AssertionError("User frame limit not enforced")
        if allocated != 24 - 3 * 4:
            raise AssertionError(f"User got {allocated} frames beside three 4-frame stacks; limit is 24")

        # CPU: admin and user spin tasks compete; the caps split each window.
        reboot.command(b"sleep 2100\r")
        result = reboot.command(b"top\r")
        admin_cpu, user_cpu = cpu_percent(result, b"admin"), cpu_percent(result, b"user")
        if not (60 <= admin_cpu <= 75 and 25 <= user_cpu <= 35):
            raise AssertionError(f"CPU split admin {admin_cpu}% / user {user_cpu}%, expected about 70/30: {result!r}")
        contains(reboot.command(f"kill {user_spin}\r".encode("ascii")), b"killed pid")
        contains(reboot.command(b"drop\r"), b"already in domain user; no command raises a domain")
        contains(reboot.command(b"su\r"), b"error: unknown operation; type help")
        result = reboot.command(b"sec\r")
        contains(result, b"Subject: pid 1 (shell) in domain user")
        contains(result, b"Audit:   13 denials since boot (policy and quota).")
        checked("MAC: labels on tasks, frames and files; one-way drop; denials audited")
        checked("quotas: user tasks, frames and files limited; CPU split 70/30 under contention")
        reboot.close()

        # Flip one byte of keep.txt on the host; the next boot must notice.
        with reboot_disk.open("r+b") as disk:
            disk.seek((8 + keep_slot * 8) * 512 + 2)
            byte = disk.read(1)
            disk.seek(-1, 1)
            disk.write(bytes([byte[0] ^ 0x20]))
        damaged = Machine("integrity", deadline, disk=reboot_disk)
        machines.append(damaged)
        damaged.start()
        contains(damaged.output, b"Disk: TaneFS mounted, 8 files")
        contains(damaged.command(b"cat u0\r"), b"\r\ndata\r\n")
        contains(damaged.command(b"cat keep.txt\r"), b"error: checksum mismatch: file contents are damaged")
        damaged.close()
        checked("disk survives a QEMU restart; damaged contents are detected")

        fatal_fault("de", deadline, machines, [b"CPU EXCEPTION 0 #DE: Divide error"])
        fatal_fault("ud", deadline, machines, [b"CPU EXCEPTION 6 #UD: Invalid opcode"])
        fatal_fault("gp", deadline, machines, [b"CPU EXCEPTION 13 #GP: General protection fault", b"error=0x0\r\n",
                                               b"rax=8000000000000000"])
        fatal_fault("pf", deadline, machines, [b"CPU EXCEPTION 14 #PF: Page fault",
                                               b"error=0x0 (not-present read) cr2=0x0000000040000000"])
        checked("#DE/#UD/#GP/#PF print diagnostics and halt")
        # A #DF handler on the faulting stack would triple fault and reboot.
        fatal_fault("df", deadline, machines, [b"CPU EXCEPTION 8 #DF: Double fault", b"rsp=0000000040001000"])
        checked("#DF runs on its IST stack after an unmapped RSP")
        fatal_fault("null", deadline, machines, [b"error=0x0 (not-present read) cr2=0x0000000000000000"])
        fatal_fault("ro", deadline, machines, [b"error=0x3 (protection write) cr2=0x0000000000010000"])
        fatal_fault("nx", deadline, machines, [b"error=0x11 (protection read fetch) cr2="])
        checked("paging: null page unmapped, kernel text read-only, data not executable")

        vga = Machine("vga-only", deadline, serial=False)
        machines.append(vga)
        vga.start()
        if b"COM1 not detected; VGA and keyboard only." not in vga.screen():
            raise AssertionError("Missing COM1 absence notice on VGA")
        for key in ["e", "c", "h", "o", "spc", "n", "o", "spc", "c", "o", "m", "1", "ret"]:
            vga.monitor("human-monitor-command", {"command-line": f"sendkey {key} 20"})
            time.sleep(0.04)
        lines = vga.wait_for_screen(b"no com1")
        if lines[lines.index(b"no com1") + 1] != b"tane>":
            raise AssertionError("No prompt after keyboard command without COM1")
        checked("keyboard works when the machine has no COM1 (run.sh default)")
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
        disks.cleanup()
    if failure:
        print(f"FAIL {failure}", file=sys.stderr)
        return 1
    print(f"All {len(checks)} integration checks passed in {elapsed:.2f}s.")
    return 0


if __name__ == "__main__":
    sys.exit(main())
