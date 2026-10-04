# Tane userspace

These are real `no_std` Rust programs running at x86-64 CPL3. They use the
original Tane `int 0x80` ABI and link without libc, external crates, or Linux.
`python3 build.py` builds these programs before embedding them in the kernel;
`python3 users/build.py` builds only the userspace artifacts.

Each executable uses one RX code page at `0x40000000`, one RW/NX data page at
`0x40001000`, and two RW/NX stack pages at `0x40004000..0x40005fff`. The gaps and
the page after the stack are unmapped. Every process has different physical
frames at these same virtual addresses. `_start` receives an argument pointer
in RDI and its length in RSI, at most 128 bytes. RSP starts with the System V
function-entry alignment (`RSP % 16 == 8`). Programs terminate through syscall
0 rather than returning.

## Executable format

The `.tane` file consists of this 32-byte little-endian header, code bytes, and
initialized data bytes, with no padding or trailing data. The entire file must
fit TaneFS's 4096-byte file limit. The loader copies data and zeroes BSS into the
data page; it also zeroes all newly allocated pages.

| Offset | Size | Field | Accepted value |
| --- | --- | --- | --- |
| 0 | 8 | Magic | `TANEEXE\0` |
| 8 | 2 | Version | 1 |
| 10 | 2 | Flags | 0 |
| 12 | 2 | Header size | 32 |
| 14 | 2 | Reserved | 0 |
| 16 | 4 | Code length | 1–4096 bytes |
| 20 | 4 | Initialized data length | 0–4096 bytes |
| 24 | 4 | BSS length | Data + BSS ≤4096 bytes |
| 28 | 4 | Entry offset | Strictly below code length |

There are no relocations, dynamic linking, or variable address mappings.
`src/executable.rs` validates the complete header and exact section lengths
before memory allocation or execution.

## Syscall ABI

RAX contains the syscall number; RDI, RSI, and RDX contain its arguments. RAX
returns a signed 64-bit result; the other registers survive `int 0x80`.
Negative results are errors. All pointers refer to the current process's
mapped user pages; the kernel validates complete ranges before copying.

| Number | Operation | Arguments | Success result |
| --- | --- | --- | --- |
| 0 | Exit | status | Does not return |
| 1 | Queued stdout | pointer, length ≤256 | Bytes queued |
| 2 | Process ID | — | ID |
| 3 | Yield | — | 0 |
| 4 | Sleep | milliseconds ≤60000 | 0 after waking |
| 5 | Open file | name pointer, name length, rights | Capability token |
| 6 | Read file | token, destination pointer, length ≤256 | Bytes read |
| 7 | Append file | token, source pointer, length ≤256 | Bytes appended |
| 8 | Close | token | 0 |
| 9 | Unlink file | name pointer, name length | 0 |
| 10 | Timer ticks | — | Ticks |
| 11–13 | Privileged format/audit/network probes | — | Always denied and audited |

Open rights are READ=1 and WRITE=2. WRITE creates a missing file; it does not
truncate an existing file. Four capabilities are available per process. Tokens
are positive, belong to one process, and are never reused during a boot.
Capabilities pin the target's slot, label, size, generation, checksum and
boot-local mutation epoch. Appending updates
the writer's pin; another capability to the older generation becomes stale.
Each read capability maintains its own offset.

Errors used by these examples include EINVAL=-22, EFAULT=-14, EACCES=-13,
EBADF=-9, ESTALE=-116, ENOSYS=-38, ENOSPC=-28, EIO=-5, ENOENT=-2,
EOVERFLOW=-75, and EAGAIN=-11. Stdout retains at most 1024 bytes over a process's
lifetime. A write that does not fit is refused in full with EAGAIN and marks
the result; bytes from successful earlier writes remain intact. Shell
`proc output` and `proc wait` copy the retained output without draining it,
so reading output does not make room for later writes. Programs never write
asynchronously into the shell's terminal. The kernel keeps the latest eight
completed outcomes, including their bounded output, until replaced or rebooted.

Processes always run in the User domain, including when Admin starts them.
Each charges 12 User frames: four page tables, code, data, two user stack pages
and four kernel stack pages. The 24-frame User limit permits at most two live
processes when no other User frame allocations consume that quota. File MAC
is domain-based; these processes share User-labelled files. A capability's
PID scope protects the token, not a private per-process filesystem.

## Built-in programs

| Program | Argument | Behavior |
| --- | --- | --- |
| `hello` | — | Prints a greeting and its process ID, then exits |
| `echo` | Text | Prints the exact argument bytes and a newline |
| `busy` | — | Infinite user loop; requires timer preemption |
| `sleep` | Milliseconds | Sleeps, prints its wakeup, and exits |
| `isolate` | Integer seed | Checks zeroed data and retains its private sentinel across 20 ticks of preemption |
| `files` | Filename | Appends and reads `written by Rust ring3` through capabilities |
| `files` | `badptr` | Rejects a kernel source pointer without appending |
| `files` | `rights` | Rejects writes through READ-only capabilities and reads after close |
| `files` | `stale` | Rejects an old generation and a closed token after its slot is reused |
| `files` | `hold` | Prints a capability token, then holds it during a five-second sleep |
| `files` | `steal TOKEN` | Rejects another process's capability token |
| `probe` | `badptr` | Tests kernel/null/cross-page/overflow pointers, excessive length, and an unknown syscall |
| `probe` | `denied` | Tests denied privileged syscall numbers 11–13 |

`probe` also deliberately faults with `kernel-read`, `kernel-write`,
`kernel-data`, `kernel-data-write`, `vga`, `vga-read`, `code-write`, `nx`,
`stack-nx`, `null`, `guard`, `stack-end`, `cli`, `hlt`, `in`, `out`, `int48`,
`sse`, `x87`, `syscall`, `sysenter`, `fsgsbase`, `xsave`, or `ud2`.
IN and OUT target low I/O port `0x20`. These cases must
terminate only the offending process. The first version disables FPU/SIMD
for user programs; `sse` and `x87` verify the hardware #NM boundary. All other
ordinary program instructions are integer-only. Alternate `SYSCALL` and
`SYSENTER` entries are disabled; `syscall` must raise #UD, and `sysenter` raises
#GP or #UD depending on the CPU. `WRFSBASE` and `XSAVE` are disabled through
CR4 and their corresponding probes must raise #UD. The `xsave` probe supplies
a page-aligned writable destination, so its expected fault tests the disabled
instruction rather than an invalid destination.
