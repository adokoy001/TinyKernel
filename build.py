#!/usr/bin/env python3
"""Build an original floppy-bootable kernel using standard, offline tools."""
import hashlib
import os
from pathlib import Path
import shutil
import subprocess
import sys

ROOT = Path(__file__).resolve().parent
OUT = ROOT / "build"
TARGET = "x86_64-unknown-none"
KERNEL_SECTORS = 256
FLOPPY_BYTES = 1_474_560


def run(*args):
    print("+", " ".join(str(arg) for arg in args), flush=True)
    subprocess.run([str(arg) for arg in args], cwd=ROOT, check=True)


def main():
    tools = {name: os.environ.get(name.upper(), name) for name in ("rustc", "as", "ld", "objcopy", "nm")}
    for name, executable in tools.items():
        if not shutil.which(executable):
            sys.exit(f"Missing {name}. See README.md for setup.")
    core_dir = subprocess.check_output([tools["rustc"], "--print", "target-libdir", "--target", TARGET], text=True).strip()
    if not list(Path(core_dir).glob("libcore-*.rlib")):
        sys.exit("Missing bare-metal Rust target. Run: rustup target add x86_64-unknown-none")
    OUT.mkdir(exist_ok=True)
    run(tools["as"], "--64", "-o", OUT / "boot.o", ROOT / "boot.S")
    run(tools["ld"], "-m", "elf_x86_64", "-T", ROOT / "boot.ld", "-o", OUT / "boot.elf", OUT / "boot.o")
    run(tools["objcopy"], "-O", "binary", OUT / "boot.elf", OUT / "boot.bin")
    run(tools["rustc"], "--edition=2021", "--crate-name", "tane_kernel", "--crate-type", "staticlib",
        "--target", TARGET, "-C", "opt-level=s", "-C", "panic=abort", "-C", "relocation-model=static",
        "-C", "code-model=small", "-C", "overflow-checks=yes", "-C", "debuginfo=0",
        "-o", OUT / "kernel.a", ROOT / "src/main.rs")
    run(tools["ld"], "-m", "elf_x86_64", "--gc-sections", "-nostdlib", "-T", ROOT / "kernel.ld",
        "-Map", OUT / "kernel.map", "-o", OUT / "kernel.elf", OUT / "kernel.a")
    run(tools["objcopy"], "-O", "binary", OUT / "kernel.elf", OUT / "kernel.bin")
    boot = (OUT / "boot.bin").read_bytes()
    kernel = (OUT / "kernel.bin").read_bytes()
    if len(boot) != 512 or boot[-2:] != b"\x55\xaa":
        sys.exit("Boot sector must be exactly 512 bytes, ending in 55 AA.")
    if not 0 < len(kernel) <= KERNEL_SECTORS * 512:
        sys.exit(f"Kernel is {len(kernel)} bytes; maximum is {KERNEL_SECTORS * 512}.")
    unresolved = subprocess.check_output([tools["nm"], "-u", str(OUT / "kernel.elf")], text=True).strip()
    if unresolved:
        sys.exit(f"Unresolved symbols: {unresolved}")
    image = bytearray(FLOPPY_BYTES)
    image[:512] = boot
    image[512:512 + len(kernel)] = kernel
    (OUT / "tane-os.img").write_bytes(image)
    version = subprocess.check_output([tools["rustc"], "--version"], text=True).strip()
    report = f"Tane OS build\n{version}\nTarget: {TARGET}\nBoot: {len(boot)} bytes\nKernel: {len(kernel)} bytes\nImage: {len(image)} bytes\nSHA256: {hashlib.sha256(image).hexdigest()}\n"
    (OUT / "build-info.txt").write_text(report, encoding="utf-8")
    print(report)


if __name__ == "__main__":
    main()
