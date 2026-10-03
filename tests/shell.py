#!/usr/bin/env python3
"""Exercise Tane Shell v1 through real QEMU serial input and ATA storage.

Uses the standard-library Machine transport from smoke.py. No host command,
network service, or replacement kernel implementation is used by these tests.
QEMU/QEMU_DATADIR/LD_LIBRARY_PATH have the same meaning as in smoke.py.
"""
import json
import os
from pathlib import Path
import re
import shutil
import socket
import subprocess
import sys
import tempfile
import time

from smoke import Machine, OUT, PROMPT, USER_PROMPT, new_disk
from network import Machine as NetworkMachine, Peer

TIME_LIMIT = 120.0
SCRIPT_DEPTH_LIMIT = 4


class DiskNetworkMachine(NetworkMachine):
    """Use the existing Ethernet peer transport with one additional IDE disk.

    NetworkMachine owns serial I/O, peer processing and cleanup. This start
    override adds the ATA fixture so a real packet timeout can stop a script.
    """
    def __init__(self, name, deadline, disk):
        super().__init__(name, deadline)
        self.disk = disk

    def start(self):
        image = Path(self.temporary.name) / "tane-os.img"
        shutil.copyfile(OUT / "tane-os.img", image)
        command = [os.environ.get("QEMU", "qemu-system-x86_64")]
        if os.environ.get("QEMU_DATADIR"):
            command += ["-L", os.environ["QEMU_DATADIR"]]
        local, other = socket.socketpair()
        self.peer = Peer(local)
        command += [
            "-machine", "pc,accel=tcg", "-m", "64M", "-display", "none",
            "-drive", f"file={image},format=raw,if=floppy,readonly=on",
            "-drive", f"file={self.disk},format=raw,if=ide",
            "-boot", "order=a", "-serial", "stdio",
            "-netdev", f"socket,id=wire,fd={other.fileno()}",
            "-device", "rtl8139,netdev=wire,mac=52:54:00:12:34:56,romfile=",
        ]
        self.stderr = (OUT / f"network-qemu-{self.name}.stderr").open("wb")
        try:
            self.process = subprocess.Popen(command, cwd=OUT.parent, stdin=subprocess.PIPE,
                                            stdout=subprocess.PIPE, stderr=self.stderr,
                                            pass_fds=(other.fileno(),))
        finally:
            other.close()
        os.set_blocking(self.process.stdout.fileno(), False)
        self.wait_for(b"READY\r\n", timeout=6)
        self.wait_for(self.prompt)
        return self


def enter(machine, text, terminator=b"\r", timeout=4.0):
    """Wait after Enter, so editor redraw prompts cannot masquerade as completion."""
    if isinstance(text, str):
        text = text.encode("ascii")
    machine.send(text)
    machine.drain()
    start = len(machine.output)
    machine.send(terminator)
    result = machine.wait_for(machine.prompt, start=start, timeout=timeout)
    if not result.endswith(machine.prompt):
        machine.drain(0.02)
        result = bytes(machine.output[start:])
    return result


def contains(result, expected):
    if isinstance(expected, str):
        expected = expected.encode("ascii")
    if expected not in result:
        raise AssertionError(f"Missing {expected!r} in execution output {result!r}")


def failed(result):
    if not any(marker in result.lower() for marker in (b"error:", b"denied:", b"unknown command")):
        raise AssertionError(f"Command unexpectedly succeeded: {result!r}")
    return result


def json_value(result):
    # Serialization emits a standalone JSON value. Ignore terminal escape codes
    # and the returned prompt, never parse the echoed command as its result.
    clean = re.sub(rb"\x1b\[[0-9;]*[A-Za-z]", b"", result).decode("ascii")
    for line in clean.splitlines():
        value = line.strip()
        if value.startswith(("[", "{")) or value.isdigit():
            try:
                return json.loads(value)
            except json.JSONDecodeError:
                pass
    raise AssertionError(f"No serialized JSON result in {result!r}")


def rows(machine, source="file list"):
    value = json_value(enter(machine, source + " | json"))
    if not isinstance(value, list) or any(not isinstance(row, dict) for row in value):
        raise AssertionError(f"Expected an array of records, got {value!r}")
    return value


