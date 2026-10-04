#!/usr/bin/env python3
"""Exercise real ring 3 Rust programs, page permissions and syscall gates.

The existing QEMU/serial/ATA transport is shared with smoke.py. Every user
program is the compiled executable shipped in this image; these checks never
replace the kernel, scheduler, page tables or syscall implementation on the
host. Run after build.py, with the same QEMU environment as smoke.py.
"""
import hashlib
import re
import sys
import tempfile
import time

from smoke import Machine, OUT, USER_PROMPT, new_disk
from shell import contains, enter, failed, rows

TIME_LIMIT = 180.0
PROCESS_FRAMES = 12


def process_row(machine, pid):
    matches = [row for row in rows(machine, "proc list") if row.get("pid") == pid]
    if len(matches) != 1:
        raise AssertionError(f"Missing or duplicate process {pid}: {matches!r}")
    return matches[0]


def status(machine, expected):
    actual = rows(machine, "status")
    if len(actual) != 1 or actual[0].get("code") != expected:
        raise AssertionError(f"Expected shell status {expected!r}, got {actual!r}")


def account(machine):
    # proc list reaps a finished process after the CPU has left its stack.
    rows(machine, "proc list")
    memory = rows(machine, "mem")
    user = [row for row in rows(machine, "top") if row.get("domain") == "user"]
    if len(memory) != 1 or len(user) != 1:
        raise AssertionError(f"Missing memory/user accounting: {memory!r}, {user!r}")
    return memory[0]["free"], user[0]["tasks"], user[0]["frames"]


def unchanged(machine, baseline, context):
    current = account(machine)
    if current != baseline:
        raise AssertionError(f"{context} leaked or altered frames/accounting: {baseline!r} -> {current!r}")


def start_process(machine, program, argument=None, verb="run", check_status=True):
    command = f"proc {verb} {program}"
    if argument is not None:
        # Quote the one literal argument; shell expansion must stay data.
        escaped = argument.replace("\\", "\\\\").replace('"', '\\"').replace("$", "\\$")
        command += f' "{escaped}"'
    result = enter(machine, command)
    match = re.search(rb"(?:as pid|pid)[ =]+(\d+)", result)
    if not match:
        raise AssertionError(f"Process did not start: {command!r}: {result!r}")
    if check_status:
        status(machine, "success")
    return int(match.group(1))


def wait_process(machine, pid, state="exited", expected_status="success", timeout=8.0):
    result = enter(machine, f"proc wait {pid}", timeout=timeout)
    status(machine, expected_status)
    info = process_row(machine, pid)
    if info.get("domain") != "user" or info.get("state") != state or info.get("frames") != 0:
        raise AssertionError(f"Wrong completed process metadata: {info!r}; output {result!r}")
    return result, info


def run_ok(machine, program, argument, expected):
    baseline = account(machine)
    pid = start_process(machine, program, argument)
    result, info = wait_process(machine, pid)
    contains(result, expected)
    contains(enter(machine, f"proc output {pid}"), expected)
    unchanged(machine, baseline, f"{program} {argument or ''} exit")
    return pid, info


def kill_process(machine, pid):
    contains(enter(machine, f"task kill {pid}"), f"killed pid {pid}")
    result, info = wait_process(machine, pid, "killed", "error")
    return result, info


