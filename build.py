#!/usr/bin/env python3
"""Build an original floppy-bootable kernel using standard, offline tools."""
import hashlib
import os
from pathlib import Path
import re
import shutil
import subprocess
import sys

ROOT = Path(__file__).resolve().parent
OUT = ROOT / "build"
TARGET = "x86_64-unknown-none"
FLOPPY_BYTES = 1_474_560


def kernel_sector_count():
    """Use the same bounded literal as the original BIOS read loop."""
    definitions = re.findall(r"^\s*\.equ\s+KERNEL_SECTORS\s*,\s*([0-9]+)\s*$",
        (ROOT / "boot.S").read_text(encoding="utf-8"), re.MULTILINE)
    if len(definitions) != 1:
        sys.exit("boot.S must define exactly one literal .equ KERNEL_SECTORS.")
    sectors = int(definitions[0])
    if not 1 <= sectors <= 768 or 512 + sectors * 512 > FLOPPY_BYTES:
        sys.exit("BIOS kernel load must be between 1 and 768 floppy sectors.")
    return sectors


def run(*args):
    print("+", " ".join(str(arg) for arg in args), flush=True)
    subprocess.run([str(arg) for arg in args], cwd=ROOT, check=True)


def integer_only_kernel(elf, objdump):
    """CR0.TS traps unsupported state; reject kernel instructions that need it.

    Inspect executable sections only. Bundled user probes intentionally use
    x87/SSE in rodata, which must not count as live kernel instructions.
    """
    disassembly = subprocess.check_output(
        [objdump, "-d", "--no-show-raw-insn", "-Mintel", str(elf)], text=True)
    forbidden = []
    for line in disassembly.splitlines():
        match = re.match(r"^\s*[0-9a-f]+:\s+(.+)$", line)
        if not match:
            continue
        instruction = match[1]
        tokens = instruction.lower().split()
        # objdump can print legal instruction prefixes as separate tokens,
        # e.g. data16 fxsave [rax]. They must not hide the state opcode.
        prefixes = {"data16", "addr16", "addr32", "addr64", "lock", "rep", "repz",
            "repnz", "repe", "repne", "bnd", "notrack", "cs", "ds", "es", "fs", "gs", "ss", "rex"}
        while tokens and (tokens[0] in prefixes or tokens[0].startswith("rex.")):
            tokens.pop(0)
        if not tokens:
            continue
        mnemonic = tokens[0]
        if (re.search(r"\b(?:xmm|ymm|zmm|mm)[0-9]+\b", instruction)
                or mnemonic.startswith("f")
                or mnemonic.startswith(("xsave", "xrstor"))
                or mnemonic in {"emms", "ldmxcsr", "stmxcsr", "xgetbv", "xsetbv", "vzeroall", "vzeroupper"}
                or (mnemonic.startswith("v") and mnemonic not in {"verr", "verw"})):
            forbidden.append(line.strip())
    if forbidden:
        sys.exit("Kernel requires unsupported FPU/SIMD state:\n" + "\n".join(forbidden[:12]))


def main():
    kernel_limit = kernel_sector_count() * 512
    tools = {name: os.environ.get(name.upper(), name) for name in ("rustc", "as", "ld", "objcopy", "nm", "objdump")}
    for name, executable in tools.items():
        if not shutil.which(executable):
            sys.exit(f"Missing {name}. See README.md for setup.")
    core_dir = subprocess.check_output([tools["rustc"], "--print", "target-libdir", "--target", TARGET], text=True).strip()
    if not list(Path(core_dir).glob("libcore-*.rlib")):
        sys.exit("Missing bare-metal Rust target. Run: rustup target add x86_64-unknown-none")
    OUT.mkdir(exist_ok=True)
    run(sys.executable, ROOT / "users/build.py")
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
    if not 0 < len(kernel) <= kernel_limit:
        sys.exit(f"Kernel is {len(kernel)} bytes; maximum is {kernel_limit}.")
    unresolved = subprocess.check_output([tools["nm"], "-u", str(OUT / "kernel.elf")], text=True).strip()
    if unresolved:
        sys.exit(f"Unresolved symbols: {unresolved}")
    integer_only_kernel(OUT / "kernel.elf", tools["objdump"])
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
