#!/usr/bin/env python3
"""Build original freestanding Rust userspace and bounded .tane files."""
import os
from pathlib import Path
import struct
import subprocess

ROOT = Path(__file__).resolve().parent.parent
OUT = ROOT / "build" / "users"
PROGRAMS = ("hello", "echo", "busy", "sleep", "isolate", "files", "probe", "heap", "control")

def run(*args):
    subprocess.run([str(arg) for arg in args], cwd=ROOT, check=True)

def main():
    OUT.mkdir(parents=True, exist_ok=True)
    rustc = os.environ.get("RUSTC", "rustc")
    ld = os.environ.get("LD", "ld")
    objcopy = os.environ.get("OBJCOPY", "objcopy")
    nm = os.environ.get("NM", "nm")
    for name in PROGRAMS:
        archive, elf = OUT / f"{name}.a", OUT / f"{name}.elf"
        run(rustc, "--edition=2021", "--crate-name", f"tane_user_{name}", "--crate-type", "staticlib",
            "--target", "x86_64-unknown-none", "-C", "opt-level=z", "-C", "panic=abort",
            "-C", "relocation-model=static", "-C", "code-model=small", "-C", "overflow-checks=yes",
            "-C", "debuginfo=0", "-o", archive, ROOT / "users" / f"{name}.rs")
        run(ld, "-m", "elf_x86_64", "--gc-sections", "-nostdlib", "-T", ROOT / "users/user.ld", "-o", elf, archive)
        symbols = {}
        for line in subprocess.check_output([nm, "-n", str(elf)], text=True).splitlines():
            parts = line.split()
            if len(parts) == 3:
                symbols[parts[2]] = int(parts[0], 16)
        unresolved = subprocess.check_output([nm, "-u", str(elf)], text=True).strip()
        if unresolved:
            raise SystemExit(f"{name}: unresolved user symbols: {unresolved}")
        code_path, data_path = OUT / f"{name}.code", OUT / f"{name}.data"
        run(objcopy, "-O", "binary", "--only-section=.code", elf, code_path)
        run(objcopy, "-O", "binary", "--only-section=.data", elf, data_path)
        code, data = code_path.read_bytes(), data_path.read_bytes()
        bss = symbols["__bss_end"] - symbols["__data_end"]
        entry = symbols["_start"] - symbols["__code_start"]
        header = struct.pack("<8sHHHHIIII", b"TANEEXE\0", 1, 0, 32, 0, len(code), len(data), bss, entry)
        executable = header + code + data
        if not (0 < len(code) <= 4096 and len(data) + bss <= 4096 and len(executable) <= 4096 and entry < len(code)):
            raise SystemExit(f"{name}: executable exceeds Tane image bounds")
        (OUT / f"{name}.tane").write_bytes(executable)
        print(f"User {name}: {len(executable)} bytes (code {len(code)}, data {len(data)}, BSS {bss})", flush=True)

if __name__ == "__main__":
    main()