def absent(machine, name):
    if any(row.get("name") == name for row in rows(machine)):
        raise AssertionError(f"Unexpected file {name!r} exists")


def main():
    OUT.mkdir(exist_ok=True)
    if not (OUT / "tane-os.img").is_file():
        sys.exit("Missing build/tane-os.img. Run python3 build.py first.")
    beginning = time.monotonic()
    deadline = beginning + TIME_LIMIT
    checks, machines = [], []
    failure = None
    disks = tempfile.TemporaryDirectory(prefix="tane-shell-disks-")

    def checked(name):
        checks.append(name)
        print(f"PASS {name}", flush=True)

    try:
        disk = new_disk(disks.name, "shell.img")
        machine = Machine("shell", deadline, disk=disk)
        machines.append(machine)
        machine.start()
        enter(machine, "format")
        no_nic = rows(machine, "net status")
        if not no_nic or no_nic[0].get("available") is not False:
            raise AssertionError(f"NIC-less typed status should remain readable: {no_nic!r}")
        checked("Shell v1 boots with a real ATA disk")

        contains(enter(machine, "echo 'hello | world'"), b"\r\nhello | world\r\n")
        contains(enter(machine, r'echo "a\"b\\c"'), b'\r\na"b\\c\r\n')
        contains(enter(machine, "echo '' tail"), b" tail\r\n")
        contains(enter(machine, 'echo "$(file write quoted BAD)"'), "$(file write quoted BAD)")
        absent(machine, "quoted")
        enter(machine, 'let literal "literal | file write injected BAD"')
        contains(enter(machine, "echo $literal"), b"literal | file write injected BAD")
        absent(machine, "injected")
        contains(enter(machine, "vars"), "literal")
        enter(machine, "unset literal")
        failed(enter(machine, "echo $literal"))
        enter(machine, "let operation file")
        failed(enter(machine, "$operation write generated BAD"))
        enter(machine, "let subcommand write")
        failed(enter(machine, "file $subcommand generated BAD"))
        absent(machine, "generated")
        checked("Quotes, escapes and variable expansion preserve data without evaluation")

        tasks = rows(machine, "task list")
        if not any(row.get("pid") == 1 and row.get("name") == "shell" for row in tasks):
            raise AssertionError(f"Shell task missing from structured rows: {tasks!r}")
        selected = json_value(enter(machine, "task list | where domain == admin | select pid name | sort pid --desc | json"))
        if not selected or any(set(row) != {"pid", "name"} for row in selected):
            raise AssertionError(f"Projection did not preserve typed task records: {selected!r}")
        if [row["pid"] for row in selected] != sorted((row["pid"] for row in selected), reverse=True):
            raise AssertionError(f"Task sort is wrong: {selected!r}")
        contains(enter(machine, "task list | where domain == admin | select pid name | sort pid --desc | count"), str(len(selected)))
        checked("Task pipelines filter, project, sort and count structured records")

        enter(machine, "file write f10 0123456789")
        enter(machine, "file write f2 12")
        sorted_files = json_value(enter(machine, "file list | where bytes > 0 | select name bytes | sort bytes | json"))
        if sorted_files != [{"name": "f2", "bytes": 2}, {"name": "f10", "bytes": 10}]:
            raise AssertionError(f"File sort must be numeric, got {sorted_files!r}")
        contains(enter(machine, "file list | where bytes '>=' 10 | count"), b"1")
        contains(enter(machine, "file list | where bytes >= 10 | count"), b"1")
        checked("File metadata pipelines use numeric comparisons and numeric sorting")

        before = rows(machine)
        bad_lines = (
            "echo hi | bogus | save malformed",
            "file write forbidden data | where bytes == 1",
            "file list | where unknown == 1 | save malformed",
            "file list | select unknown | save malformed",
            "file list | sort unknown | save malformed",
            "file list | count | where name == x | save malformed",
            "file list | save malformed | count",
            "file list | | count",
            "file list | count |",
            "file write unterminated 'data",
            "echo hi ; file write semicolon BAD",
            "echo $(file write substitution BAD)",
        )
        for command in bad_lines:
            failed(enter(machine, command))
            if rows(machine) != before:
                raise AssertionError(f"Malformed pipeline mutated the filesystem: {command!r}")
        checked("Whole-pipeline validation rejects malformed stages before any source or sink effect")

        enter(machine, "echo 'text | remains data' | save text.txt")
        contains(enter(machine, "file read text.txt"), b"text | remains data")
        expected_saved = [{"name": row["name"], "bytes": row["bytes"]} for row in rows(machine)]
        enter(machine, "file list | select name bytes | json | save metadata.json")
        saved = json_value(enter(machine, "file read metadata.json"))
        if saved != expected_saved:
            raise AssertionError(f"Serialized save changed record data: {saved!r}")
        checked("Explicit save sinks persist text and serialized records")

        enter(machine, r'file write invalid.tsh "file write invalid-first BAD\nnonsense"')
        failed(enter(machine, "run invalid.tsh"))
        absent(machine, "invalid-first")
        enter(machine, r'file write stop.tsh "file write first yes\nfile read absent.txt\nfile write later NO"')
        failed(enter(machine, "run stop.tsh"))
        contains(enter(machine, "file read first"), "yes")
        absent(machine, "later")
        enter(machine, "file write recurse.tsh 'run recurse.tsh'")
        failed(enter(machine, "run recurse.tsh"))
        contains(enter(machine, "echo after recursion"), "after recursion")
        enter(machine, r'file write binding.tsh "let delay 20\nsleep \$delay\necho bound-\${delay}"')
        contains(enter(machine, "run binding.tsh"), "bound-20")
        checked("TaneFS scripts stop on the first error and reject unbounded recursion")

        leaf = r'file append depth-proof success\nfile list | where label == admin | select name bytes | sort bytes | take 32 | where bytes >= 0 | count | json\nfile read depth-proof'
        enter(machine, f'file write depth{SCRIPT_DEPTH_LIMIT}.tsh "{leaf}"')
        for parent in reversed(range(1, SCRIPT_DEPTH_LIMIT)):
            enter(machine, f"file write depth{parent}.tsh 'run depth{parent + 1}.tsh'")
        file_count = len(rows(machine))
        nested = enter(machine, "run depth1.tsh")
        contains(nested, "success")
        if json_value(nested) != [{"count": file_count + 1}]:
            raise AssertionError(f"Eight-stage pipeline failed at the script depth limit: {nested!r}")
        enter(machine, f"file write depth{SCRIPT_DEPTH_LIMIT + 1}.tsh 'file write depth-forbidden BAD'")
        enter(machine, f"file write depth{SCRIPT_DEPTH_LIMIT}.tsh 'run depth{SCRIPT_DEPTH_LIMIT + 1}.tsh'")
        before_depth_failure = rows(machine)
        rejected = failed(enter(machine, "run depth1.tsh"))
        contains(rejected, f"nesting exceeds {SCRIPT_DEPTH_LIMIT}")
        if rows(machine) != before_depth_failure:
            raise AssertionError("A script beyond the depth limit mutated files before rejection")
        contains(enter(machine, "echo after depth boundary"), "after depth boundary")
        checked(f"{SCRIPT_DEPTH_LIMIT} nested scripts execute a maximal pipeline; the next level fails without effects")

        enter(machine, r'file write cancel.tsh "sleep 5000\nfile write skipped BAD"')
        machine.send(b"run cancel.tsh\r")
        time.sleep(0.1)
        machine.drain()
        start = len(machine.output)
        machine.send(b"\x03")
        canceled = machine.wait_for(machine.prompt, start=start)
        contains(canceled.lower(), b"cancel")
        status = rows(machine, "status")
        if not status or status[0].get("code") != "cancelled":
            raise AssertionError(f"Script cancellation lost its result code: {status!r}")
        absent(machine, "skipped")
        checked("Ctrl-C cancels a sleeping script before its next effect")

        enter(machine, "plan file write planned before")
        contains(enter(machine, "show"), "planned")
        absent(machine, "planned")
        enter(machine, "apply")
        contains(enter(machine, "file read planned"), "before")
        failed(enter(machine, "apply"))
        enter(machine, "let payload 'later | file write rogue BAD'")
        enter(machine, "plan file write literal-plan $payload")
        enter(machine, "let payload changed")
        enter(machine, "apply")
        contains(enter(machine, "file read literal-plan"), "later | file write rogue BAD")
        absent(machine, "rogue")
        enter(machine, "plan file write stale BAD")
        enter(machine, "file write intervening mutation")
        failed(enter(machine, "apply"))
        absent(machine, "stale")
        checked("Plans have no effect before apply; apply is one-shot and rejects stale state")

        contains(enter(machine, "help 'task list'"), "task list")
        contains(enter(machine, "help 'file write'"), "file write")
        operations = rows(machine, "ops")
        if not any("task list" in row.values() for row in operations):
            raise AssertionError(f"Registry omits task list: {operations!r}")
        if not any("file write" in row.values() for row in operations):
            raise AssertionError("Registry omits file write")
        checked("Operation discovery and help describe the same typed registry")

        failed(enter(machine, "file read absent-status.txt"))
        status = rows(machine, "status")
        if not status or status[0].get("code") != "error":
            raise AssertionError(f"Last error is missing from typed status: {status!r}")
        enter(machine, "echo success")
        status = rows(machine, "status")
        if not status or status[0].get("code") != "success":
            raise AssertionError(f"Successful command did not update typed status: {status!r}")
        checked("Structured status reports the preceding command's result")

        editor_cases = (
            (b"echo ac\x1b[Db", b"abc"),
            (b"echo aXb\x1b[D\x1b[D\x1b[3~", b"ab"),
            (b"cho home\x01e\x05!", b"home!"),
            (b"echo wrong\x15echo cleared", b"cleared"),
            (b"echo keep wrong\x17right", b"keep right"),
            (b"ec\tcompleted", b"completed"),
        )
        for typed, expected in editor_cases:
            contains(enter(machine, typed), b"\r\n" + expected + b"\r\n")
        contains(enter(machine, "echo history-marker"), "history-marker")
        contains(enter(machine, b"\x1b[A"), "history-marker")
        contains(enter(machine, b"echo draft\x1b[A\x1b[B"), b"\r\ndraft\r\n")
        enter(machine, b"file write canceled BAD", terminator=b"\x03")
        absent(machine, "canceled")
        checked("Serial cursor editing, delete, history, draft restoration, controls and completion work")

        for key in ("e", "c", "h", "o", "spc", "a", "c", "left", "b"):
            machine.monitor("human-monitor-command", {"command-line": f"sendkey {key} 20"})
            time.sleep(0.04)
            machine.drain()
        start = len(machine.output)
        machine.monitor("human-monitor-command", {"command-line": "sendkey ret 20"})
        contains(machine.wait_for(machine.prompt, start=start), b"\r\nabc\r\n")
        checked("PS/2 extended cursor keys share the same line editor")

        # 254 bytes before insertion, exactly 255 afterward: four VGA rows
        # including the prompt, while the terminal is already at the bottom.
        wrapped = b"wrap-start-" + b"x" * 236 + b"abc"
        machine.send(b"echo wrap-start-" + b"x" * 236 + b"ac\x1b[Db")
        machine.drain(0.05)
        visible = b"".join(row.ljust(80) for row in machine.screen())
        contains(visible, b"echo " + wrapped)
        contains(enter(machine, b""), b"\r\n" + wrapped + b"\r\n")
        checked("Long-line cursor insertion survives VGA wrapping and scrolling")

        grouped_history = {}
        for row in rows(machine, "history"):
            if type(row.get("id")) is not int or type(row.get("part")) is not int or not isinstance(row.get("command"), str):
                raise AssertionError(f"History pieces lost typed identities: {row!r}")
            if len(row["command"]) > 64:
                raise AssertionError(f"History piece exceeds its text budget: {row!r}")
            grouped_history.setdefault(row["id"], []).append((row["part"], row["command"]))
        reconstructed = ["".join(text for _, text in sorted(parts)) for parts in grouped_history.values()]
        if "echo " + wrapped.decode("ascii") not in reconstructed:
            raise AssertionError(f"History truncated the 255-byte command: {reconstructed!r}")
        checked("Typed history splits and reconstructs a complete command at the line limit")

        failed(enter(machine, b"file write overflow " + b"X" * 2100))
        status = rows(machine, "status")
        if not status or status[0].get("code") != "error":
            raise AssertionError(f"Input rejection lost its result code: {status!r}")
        absent(machine, "overflow")
        contains(enter(machine, "echo recovered"), b"\r\nrecovered\r\n")
        checked("Input overflow rejects the entire command and the next line recovers")

        enter(machine, "let private admin-secret")
        enter(machine, "echo history-secret")
        enter(machine, "plan file write pinned BAD")
        machine.prompt = USER_PROMPT
        enter(machine, "drop")
        failed(enter(machine, "apply"))
        variables = rows(machine, "vars")
        if any("private" in row.values() or "admin-secret" in row.values() for row in variables):
            raise AssertionError(f"Variables retained an admin value after drop: {variables!r}")
        history = rows(machine, "history")
        if any("admin-secret" in str(row) or "history-secret" in str(row) for row in history):
            raise AssertionError(f"History leaked an admin payload after drop: {history!r}")
        if rows(machine):
            raise AssertionError("User file list leaked admin-labelled files")
        failed(enter(machine, "file read first"))
        failed(enter(machine, "audit | json"))
        status = rows(machine, "status")
        if not status or status[0].get("code") != "denied":
            raise AssertionError(f"Admin-only source denial missing from typed status: {status!r}")
        failed(enter(machine, "audit | json | save denied-audit"))
        absent(machine, "denied-audit")
        failed(enter(machine, "echo overwrite | save first"))
        failed(enter(machine, "run stop.tsh"))
        enter(machine, r'file write user.tsh "file read first\nfile write elevated BAD"')
        failed(enter(machine, "run user.tsh"))
        absent(machine, "elevated")
        enter(machine, "echo user-data | save user.txt")
        contains(enter(machine, "file read user.txt"), "user-data")
        if any(row.get("label") != "user" for row in rows(machine)):
            raise AssertionError("Saved user data did not retain the caller's domain")
        checked("MAC applies to pipeline sources, save sinks, scripts and domain-pinned plans")

        machine.close()
        reboot = Machine("shell-persistence", deadline, disk=disk)
        machines.append(reboot)
        reboot.start()
        contains(enter(reboot, "file read first"), "yes")
        contains(enter(reboot, "file read user.txt"), "user-data")
        absent(reboot, "pinned")
        contains(enter(reboot, "run stop.tsh"), "error:")
        checked("Saved pipeline data and script contents persist across a fresh boot")
        reboot.close()

        network = DiskNetworkMachine("shell-peer", deadline, new_disk(disks.name, "network.img"))
        machines.append(network)
        network.start()
        enter(network, "format")
        status = rows(network, "net status")
        if not status or status[0].get("available") is not True:
            raise AssertionError(f"Typed network status does not report the real NIC: {status!r}")
        for field in ("rx", "tx"):
            if type(status[0].get(field)) is not int or status[0][field] < 0:
                raise AssertionError(f"Network status {field!r} is not a typed counter: {status!r}")
        checked("Typed network status reports the real RTL8139 and numeric counters")

        for target, counter in (("10.0.2.2", "echo4"), ("fd00::2", "echo6")):
            before_echoes = getattr(network.peer, counter)
            replies = json_value(enter(network, f"net ping {target} --count 2 --timeout 200 | where kind == reply | select source sequence rtt_ms | json"))
            if len(replies) != 2 or [row.get("sequence") for row in replies] != [1, 2]:
                raise AssertionError(f"Ping did not expose two ordered reply records: {replies!r}")
            for reply in replies:
                if set(reply) != {"source", "sequence", "rtt_ms"} or reply["source"] != target:
                    raise AssertionError(f"Ping projection has incorrect fields: {reply!r}")
                if type(reply["sequence"]) is not int or type(reply["rtt_ms"]) is not int or not 0 <= reply["rtt_ms"] <= 200:
                    raise AssertionError(f"Ping sequence/latency lost their numeric types: {reply!r}")
            if getattr(network.peer, counter) != before_echoes + 2:
                raise AssertionError("Typed ping results were not backed by real Ethernet echo requests")
        counters = rows(network, "net status")[0]
        if counters["rx"] < 4 or counters["tx"] < 4:
            raise AssertionError(f"Typed counters did not reflect the packet exchanges: {counters!r}")
        checked("IPv4 and IPv6 ping events remain typed through filtering, projection and JSON")

        network.peer.mode = "silent"
        lost = json_value(enter(network, "net ping 10.0.2.2 --count 2 --timeout 100 | json"))
        timeouts = [row for row in lost if row.get("kind") == "timeout"]
        summaries = [row for row in lost if row.get("kind") == "summary"]
        if [row.get("sequence") for row in timeouts] != [1, 2] or any(row.get("rtt_ms") is not None for row in timeouts):
            raise AssertionError(f"Typed timeouts are missing or masquerade as replies: {lost!r}")
        if len(summaries) != 1 or summaries[0].get("sent") != 2 or summaries[0].get("received") != 0:
            raise AssertionError(f"Typed packet-loss summary is incorrect: {lost!r}")
        if rows(network, "status")[0].get("code") != "error":
            raise AssertionError("Typed ping packet loss did not update status to error")
        enter(network, r'file write timeout.tsh "net ping 10.0.2.2 --count 1 --timeout 100 | json\nfile write after-timeout BAD"')
        stopped = enter(network, "run timeout.tsh")
        lost = json_value(stopped)
        if not any(row.get("kind") == "timeout" for row in lost):
            raise AssertionError("Script did not preserve its typed timeout result")
        if rows(network, "status")[0].get("code") != "error":
            raise AssertionError("Script hid the failed typed ping status")
        absent(network, "after-timeout")
        network.peer.mode = "reply"
        recovered = json_value(enter(network, "net ping 10.0.2.2 --count 1 --timeout 200 | where kind == reply | json"))
        if len(recovered) != 1:
            raise AssertionError("Typed ping failed to recover after packet-loss results")
        checked("Typed timeouts retain records and failed status; scripts stop before the next write")

        network.drain(0.05)
        before_frames = network.peer.frames
        for command in (
            "net ping 10.0.2.99 --count 1 --timeout 100 | nonsense",
            "net ping fd00::99 --count 1 --timeout 100 | select unknown | json",
            "net ping 10.0.2.99 --count 1 --timeout 100 | where unknown == 1 | json",
        ):
            failed(enter(network, command))
            network.drain(0.05)
            if network.peer.frames != before_frames:
                raise AssertionError(f"Malformed pipeline transmitted Ethernet before validation: {command!r}")
        checked("Invalid network pipeline stages are rejected before ARP, NDP or ICMP transmission")

        network.prompt = USER_PROMPT
        enter(network, "drop")
        for target in ("10.0.2.99", "fd00::99"):
            failed(enter(network, f"net ping {target} --count 1 --timeout 100 | json"))
            network.drain(0.05)
            if network.peer.frames != before_frames:
                raise AssertionError("A user-domain typed ping leaked an Ethernet packet")
        status = rows(network, "net status")
        if not status or status[0].get("available") is not True:
            raise AssertionError("User domain lost permitted network-status access")
        checked("User-domain typed network pipelines are denied before any packet emission")
    except Exception as error:
        failure = str(error)
    finally:
        for machine in machines:
            machine.close()
        transcript = bytearray()
        for machine in machines:
            transcript += f"\n--- QEMU {machine.name} ---\n".encode("ascii")
            transcript += machine.output
        (OUT / "shell-serial.txt").write_bytes(transcript)
        elapsed = time.monotonic() - beginning
        report = ["Tane Shell v1 QEMU integration test", f"Elapsed: {elapsed:.2f}s"]
        report += [f"Transport ({machine.name}): {getattr(machine, 'transport', 'stdio + Ethernet socketpair')}" for machine in machines]
        report += [f"PASS: {name}" for name in checks]
        report.append(f"FAIL: {failure}" if failure else "RESULT: PASS")
        (OUT / "shell-summary.txt").write_text("\n".join(report) + "\n", encoding="utf-8")
        disks.cleanup()
    if failure:
        print(f"FAIL {failure}", file=sys.stderr)
        return 1
    print(f"All {len(checks)} shell checks passed in {elapsed:.2f}s.")
    return 0


if __name__ == "__main__":
    sys.exit(main())
