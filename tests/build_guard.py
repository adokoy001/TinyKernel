#!/usr/bin/env python3
"""Check the kernel's integer-only build gate against real assembled code.

User probes embedded in read-only data may use unsupported register state.
Executable kernel instructions must fail the gate before an image is written.
"""
import importlib.util
import os
from pathlib import Path
import shutil
import subprocess
import tempfile
import unittest

ROOT = Path(__file__).resolve().parents[1]


class IntegerOnlyBuildGuard(unittest.TestCase):
    @classmethod
    def setUpClass(cls):
        cls.assembler = os.environ.get("AS", "as")
        cls.objdump = os.environ.get("OBJDUMP", "objdump")
        for executable in (cls.assembler, cls.objdump):
            if not shutil.which(executable):
                raise RuntimeError(f"Missing build-guard test tool: {executable}")
        spec = importlib.util.spec_from_file_location("tane_build_guard", ROOT / "build.py")
        cls.build = importlib.util.module_from_spec(spec)
        spec.loader.exec_module(cls.build)

    def assembled(self, instructions, rodata=""):
        directory = tempfile.TemporaryDirectory(prefix="tane-build-guard-")
        self.addCleanup(directory.cleanup)
        source = Path(directory.name) / "probe.S"
        binary = Path(directory.name) / "probe.o"
        source.write_text(".intel_syntax noprefix\n.section .text,\"ax\"\n"
                          ".global probe\nprobe:\n" + instructions + "\nret\n"
                          ".section .rodata,\"a\"\n" + rodata + "\n"
                          ".section .note.GNU-stack,\"\",@progbits\n",
                          encoding="utf-8")
        subprocess.run([self.assembler, "--64", "-o", str(binary), str(source)],
                       check=True, stdout=subprocess.PIPE, stderr=subprocess.PIPE)
        return binary

    def rejected(self, instructions):
        binary = self.assembled(instructions)
        with self.assertRaises(SystemExit) as failure:
            self.build.integer_only_kernel(binary, self.objdump)
        self.assertNotEqual(failure.exception.code, 0)

    def test_integer_code_accepts_embedded_user_state_probes(self):
        binary = self.assembled("xor eax, eax\nadd eax, 1\nmov [rsp - 8], rax",
                                "pxor xmm0, xmm0\nfninit\nxsave [rax]")
        self.build.integer_only_kernel(binary, self.objdump)

    def test_executable_sse_is_rejected(self):
        self.rejected("pxor xmm0, xmm0")

    def test_executable_x87_is_rejected(self):
        self.rejected("fninit\nfld1\nfstp st(0)")

    def test_executable_mxcsr_state_is_rejected(self):
        self.rejected("ldmxcsr [rax]")

    def test_executable_xsave_is_rejected(self):
        self.rejected("xsave [rax]")

    def test_executable_xrstor_is_rejected(self):
        self.rejected("xrstor [rax]")

    def test_prefixed_executable_fxsave_is_rejected(self):
        self.rejected("data16 fxsave [rax]")


if __name__ == "__main__":
    unittest.main(verbosity=2)
