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

There are no relocations or dynamic linking. Code, data and stack addresses
are fixed; syscall 14 can resize a bounded anonymous heap at `0x40010000`.
`src/executable.rs` validates the complete header and exact section lengths
before memory allocation or execution.

## Syscall ABI

RAX contains the syscall number; RDI, RSI, and RDX contain its arguments. RAX
returns a signed 64-bit result; the other registers survive `int 0x80`.
Negative results are errors. New calls require unused argument registers to
be zero: RSI and RDX for 14/15/17, and RDX for 16/18/20. All pointers refer to the current process's
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
| 14 | Resize anonymous heap | Desired total page count, 0–8 | Base address `0x40010000` |
| 15 | Spawn child | Pointer to 40-byte spawn request | Child PID |
| 16 | Wait for child | Positive child PID, writable 40-byte result pointer | Child PID after completion |
| 17 | Kill child | Own running child PID | 0 |
| 18 | Seek file | token, offset 0–4096 | Offset |
| 19 | Write at cursor | token, source pointer, length ≤256 | Bytes written |
| 20 | Truncate file | token, size 0–4096 | Size |

The heap begins unmapped. Each heap page is independent RW/NX memory owned
by the process, and the maximum size is 32 KiB. Pages at and beyond
`0x40018000` remain unmapped. Resize takes a desired total count, rather than
a delta. Growth reserves and zeroes every new page before publishing any
mapping; failure preserves the previous mappings and bytes. Shrink removes
and invalidates the tail mappings before wiping and releasing their frames.
The retained prefix survives, and any regrown page starts zeroed. Resizing to
zero still returns the base address, but that address is then invalid even
for a zero-byte pointer range. The kernel does not provide `malloc` or a Rust
`GlobalAlloc` implementation. Requests over eight pages return EINVAL, domain
quota exhaustion returns EACCES and is audited, and physical exhaustion returns ENOMEM.

The spawn request is five little-endian `u64` words: name pointer, name
length (1–47), argument pointer, argument length (0–128), and kind (0 for a
builtin, 1 for a TaneFS executable). The kernel copies the request, name and
argument before filesystem effects or process allocation. A zero-length
argument still needs a mapped user pointer. Each child starts with its own
address space, empty heap and empty capability table; this is not `fork`.

Each live User parent has one outstanding child relation. Running and completed
children both occupy it until a successful wait consumes the relation; another
spawn returns EAGAIN meanwhile. Wait only accepts the exact positive child PID
and blocks the parent if necessary. The whole result span is validated before
blocking or consuming a completion; EFAULT leaves it available. Completion is
retained in the parent separately from the shell's eight-entry outcome log.

The wait result is five little-endian `u64` words: PID, kind, exit-code bits,
fault vector and fault error code. Kind 0 means exit (the code is signed i64),
1 means fault, and 2 means killed. Fields not relevant to that kind are zero.
It contains no stdout, fault address or kernel pointer. Wait returns the PID
and consumes this relation once; a repeat returns ECHILD. Kill accepts only
one's own still-running child. Shared User domain membership does not grant
wait or kill authority over unrelated PIDs. PID 0 and wait-any are unsupported.

A parent's exit, fault or kill drops its completion authority while its child
continues as an orphan. The child remains visible to the trusted shell, and
its eventual outcome follows the ordinary shell log. PIDs are not reused,
so a new process occupying the parent's old task slot cannot adopt its child.
There is no signal, process group, automatic descendant kill or orphan adoption.

Open rights are READ=1 and WRITE=2. WRITE creates a missing file; it does not
truncate an existing file. Four capabilities are available per process. Tokens
are positive, belong to one process, and are never reused during a boot.
Capabilities pin the target's slot, label, size, generation, checksum and
boot-local mutation epoch. Appending updates
the writer's pin; another capability to the older generation becomes stale.
Each read capability maintains its own offset.

Seek requires READ or WRITE and sets the per-handle cursor, including beyond
EOF. Read at EOF returns zero. Write requires WRITE, overwrites at the cursor,
zeroes any gap beyond EOF, and advances the cursor only on success. A zero-byte
write has no file or cursor effect. Truncate requires WRITE and shrinks or
zero-extends the file without changing the cursor. Successful content changes
refresh that handle's pinned identity; other handles to the old target become
stale. Append still writes at EOF, independently of this positioned-write API.

TaneFS v2 journals file mutations and replays valid committed updates on mount,
under the device's sector durability and flush contract. Legacy v1 volumes
are readable but refuse mutations until Admin explicitly runs `file upgrade`,
which preserves file contents and labels. Filesystem or journal corruption fails
closed; checksums are accidental-corruption checks, not authentication.
See [storage details](../docs/storage-v2.md).

Errors used by these examples include ENOMEM=-12, ECHILD=-10, EROFS=-30, EINVAL=-22, EFAULT=-14, EACCES=-13,
EBADF=-9, ESTALE=-116, ENOSYS=-38, ENOSPC=-28, EIO=-5, ENOENT=-2,
EOVERFLOW=-75, and EAGAIN=-11. Stdout retains at most 1024 bytes over a process's
lifetime. A write that does not fit is refused in full with EAGAIN and marks
the result; bytes from successful earlier writes remain intact. Shell
`proc output` and `proc wait` copy the retained output without draining it,
so reading output does not make room for later writes. Programs never write
asynchronously into the shell's terminal. The kernel keeps the latest eight
completed outcomes, including their bounded output, until replaced or rebooted.

Processes always run in the User domain, including when Admin starts them.
Each starts with 12 User frames: four page tables, code, data, two user stack
pages and four kernel stack pages. Up to eight heap frames are charged to the
same domain. The 24-frame User limit permits at most two live processes with
empty heaps when no other User frame allocations consume that quota. Any
heap page in one process leaves too little quota for a second 12-frame process;
child processes obey this same limit. File MAC
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
| `heap` | `basic` (default) | Checks zeroed growth, cross-page syscalls, shrink, regrowth and retained bytes |
| `heap` | `limit`, `hold N`, `quota` | Checks bounds; holds eight pages for five seconds; expects quota refusal beside a sibling |
| `heap` | `freed`, `nx`, `guard` | Faults after shrink, on heap execution, or on the page before the heap |
| `control` | `basic` (default), `live`, `exit`, `fault`, `kill` | Spawns a child and checks normal, delayed, exit 37, fault and killed outcomes |
| `control` | `file NAME`, `deniedfile NAME` | Spawns a TaneFS child; checks refusal of an Admin-only executable |
| `control` | `foreign PID` | Checks wait/kill refusal against a live unrelated User process |
| `control` | `validate`, `quota`, `pending`, `orphan` | Checks malformed/foreign/consumed requests; quota failure and retry; retained completion; a child surviving its parent |
| `files` | Filename | Appends and reads `written by Rust ring3` through capabilities |
| `files` | `position` | Checks seek/write/truncate, zero-filled gaps, cursor behavior, stale tokens and rights |
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
terminate only the offending process. The current ABI disables FPU/SIMD
for user programs; `sse` and `x87` verify the hardware #NM boundary. All other
ordinary program instructions are integer-only. Alternate `SYSCALL` and
`SYSENTER` entries are disabled; `syscall` must raise #UD, and `sysenter` raises
#GP or #UD depending on the CPU. `WRFSBASE` and `XSAVE` are disabled through
CR4 and their corresponding probes must raise #UD. The `xsave` probe supplies
a page-aligned writable destination, so its expected fault tests the disabled
instruction rather than an invalid destination.
