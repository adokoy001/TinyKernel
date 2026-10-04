#!/usr/bin/env python3
"""Exercise real heap mappings, process lifecycle and TaneFS v2 in QEMU.

Programs are the bounded Rust executables embedded in the tested boot image.
The host supplies only serial/QMP transport and an independent ATA fixture.
"""
import hashlib
import re
import struct
import sys
import tempfile
import time

from smoke import Machine, OUT, USER_PROMPT, new_disk
from shell import contains, enter, failed, rows
from process import account, kill_process, process_row, run_ok, start_process, status, unchanged, wait_process

TIME_LIMIT = 180.0
HEAP_BASE = 0x40010000


def legacy_disk(directory):
    """Original v1 byte layout from 6cb72ee:src/fs.rs, without running Rust.

    This fixture is intentionally written by an independent format reader:
    28-byte FNV-1a header, four table sectors, and fixed eight-sector slots.
    """
    def checksum(data):
        value = 0x811C9DC5
        for byte in data:
            value = ((value ^ byte) * 0x01000193) & 0xFFFFFFFF
        return value

    disk = new_disk(directory, "legacy.img")
    data = bytearray(1 << 20)
    data[:8] = b"TANEFS1\0"
    struct.pack_into("<IIIII", data, 8, 1, 32, 8, 8, len(data) // 512)
    struct.pack_into("<I", data, 28, checksum(data[:28]))
    for slot, name, label, contents, generation in (
        (0, b"legacy-admin", 1, b"original admin bytes", 7),
        (1, b"legacy-user", 2, b"original user bytes", 11),
    ):
        at = 512 + slot * 64
        data[at:at + 3] = bytes((1, label, len(name)))
        struct.pack_into("<III", data, at + 4, len(contents), checksum(contents), generation)
        data[at + 16:at + 16 + len(name)] = name
        start = (8 + slot * 8) * 512
        data[start:start + len(contents)] = contents
    disk.write_bytes(data)
    return disk


def child(machine, parent):
    live = rows(machine, "proc list")
    children = [row for row in live if row.get("parent") == parent]
    if len(children) != 1:
        raise AssertionError(f"Parent {parent} did not have exactly one child: {live!r}")
    return children[0]


def child_pid(output):
    match = re.search(rb"control: child=(\d+)", output)
    if not match:
        raise AssertionError(f"Missing child PID in control output: {output!r}")
    return int(match.group(1))


def pause(machine, pid):
    enter(machine, f"proc pause {pid}")
    status(machine, "success")
    row = process_row(machine, pid)
    if row.get("state") != "stopped":
        raise AssertionError(f"Process was not stopped: {row!r}")
    return row


def resume(machine, pid):
    enter(machine, f"proc resume {pid}")
    status(machine, "success")


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
    disks = tempfile.TemporaryDirectory(prefix="tane-advanced-disks-")

    def checked(name):
        checks.append(name)
        print(f"PASS {name}", flush=True)

    try:
        recorded = re.search(r"^SHA256: ([0-9a-f]{64})$", (OUT / "build-info.txt").read_text(), re.M)
        if not recorded or recorded.group(1) != image_hash:
            raise AssertionError("Build report does not identify the tested boot image")
        disk = new_disk(disks.name, "advanced.img")
        machine = Machine("advanced", deadline, disk=disk)
        machines.append(machine)
        machine.start()
        enter(machine, "format")
        initial = account(machine)
        if initial[1:] != (0, 0):
            raise AssertionError(f"Fresh boot retained user resources: {initial!r}")
        catalog = rows(machine, "proc programs")
        if not {"heap", "control"}.issubset({row.get("name") for row in catalog}):
            raise AssertionError(f"Missing new compiled Rust programs: {catalog!r}")
        if any(not 32 < row.get("bytes", 0) <= 4096 for row in catalog):
            raise AssertionError(f"A user executable exceeded its bounded image: {catalog!r}")
        checked("Heap and process-control Rust executables are bounded and boot with clean resource accounting")

        run_ok(machine, "heap", "basic", "heap: grow cross-page shrink-regrow zeroing ok")
        checked("Heap grows with zeroed pages; syscall copy crosses heap pages and shrink-regrow preserves only retained bytes")
        run_ok(machine, "heap", "limit", "heap: limit flags and full release-regrow zeroing ok")
        checked("Heap rejects excessive pages and flags atomically; all eight released pages return zeroed")

        busy = start_process(machine, "busy")
        baseline = account(machine)
        run_ok(machine, "heap", "quota", "heap: shared user quota rejected growth")
        unchanged(machine, baseline, "quota-denied heap process")
        kill_process(machine, busy)
        unchanged(machine, initial, "heap quota sibling cleanup")
        checked("Heap growth is charged to the shared user frame quota and a refused allocation leaks no pages")

        holder = start_process(machine, "heap", "hold 85")
        live = account(machine)
        if live != (initial[0] - 20, 1, 20):
            raise AssertionError(f"Eight heap pages must raise a user's allocation to twenty frames: {live!r}")
        memory = rows(machine, f"proc memory {holder}")
        heap_rows = [row for row in memory if row.get("region") == "heap"]
        if len(heap_rows) != 1 or heap_rows[0].get("base") != HEAP_BASE or heap_rows[0].get("pages") != 8:
            raise AssertionError(f"Heap mapping metadata is incorrect: {memory!r}")
        if heap_rows[0].get("bytes") != 32768 or heap_rows[0].get("read") is not True or heap_rows[0].get("write") is not True or heap_rows[0].get("execute") is not False:
            raise AssertionError(f"Heap mapping lost its RW/NX permissions: {heap_rows!r}")
        by_region = {row["region"]: row for row in memory}
        for region, permissions in (("code", (True, False, True)), ("data", (True, True, False)), ("stack", (True, True, False))):
            if tuple(by_region.get(region, {}).get(key) for key in ("read", "write", "execute")) != permissions:
                raise AssertionError(f"Typed {region} permissions are incorrect: {memory!r}")
        guards = [row for row in memory if "guard" in row.get("region", "")]
        if len(guards) < 2 or any(row.get(key) is not False for row in guards for key in ("read", "write", "execute")):
            raise AssertionError(f"Stack and heap guards must be reported unmapped: {memory!r}")
        failed(enter(machine, "proc run hello"))
        unchanged(machine, live, "refused child beside eight-page heap")
        kill_process(machine, holder)
        unchanged(machine, initial, "heap holder kill")
        run_ok(machine, "heap", "limit", "heap: limit flags and full release-regrow zeroing ok")
        checked("Typed memory records show the actual RW/NX heap; quota rejection and kill reclaim its full twenty frames")

        for mode, text in (("freed", "accessing freed page"), ("nx", "executing NX page"), ("guard", "accessing unmapped guard")):
            pid = start_process(machine, "heap", mode)
            output, _ = wait_process(machine, pid, "faulted", "error")
            contains(output, f"heap: {text}")
            contains(output, "fault vector 14")
            if b"forbidden access returned" in output:
                raise AssertionError(f"Forbidden heap access returned: {mode}")
            unchanged(machine, initial, f"heap {mode} fault")
            checked(f"Real hardware rejects heap {mode} access and leaves the shell and frame pool intact")

        busy = start_process(machine, "busy")
        failed(enter(machine, f"proc pause {busy} | json"))
        if process_row(machine, busy).get("state") not in {"ready", "running"}:
            raise AssertionError("Pipeline validation stopped a process before rejecting the whole command")
        stopped = pause(machine, busy)
        stopped_resources = account(machine)
        enter(machine, "sleep 150")
        later = process_row(machine, busy)
        if later.get("state") != "stopped" or later.get("cpu_ticks") != stopped.get("cpu_ticks"):
            raise AssertionError(f"A stopped process kept consuming CPU: {stopped!r} -> {later!r}")
        unchanged(machine, stopped_resources, "paused user process")
        resume(machine, busy)
        enter(machine, "sleep 1100")
        if process_row(machine, busy).get("cpu_ticks", 0) <= stopped.get("cpu_ticks", 0):
            raise AssertionError("Resumed busy process did not run again")
        kill_process(machine, busy)
        unchanged(machine, initial, "paused/resumed busy cleanup")
        checked("Pause halts scheduling without releasing memory; resume restores preemptible execution")
        for command in ("proc memory 999999", "proc memory 1", "proc pause 0", "proc pause 1", "proc resume 999999", "proc pause"):
            failed(enter(machine, command))
            unchanged(machine, initial, f"invalid control {command!r}")
        checked("Invalid memory and process-control targets are rejected; mutating control cannot run as a pipeline source")

        for mode in ("basic", "exit", "fault", "kill"):
            pid, _ = run_ok(machine, "control", mode, "control: outcome consumed and child slot reusable")
            output = enter(machine, f"proc output {pid}")
            spawned = child_pid(output)
            outcome = process_row(machine, spawned)
            expected = {"basic": "exited", "exit": "exited", "fault": "faulted", "kill": "killed"}[mode]
            if outcome.get("state") != expected or outcome.get("frames") != 0:
                raise AssertionError(f"Incorrect child outcome for {mode}: {outcome!r}")
            if mode == "basic":
                contains(enter(machine, f"proc output {spawned}"), "child literal | command")
            checked(f"A ring 3 parent receives its {mode} child outcome once and can reuse its child slot")

        run_ok(machine, "control", "validate", "control: pointers flags foreign and consumed child rejected")
        checked("Spawn and wait validate whole pointers, lengths and flags; bad status buffers do not consume a child and cross-page copy succeeds")
        foreign = start_process(machine, "busy")
        run_ok(machine, "control", f"foreign {foreign}", "control: live foreign process protected")
        if process_row(machine, foreign).get("state") not in {"ready", "running"}:
            raise AssertionError("An unrelated user process acquired wait or kill authority over a live PID")
        kill_process(machine, foreign)
        unchanged(machine, initial, "live foreign-PID protection")
        checked("A real concurrent User PID grants no direct-child wait or kill authority")
        run_ok(machine, "control", "quota", "control: child quota rollback and retry ok")
        checked("A parent's heap can exhaust child-creation quota; freeing it permits a clean retry")

        parent = start_process(machine, "control", "live")
        offspring = child(machine, parent)
        parent_info = process_row(machine, parent)
        if parent_info.get("state") != "waiting" or offspring.get("state") != "sleeping":
            raise AssertionError(f"wait did not block the parent beside its sleeping child: {parent_info!r}, {offspring!r}")
        if account(machine) != (initial[0] - 24, 2, 24):
            raise AssertionError("Blocking parent and child did not retain two distinct twelve-frame allocations")
        output, _ = wait_process(machine, parent)
        contains(output, "control: blocked wait resumed")
        unchanged(machine, initial, "blocking parent and sleeping child")
        checked("wait blocks the parent in the scheduler while its independently mapped child sleeps, then resumes on completion")

        parent = start_process(machine, "control", "live")
        offspring = child(machine, parent)
        stopped = pause(machine, parent)
        enter(machine, "sleep 1750")
        after = process_row(machine, parent)
        if after.get("state") != "stopped" or after.get("cpu_ticks") != stopped.get("cpu_ticks"):
            raise AssertionError(f"Child completion ran a paused waiting parent: {stopped!r} -> {after!r}")
        if process_row(machine, offspring["pid"]).get("state") != "exited":
            raise AssertionError("Child did not complete while its waiting parent was stopped")
        if account(machine) != (initial[0] - 12, 1, 12):
            raise AssertionError("Completing a stopped parent's child did not reclaim only the child frames")
        resume(machine, parent)
        output, _ = wait_process(machine, parent)
        contains(output, "control: blocked wait resumed")
        unchanged(machine, initial, "paused waiting parent")
        checked("A child's completion delivers a wait result without scheduling its paused parent; resume consumes the result safely")

        parent = start_process(machine, "control", "pending")
        pause(machine, parent)
        enter(machine, "sleep 50")
        # The completed-child reservation must outlive the independent ring
        # of eight shell-visible outcomes and forbid a second spawn.
        for _ in range(10):
            run_ok(machine, "hello", None, "Hello from Rust ring3. pid=")
        enter(machine, "sleep 1600")
        if process_row(machine, parent).get("state") != "stopped":
            raise AssertionError("Sleeping-parent deadline bypassed the stopped flag")
        resume(machine, parent)
        output, _ = wait_process(machine, parent)
        contains(output, "control: completed child retained until wait")
        unchanged(machine, initial, "retained child reservation")
        checked("A completed child remains waitable after ten unrelated exits and occupies its parent's slot until consumption")

        parent = start_process(machine, "control", "live")
        offspring = child(machine, parent)
        kill_process(machine, parent)
        if process_row(machine, offspring["pid"]).get("state") != "sleeping":
            raise AssertionError("Killing a waiting parent implicitly killed its independent child")
        replacement, _ = run_ok(machine, "hello", None, "Hello from Rust ring3. pid=")
        if replacement == parent:
            raise AssertionError("A new process reused a dead parent's PID")
        output, _ = wait_process(machine, offspring["pid"])
        contains(output, "sleep: awake")
        unchanged(machine, initial, "killed-parent orphan and reused task slot")
        checked("Parent death leaves its child alive; old completion cannot target the replacement task in the reused slot")

        parent = start_process(machine, "control", "orphan")
        output, _ = wait_process(machine, parent)
        contains(output, "control: parent leaving live child")
        offspring = child_pid(output)
        if process_row(machine, offspring).get("state") != "sleeping":
            raise AssertionError("Normal parent exit lost its live child")
        wait_process(machine, offspring)
        unchanged(machine, initial, "normally exited parent and orphan")
        checked("Normal parent exit also preserves a live child's execution and eventual full reclamation")

        run_ok(machine, "files", "position", "files: seek write truncate zero-fill rights and stale cursor checks ok")
        position = [row for row in rows(machine, "file list") if row.get("name") == "ring3-position"]
        if len(position) != 1 or position[0].get("label") != "user" or position[0].get("bytes") != 10:
            raise AssertionError(f"Positioned user I/O produced wrong file metadata: {position!r}")
        checked("User seek, positioned write and truncate enforce capabilities, stale identities, cursor rollback and zero-filled growth")

        enter(machine, "file write admin-source abcdef")
        enter(machine, "file write occupied unchanged")
        enter(machine, "file rename admin-source admin-renamed")
        names = {row.get("name"): row for row in rows(machine, "file list")}
        if "admin-source" in names or names.get("admin-renamed", {}).get("label") != "admin":
            raise AssertionError(f"Rename changed file identity visibility or label: {names!r}")
        before = rows(machine, "file list")
        failed(enter(machine, "file rename admin-renamed occupied"))
        if rows(machine, "file list") != before:
            raise AssertionError("Refused rename overwrote a destination or changed metadata")
        contains(enter(machine, "file read occupied"), "unchanged")
        enter(machine, "plan file append admin-renamed STALE")
        enter(machine, "file truncate admin-renamed 3")
        failed(enter(machine, "apply"))
        contains(enter(machine, "file read admin-renamed"), b"\r\nabc\r\n")
        enter(machine, "file truncate admin-renamed 6")
        metadata = [row for row in rows(machine, "file list") if row.get("name") == "admin-renamed"]
        if len(metadata) != 1 or metadata[0].get("bytes") != 6:
            raise AssertionError(f"Truncate-grow produced wrong metadata: {metadata!r}")
        failed(enter(machine, "file truncate admin-renamed 4097"))
        checked("Rename preserves labels and refuses overwrite; truncate changes length and invalidates staged shell plans")

        status_rows = rows(machine, "file status")
        if len(status_rows) != 1:
            raise AssertionError(f"Storage status must be a single typed record: {status_rows!r}")
        if status_rows[0].get("mounted") is not True or status_rows[0].get("version") != 2 or status_rows[0].get("readonly") is not False or status_rows[0].get("error") is not None:
            raise AssertionError(f"Freshly formatted storage must be writable TaneFS v2: {status_rows!r}")
        checked_rows = rows(machine, "file check")
        visible = rows(machine, "file list")
        if len(checked_rows) != 1 or checked_rows[0].get("files") != len(visible) or checked_rows[0].get("bytes") != sum(row["bytes"] for row in visible):
            raise AssertionError(f"Checksum report disagrees with visible files: {checked_rows!r}, {visible!r}")
        enter(machine, "file sync")
        status(machine, "success")
        checked("Typed disk status and checksum verification report visible files; explicit sync completes through the ATA driver")

        enter(machine, "proc install echo admin-echo.tane")
        run_ok(machine, "control", "deniedfile admin-echo.tane", "control: privileged child file denied")
        machine.prompt = USER_PROMPT
        enter(machine, "drop")
        failed(enter(machine, "file rename admin-renamed stolen"))
        failed(enter(machine, "file truncate admin-renamed 0"))
        failed(enter(machine, "file sync"))
        failed(enter(machine, "file upgrade"))
        before = rows(machine, "file list")
        if any(row.get("label") != "user" for row in before):
            raise AssertionError(f"User storage commands exposed privileged files: {before!r}")
        checked_rows = rows(machine, "file check")
        if checked_rows[0].get("files") != len(before) or checked_rows[0].get("bytes") != sum(row["bytes"] for row in before):
            raise AssertionError("User checksum verification leaked privileged file counts or byte lengths")
        enter(machine, "proc install echo child-echo.tane")
        run_ok(machine, "control", "file child-echo.tane", "control: file child exited")
        enter(machine, "file write user-old persistent")
        enter(machine, "file rename user-old user-new")
        enter(machine, "file truncate user-new 4")
        contains(enter(machine, "file read user-new"), b"\r\npers\r\n")
        checked("MAC protects rename, truncate, sync and file-backed child creation while checksum reports follow caller visibility")

        machine.close()
        fresh = Machine("advanced-persist", deadline, disk=disk)
        machines.append(fresh)
        fresh.start()
        fresh_baseline = account(fresh)
        contains(enter(fresh, "file read user-new"), b"\r\npers\r\n")
        contains(enter(fresh, "file read occupied"), "unchanged")
        failed(enter(fresh, "file read user-old"))
        run_ok(fresh, "control", "file child-echo.tane", "control: file child exited")
        rows(fresh, "file check")
        enter(fresh, "file sync")
        unchanged(fresh, fresh_baseline, "persisted storage check and sync")
        checked("Renamed and truncated files plus file-backed child code survive a fresh QEMU boot")

        old_disk = legacy_disk(disks.name)
        old_bytes = old_disk.read_bytes()
        legacy = Machine("advanced-legacy", deadline, disk=old_disk)
        machines.append(legacy)
        legacy.start()
        old_status = rows(legacy, "file status")
        if len(old_status) != 1 or old_status[0].get("version") != 1 or old_status[0].get("readonly") is not True:
            raise AssertionError(f"A v1 disk must mount read-only: {old_status!r}")
        old_metadata = rows(legacy, "file list")
        expected_old = {"legacy-admin": ("admin", 7), "legacy-user": ("user", 11)}
        if {row["name"]: (row["label"], row["generation"]) for row in old_metadata} != expected_old:
            raise AssertionError(f"v1 labels or generations changed on mount: {old_metadata!r}")
        contains(enter(legacy, "file read legacy-admin"), "original admin bytes")
        contains(enter(legacy, "file read legacy-user"), "original user bytes")
        failed(enter(legacy, "file write legacy-user MUST-NOT-WRITE"))
        failed(enter(legacy, "plan file write legacy-user MUST-NOT-PLAN"))
        failed(enter(legacy, "apply"))
        if rows(legacy, "file list") != old_metadata or old_disk.read_bytes() != old_bytes:
            raise AssertionError("A rejected legacy write or plan changed v1 disk bytes")
        enter(legacy, "file upgrade")
        status(legacy, "success")
        upgraded = rows(legacy, "file status")
        if len(upgraded) != 1 or upgraded[0].get("version") != 2 or upgraded[0].get("readonly") is not False:
            raise AssertionError(f"Explicit admin upgrade did not publish writable v2: {upgraded!r}")
        if rows(legacy, "file list") != old_metadata:
            raise AssertionError("Upgrade altered file names, labels, lengths or generations")
        contains(enter(legacy, "file read legacy-admin"), "original admin bytes")
        contains(enter(legacy, "file read legacy-user"), "original user bytes")
        enter(legacy, "file append legacy-user -v2")
        contains(enter(legacy, "file read legacy-user"), "original user bytes-v2")
        legacy.close()
        upgraded_boot = Machine("advanced-upgraded", deadline, disk=old_disk)
        machines.append(upgraded_boot)
        upgraded_boot.start()
        version = rows(upgraded_boot, "file status")
        if version[0].get("version") != 2 or version[0].get("readonly") is not False:
            raise AssertionError("Upgraded disk reverted to legacy mode after reboot")
        final_metadata = rows(upgraded_boot, "file list")
        if {row["name"]: (row["label"], row["generation"]) for row in final_metadata} != {"legacy-admin": ("admin", 7), "legacy-user": ("user", 12)}:
            raise AssertionError(f"Upgraded disk lost labels or post-upgrade mutation generations: {final_metadata!r}")
        contains(enter(upgraded_boot, "file read legacy-admin"), "original admin bytes")
        contains(enter(upgraded_boot, "file read legacy-user"), "original user bytes-v2")
        rows(upgraded_boot, "file check")
        checked("Legacy v1 mounts read-only with bytes and labels intact; explicit upgrade enables v2 writes and survives reboot")
    except Exception as error:
        failure = str(error)
    finally:
        for machine in machines:
            machine.close()
        if image.is_file() and hashlib.sha256(image.read_bytes()).hexdigest() != image_hash:
            changed = "The boot image changed while advanced integration tests were running"
            failure = f"{failure}; {changed}" if failure else changed
        transcript = bytearray()
        for machine in machines:
            transcript += f"\n--- QEMU {machine.name} ---\n".encode("ascii") + machine.output
        (OUT / "advanced-serial.txt").write_bytes(transcript)
        elapsed = time.monotonic() - beginning
        report = ["Tane OS 0.9 heap, process-control and storage QEMU integration test", f"Image SHA256: {image_hash}", f"Elapsed: {elapsed:.2f}s"]
        report += [f"Transport ({machine.name}): {machine.transport}" for machine in machines]
        report += [f"PASS: {name}" for name in checks]
        report.append(f"FAIL: {failure}" if failure else "RESULT: PASS")
        (OUT / "advanced-summary.txt").write_text("\n".join(report) + "\n", encoding="utf-8")
        disks.cleanup()
    if failure:
        print(f"FAIL {failure}", file=sys.stderr)
        return 1
    print(f"All {len(checks)} advanced integration checks passed in {elapsed:.2f}s.")
    return 0


if __name__ == "__main__":
    sys.exit(main())