def main():
    OUT.mkdir(exist_ok=True)
    image = OUT / "tane-os.img"
    if not image.is_file():
        sys.exit("Missing build/tane-os.img. Run python3 build.py first.")
    image_hash = hashlib.sha256(image.read_bytes()).hexdigest()
    beginning = time.monotonic()
    deadline = beginning + TIME_LIMIT
    checks, machines = [], []
    failure = None
    disks = tempfile.TemporaryDirectory(prefix="tane-process-disks-")

    def checked(name):
        checks.append(name)
        print(f"PASS {name}", flush=True)

    try:
        build_info = (OUT / "build-info.txt").read_text(encoding="utf-8")
        recorded_hash = re.search(r"^SHA256: ([0-9a-f]{64})$", build_info, re.M)
        if not recorded_hash or recorded_hash.group(1) != image_hash:
            raise AssertionError("Build report does not identify the tested boot image")
        disk = new_disk(disks.name, "process.img")
        machine = Machine("process", deadline, disk=disk)
        machines.append(machine)
        machine.start()
        enter(machine, "format")
        programs = rows(machine, "proc programs")
        names = {row.get("name") for row in programs}
        expected_names = {"hello", "echo", "busy", "sleep", "isolate", "files", "probe"}
        if not expected_names.issubset(names):
            raise AssertionError(f"Missing bundled Rust programs: {programs!r}")
        if any(not isinstance(row.get("bytes"), int) or not 32 < row["bytes"] <= 4096 for row in programs):
            raise AssertionError(f"Invalid bounded executable sizes: {programs!r}")
        initial = account(machine)
        if initial[1:] != (0, 0):
            raise AssertionError(f"Fresh boot leaked user resources: {initial!r}")
        checked("Ring 3 program catalog and clean user resource baseline")

        hello_pid, _ = run_ok(machine, "hello", None, "Hello from Rust ring3. pid=")
        contains(enter(machine, f"proc output {hello_pid}"), str(hello_pid))
        run_ok(machine, "echo", "literal | file write injected BAD", "literal | file write injected BAD")
        if any(row.get("name") == "injected" for row in rows(machine, "file list")):
            raise AssertionError("A process argument was reparsed as a shell command")
        run_ok(machine, "echo", "", b"\r\n")
        checked("Compiled Rust runs in user domain; literal arguments and captured output survive exit")

        run_ok(machine, "sleep", "30", "sleep: awake")
        checked("User sleep syscall blocks and resumes through the scheduler")

        baseline = account(machine)
        # Avoid additional status round trips until the one dual-live
        # snapshot; the 20-tick program deliberately has a short lifetime.
        first = start_process(machine, "isolate", "111111", check_status=False)
        second = start_process(machine, "isolate", "222222", check_status=False)
        if first == second:
            raise AssertionError("Two user processes share a PID")
        simultaneous = {row["pid"]: row for row in rows(machine, "proc list")}
        for pid in (first, second):
            info = simultaneous.get(pid, {})
            if info.get("state") not in {"ready", "running"} or info.get("frames") != PROCESS_FRAMES:
                raise AssertionError(f"Isolation proof did not overlap two live address spaces: {simultaneous!r}")
        first_result, first_info = wait_process(machine, first)
        second_result, second_info = wait_process(machine, second)
        contains(first_result, "isolate: ok seed=111111")
        contains(second_result, "isolate: ok seed=222222")
        if first_info.get("cpu_ticks", 0) == 0 or second_info.get("cpu_ticks", 0) == 0:
            raise AssertionError(f"Timer did not preempt both user loops: {first_info!r}, {second_info!r}")
        unchanged(machine, baseline, "two isolated processes")
        run_ok(machine, "isolate", "333333", "isolate: ok seed=333333")
        checked("Same virtual data address remains private under timer preemption and reused pages start zeroed")

        baseline = account(machine)
        first = start_process(machine, "busy")
        second = start_process(machine, "busy")
        live = account(machine)
        if live != (baseline[0] - 2 * PROCESS_FRAMES, baseline[1] + 2, baseline[2] + 2 * PROCESS_FRAMES):
            raise AssertionError(f"Two process allocations must charge 24 frames to user: {baseline!r} -> {live!r}")
        for pid in (first, second):
            info = process_row(machine, pid)
            if info.get("domain") != "user" or info.get("frames") != PROCESS_FRAMES:
                raise AssertionError(f"Privileged creator changed user label or allocation: {info!r}")
        failed(enter(machine, "proc run hello"))
        unchanged(machine, live, "refused third user process")
        before_ticks = {pid: process_row(machine, pid)["cpu_ticks"] for pid in (first, second)}
        # Cross the one-second accounting window so a process that already
        # reached its 30% user share has a chance to run again.
        enter(machine, "sleep 1100")
        contains(enter(machine, "echo shell remains responsive"), "shell remains responsive")
        for pid in (first, second):
            if process_row(machine, pid)["cpu_ticks"] <= before_ticks[pid]:
                raise AssertionError(f"Busy user process {pid} was not preempted/accounted")
        checked("Busy ring 3 loops are preemptible; immutable user quota rejects a third allocation atomically")

        machine.send(f"proc wait {first}".encode("ascii"))
        machine.drain()
        marker = len(machine.output)
        machine.send(b"\r")
        machine.drain(0.05)
        machine.send(b"\x03")
        cancelled = machine.wait_for(machine.prompt, start=marker, timeout=3)
        contains(cancelled, "cancelled")
        status(machine, "cancelled")
        if process_row(machine, first).get("state") not in {"ready", "running"}:
            raise AssertionError("Cancelling a wait terminated the user process")
        kill_process(machine, first)
        kill_process(machine, second)
        unchanged(machine, baseline, "killed user processes")
        checked("Ctrl-C cancels waiting; explicit kill captures outcome and releases every frame")

        # An ordinary privileged kernel demonstration task must coexist with
        # user CR3/TSS switches and survive a user process's hardware fault.
        kernel = enter(machine, "task spawn spin")
        kernel_pid = int(re.search(rb"as pid (\d+)", kernel).group(1))
        baseline = account(machine)
        faults = (
            ("kernel-read", 14), ("kernel-data", 14), ("kernel-write", 14),
            ("kernel-data-write", 14), ("vga", 14), ("vga-read", 14),
            ("code-write", 14), ("nx", 14), ("stack-nx", 14),
            ("null", 14), ("guard", 14), ("stack-end", 14),
            ("cli", 13), ("hlt", 13), ("in", 13), ("out", 13), ("int48", 13),
            ("sse", 7), ("x87", 7), ("ud2", 6),
            ("syscall", 6), ("sysenter", (6, 13)), ("fsgsbase", 6), ("xsave", 6),
        )
        for case, vector in faults:
            pid = start_process(machine, "probe", case)
            result, _ = wait_process(machine, pid, "faulted", "error")
            contains(result, f"probe: {case}")
            expected_vectors = vector if isinstance(vector, tuple) else (vector,)
            actual = re.search(rb"(?:vector|fault)[ =:#]+(\d+)\b", result)
            if not actual or int(actual.group(1)) not in expected_vectors:
                raise AssertionError(f"{case} raised the wrong hardware exception; expected vector {vector}: {result!r}")
            if b"forbidden instruction returned" in result:
                raise AssertionError(f"Hardware permission probe returned to user code: {case}")
            contains(enter(machine, "echo survived user fault"), "survived user fault")
            unchanged(machine, baseline, f"{case} fault")
            kernel_rows = [row for row in rows(machine, "task list") if row.get("pid") == kernel_pid]
            if len(kernel_rows) != 1 or kernel_rows[0].get("domain") != "admin":
                raise AssertionError(f"User fault damaged the kernel task: {kernel_rows!r}")
            checked(f"Hardware {case} terminates only its user process with vector {int(actual.group(1))}")
        contains(enter(machine, f"task kill {kernel_pid}"), f"killed pid {kernel_pid}")
        unchanged(machine, initial, "all hardware faults and kernel task cleanup")

        before_files = rows(machine, "file list")
        run_ok(machine, "probe", "badptr", "probe: bad pointers/length/syscall rejected")
        if rows(machine, "file list") != before_files:
            raise AssertionError("Rejected syscall pointers altered the filesystem")
        run_ok(machine, "probe", "denied", "probe: privileged syscalls denied")
        if rows(machine, "file list") != before_files:
            raise AssertionError("User privileged syscall altered the filesystem")
        audit = rows(machine, "audit")
        if not any(row.get("subject") == "user" and row.get("reason") == "policy" for row in audit):
            raise AssertionError(f"User-domain syscall denials were not audited: {audit!r}")
        checked("Syscall pointer, overflow, size and number validation has no effect; privileged calls are audited")

        run_ok(machine, "files", "ring3-demo", "files: written by Rust ring3")
        file_rows = rows(machine, "file list")
        demo = [row for row in file_rows if row.get("name") == "ring3-demo"]
        if len(demo) != 1 or demo[0].get("label") != "user":
            raise AssertionError(f"A privileged creator gave its child an admin-labelled file: {demo!r}")
        contains(enter(machine, "file read ring3-demo"), "written by Rust ring3")
        baseline = account(machine)
        contains(enter(machine, 'plan file append ring3-demo "SHELL-STALE"'), "plan")
        pid = start_process(machine, "files", "ring3-demo")
        result, _ = wait_process(machine, pid)
        contains(result, "files: written by Rust ring3")
        changed_content = enter(machine, "file read ring3-demo")
        if changed_content.count(b"written by Rust ring3") != 2:
            raise AssertionError(f"User process did not commit its concurrent file mutation: {changed_content!r}")
        failed(enter(machine, "apply"))
        after_rejection = enter(machine, "file read ring3-demo")
        if after_rejection != changed_content or b"SHELL-STALE" in after_rejection:
            raise AssertionError("A stale shell plan overwrote a user process's committed file mutation")
        enter(machine, 'plan file append ring3-demo "SHELL-FRESH"')
        enter(machine, "apply")
        status(machine, "success")
        contains(enter(machine, "file read ring3-demo"), "SHELL-FRESH")
        unchanged(machine, baseline, "user file mutation and stale/fresh shell plans")
        checked("Ring 3 file mutation invalidates a staged shell plan; rejection retains user data and a fresh plan commits")
        run_ok(machine, "files", "badptr", "files: bad pointer rejected without write")
        cap = [row for row in rows(machine, "file list") if row.get("name") == "ring3-cap"]
        if len(cap) != 1 or cap[0].get("bytes") != 0:
            raise AssertionError(f"Invalid user write modified its file: {cap!r}")
        run_ok(machine, "files", "rights", "files: write right and closed handle rejected")
        checked("User file capabilities enforce rights, reject closed handles and validate input before storage mutation")

        run_ok(machine, "files", "stale", "files: stale generation and reused handle rejected")
        checked("File generations invalidate stale capabilities and reused slots never revive closed tokens")

        baseline = account(machine)
        holder = start_process(machine, "files", "hold")
        output = enter(machine, f"proc output {holder}")
        token = re.search(rb"files: handle=(\d+)", output)
        if not token:
            raise AssertionError(f"Capability holder did not expose its test token: {output!r}")
        before_files = rows(machine, "file list")
        thief = start_process(machine, "files", "steal " + token.group(1).decode("ascii"))
        result, _ = wait_process(machine, thief)
        contains(result, "files: foreign handle rejected")
        if rows(machine, "file list") != before_files:
            raise AssertionError("Another PID used a foreign capability to alter file data")
        if process_row(machine, holder).get("state") != "sleeping":
            raise AssertionError("The foreign-capability test did not overlap two live processes")
        kill_process(machine, holder)
        unchanged(machine, baseline, "capability holder and foreign-PID rejection")
        checked("Capabilities are scoped to the issuing live PID even when a second process knows the token")

        enter(machine, "file write admin-secret classified")
        baseline = account(machine)
        pid = start_process(machine, "files", "admin-secret")
        result, _ = wait_process(machine, pid, "exited", "error")
        contains(result, "files: open denied/error")
        unchanged(machine, baseline, "denied child access to creator admin file")
        contains(enter(machine, "file read admin-secret"), "classified")
        enter(machine, "proc install hello adminhello.tane")
        installed = [row for row in rows(machine, "file list") if row.get("name") == "adminhello.tane"]
        if len(installed) != 1 or installed[0].get("label") != "admin":
            raise AssertionError(f"Executable install lost creator label: {installed!r}")
        baseline = account(machine)
        failed(enter(machine, "proc exec adminhello.tane"))
        unchanged(machine, baseline, "admin executable denied to destination user before allocation")
        checked("Admin cannot transfer its file-read authority into an unprivileged process or executable mapping")

        baseline = account(machine)
        for command in (
            "proc run absent", "proc run", "proc wait 0", "proc output 0",
            "proc run hello | nonsense", "proc run hello | json",
            "proc programs | select unknown | json", "proc list | sort unknown",
            'proc run echo "' + "X" * 129 + '"',
        ):
            failed(enter(machine, command))
            unchanged(machine, baseline, f"invalid operation {command!r}")
        checked("Malformed process commands and arguments are rejected before allocation or execution")

        machine.prompt = USER_PROMPT
        enter(machine, "drop")
        status(machine, "success")
        if any(row.get("name") in {"admin-secret", "adminhello.tane"} for row in rows(machine, "file list")):
            raise AssertionError("User shell lists privileged files")
        baseline = account(machine)
        failed(enter(machine, "proc exec adminhello.tane"))
        failed(enter(machine, "file read admin-secret"))
        failed(enter(machine, "audit"))
        failed(enter(machine, "format"))
        unchanged(machine, baseline, "user attempts at admin files/control")
        run_ok(machine, "hello", None, "Hello from Rust ring3. pid=")
        run_ok(machine, "probe", "denied", "probe: privileged syscalls denied")
        checked("Dropped shell can start user programs while admin storage, audit and format remain protected")

        enter(machine, "proc install echo echo.tane")
        executable = [row for row in rows(machine, "file list") if row.get("name") == "echo.tane"]
        if len(executable) != 1 or executable[0].get("label") != "user":
            raise AssertionError(f"User executable has the wrong filesystem label: {executable!r}")
        baseline = account(machine)
        pid = start_process(machine, "echo.tane", "from persisted executable | still literal", "exec")
        result, _ = wait_process(machine, pid)
        contains(result, "from persisted executable | still literal")
        unchanged(machine, baseline, "disk executable exit")
        enter(machine, "file write bad.tane invalid-executable")
        failed(enter(machine, "proc exec bad.tane"))
        unchanged(machine, baseline, "malformed disk executable")
        checked("User-labelled TaneFS executables load real code; invalid headers never allocate process resources")

        machine.close()
        fresh = Machine("process-persist", deadline, disk=disk)
        machines.append(fresh)
        fresh.start()
        baseline = account(fresh)
        pid = start_process(fresh, "echo.tane", "fresh boot executable", "exec")
        result, _ = wait_process(fresh, pid)
        contains(result, "fresh boot executable")
        unchanged(fresh, baseline, "persisted executable after fresh boot")
        failed(enter(fresh, "proc exec adminhello.tane"))
        unchanged(fresh, baseline, "persisted privileged executable refusal")
        checked("Saved user executable survives a fresh QEMU boot and destination access checks remain enforced")
    except Exception as error:
        failure = str(error)
    finally:
        for machine in machines:
            machine.close()
        if image.is_file() and hashlib.sha256(image.read_bytes()).hexdigest() != image_hash:
            changed = "The boot image changed while process integration tests were running"
            failure = f"{failure}; {changed}" if failure else changed
        transcript = bytearray()
        for machine in machines:
            transcript += f"\n--- QEMU {machine.name} ---\n".encode("ascii")
            transcript += machine.output
        (OUT / "process-serial.txt").write_bytes(transcript)
        elapsed = time.monotonic() - beginning
        report = ["Tane OS ring 3 QEMU regression test", f"Image SHA256: {image_hash}", f"Elapsed: {elapsed:.2f}s"]
        report += [f"Transport ({machine.name}): {machine.transport}" for machine in machines]
        report += [f"PASS: {name}" for name in checks]
        report.append(f"FAIL: {failure}" if failure else "RESULT: PASS")
        (OUT / "process-summary.txt").write_text("\n".join(report) + "\n", encoding="utf-8")
        disks.cleanup()
    if failure:
        print(f"FAIL {failure}", file=sys.stderr)
        return 1
    print(f"All {len(checks)} process checks passed in {elapsed:.2f}s.")
    return 0


if __name__ == "__main__":
    sys.exit(main())
