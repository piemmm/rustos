# tairix-rt

The pure-Rust userland runtime: the `_start` entry trampoline, idiomatic
`abi-v1` syscall wrappers, the `entry!` macro, the per-process stack-canary
symbols, and the panic handler that a **first-party TAIRiX program written in
Rust** links. TAIRiX is Rust-only (`AGENTS.md` §1), so its own programs use
this runtime.

## Relationship to the C ABI (`crt0` + `abi-sys`)

`tairix-crt0` and `tairix-abi-sys` are the curated *System runtime / C ABI*
class (`AGENTS.md` §9, §16.4): a libc-equivalent that exists **solely** so a
program **not** written in Rust (C, …) can call `abi-v1`. They are not for
TAIRiX's own code. `tairix-rt` is the Rust counterpart. Both build on the one
shared syscall trap (`tairix-abi-trap`, `AGENTS.md` §2.2), so the trap assembly
is not duplicated, and neither is a privileged path — every capability and
input check happens kernel-side (`AGENTS.md` §5.4).

## Using it

A program is `#![no_std]`, `#![no_main]`, declares its `main`, and hands it to
`entry!`:

```rust
#![no_std]
#![no_main]

use tairix_rt::io::{Stdout, Write};

fn main() -> i32 {
    let _ = Stdout.write_all(b"hello\n");
    0
}

tairix_rt::entry!(main);
```

`_start` validates the kernel-supplied startup vector, installs the
per-process stack canary (`AGENTS.md` §19.2), calls `main`, and routes its
return value through the `exit` syscall.

## Heap (`#[global_allocator]`)

On the three native targets `tairix-rt` registers a `#[global_allocator]`
(`src/heap.rs`) so a first-party Rust program can use `alloc` (`Box`, `Vec`,
`String`, …). It is a free-span allocator over a single contiguous virtual
arena that grows upward, one or more whole pages at a time, by `mem_map`ping
with `MapFlags::FIXED` at the arena's current top; freed regions are tracked as
a coalesced, address-sorted free list held inside the allocator (not as
intrusive links in user memory), so every returned pointer is bounds-checked
before it is handed out (`AGENTS.md` §4). When coalescing frees whole trailing
pages they are returned to the kernel with `mem_unmap` (the arena shrinks).
`realloc` resizes in place wherever it can, avoiding the copy (`AGENTS.md`
§2.16): a shrink always succeeds in place (the surrendered tail returns to the
free list, and whole top pages are unmapped if it reaches the arena top), and a
grow succeeds in place when the bytes immediately after the block are free or
the block abuts the growable arena top. Only when neither holds does it fall
back to allocate-copy-free (copying just the overlapping prefix, leaving the
original block intact if the new allocation fails).
The free-span table is **not** a fixed-size array: it is a capacity that grows
on demand (`AGENTS.md` §24.1 "grow before you fail"). When a workload fragments
the heap past the table's current capacity, the allocator maps one more whole
metadata page (its own `SpanStore` window, distinct from the data arena) and
continues, rather than capping the workload at a hand-picked `const`. Only
genuine resource exhaustion — `mem_map` can no longer supply an arena page *or*
a metadata page — returns a null pointer per the `GlobalAlloc` contract:
deterministic OOM, never a panic (`AGENTS.md` §4 / §2.9). The kernel zeroes
pages on map and on free, so the heap does not re-zero on free; a process
reusing its own freed bytes is not a security boundary (`AGENTS.md` §2.16). The
arena and metadata bases are fixed virtual addresses documented in
`src/heap.rs`.

## I/O abstraction (`io` module)

`tairix_rt::io` is the ergonomic `std::io`-style layer a program programs
against instead of hand-marshalling byte slices: one fd-generic `Read`/`Write`
trait pair (the `read_fill`/`write_drain` transfer loops every other helper —
`read_exact`, `write_all`, `write_fmt`, and `File`'s positional helpers — is
built on), the buffering built on them (`BufReader` with
`read_line`/`read_until`/`lines`, `BufWriter` coalescing small writes), and the
four well-known standard streams (`Stdin`, `Stdout`, `Stderr`, `StdInfo`) plus
a borrowed `Stream` over any descriptor and the owning `File`. It is a pure
layer over the existing `stream_read`/`stream_write` traps — no new syscall,
capability, or `lib/abi` type — so the standard streams, opened files, pipe and
pty ends, and resource references all share one I/O vocabulary (`AGENTS.md`
§2.2).

A kernel refusal is reported as `Error::Os(Errno)` carrying the kernel's own
code, never folded into a zero-length read: `Ok(0)` means end-of-input and
nothing else, so a consumer can never silently truncate what it read because a
capability was revoked or a pipe broke. `Error::as_errno` converts back for an
interface that speaks the kernel's vocabulary. `StdInfo` (fd 3) is the one
deliberate exception — its writes are best-effort and ignorable per `AGENTS.md`
§20.1. Every path is fail-closed, never a panic. See `docs/src/lib/rt-io.md`
and `plans/IO.md`.

## Filesystem (`File`, `Dir`)

`tairix-rt` exposes the userland filesystem surface (`PREREQUISITES.md` P-A):
thin `fs_open`/`fs_close`/`fs_read`/`fs_write`/`fs_readdir`/`fs_stat_raw`/
`fs_truncate`/`fs_sync`/`fs_mkdir`/`fs_unlink`/`fs_rename`/`fs_symlink`/
`fs_readlink` wrappers over the `abi-v1` syscalls, the working-directory pair (`fs_chdir`/`fs_getcwd`, against
which relative paths resolve, `plans/SHELL.md` P2), plus the
ergonomic `File` and `Dir` handles a program normally uses.
`File` is the one **owning** descriptor handle, whatever its backing (a path, a
resource reference, a pipe end, a pty end — the close trap releases any of
them), and it releases the descriptor on `Drop`, so a handle is never leaked.
It implements `tairix_rt::io::Read`/`Write`, which read and write at the shared
open-file-description cursor: two handles cloned from one description (a spawn
wire, a delegation) walk the file together instead of each restarting it, and a
file is streamed with exactly the same code a program uses on standard input.
`File::read_at` / `write_at` are the **positional** pair — they take an explicit
offset, leave that shared cursor untouched, and split a transfer larger than
`tairix_abi::FS_IO_MAX` across successive syscalls through the same
`read_fill`/`write_drain` loop rather than a second copy of it. A program names
a descriptor, never a device (`AGENTS.md` §20). Every capability, identity, and
per-inode check stays kernel-side behind the secured VFS (`AGENTS.md` §5.4); a
refusal surfaces as the raw `-errno`. The `open` / `create` / `open_dir` free
functions are the common-case openers (read-only, write+create+truncate, and
directory-listing respectively).

`File::open` is the one **open-by-name** path, and it applies the shared
`lib/resref` spelling rule (`names_resource_reference`) before any lookup: a
filesystem path goes to `fs_open`, while a resource reference (`sys:random`,
`sys:null`, …) goes to `resource_open`, the kernel's capability-checked
resource resolver. Because the routing lives here, **every** first-party
program accepts a resource reference wherever it accepts a file name —
`cat sys:random`, `tee sys:null` — with no tool-side code (`AGENTS.md` §2.2).
A spelling that names a reference is never retried as a filesystem lookup
(the kernel resolver's refusal stands, fail closed), an on-disk name
containing `:` stays reachable as `./name`, and `File::open_resource` remains
the explicit constructor for a caller that has already classified its target
(the shell's parsed redirection targets).

## Raw syscall results (`Errno::from_syscall`)

The low-level wrappers hand back the kernel's raw signed register, so a
refusal arrives as a negated `Errno` discriminant. `Errno::from_syscall` in
`lib/abi` is the **one** place that becomes a typed error — this runtime's own
wrappers, the driver programs that issue syscalls directly, and every
application all recover their `Errno` through it, so a refusal cannot be read
one way in one program and another way in the next. It lives beside `Errno`
rather than here because a raw result reaches consumers that have no reason to
link a runtime.

Anything unrecognisable — a code this build has no variant for, a magnitude
too large for an `i32`, `i64::MIN`, or a success value handed in by mistake —
fails closed as `Errno::NotImplemented`. It deliberately never becomes
`Errno::NotFound`, which asserts a named object is absent and which callers act
on; an unreadable result must not be able to masquerade as that answer.
`Errno::try_from_syscall` is the fallible form, for the caller that must tell
an unreadable register from a real refusal instead of folding the two.

## Memory pressure (`pressure` module)

`tairix_rt::pressure` holds the process's single
[`ReportedPressure`](../reclaim/README.md) gauge (`plans/SMARTRAM.md`
SMART5): the process-wide answer to "how tight is memory" that every
`tairix_reclaim::ReclaimCache` in the program consults, so they all shrink
on the same band at the same moment. A userland process cannot measure free
frames, watermarks, or the reserve floor itself, so `pressure::gauge()`
answers `Critical` until `pressure::report(band)` tells it otherwise —
admitting nothing rather than assuming the machine is comfortable.

The runtime deliberately does not fetch the band itself: reading it needs a
System Information endpoint and a transport the runtime has no business
choosing for a program. The owning program parks on the ungated
`WaitSourceKind::MemoryPressure` wait-set member, reads the band with the
ungated `SysinfoQueryId::MEMORY_PRESSURE_BAND` query on wake, and calls
`pressure::report` — event-driven throughout, never polled (`AGENTS.md`
§2.23).

## Reporting the process's caches (`cachereport` module)

The band comes *in* through `pressure`; the figures go *out* through
`tairix_rt::cachereport`. A process's heap is its own, so nothing outside
it can measure what its glyph atlas or its decoded icon artwork is
holding — left unreported, the `disposable-ui` reclaim class would read
zero on a monitor while a desktop held megabytes of exactly that. Each
cache hands the runtime a `tairix_reclaim::CacheLedger` with
`cachereport::register`, and the runtime submits the set through the
ungated, self-scoped `SysinfoQueryId::CACHE_REPORT` operation. Being
ungated costs nothing: the process describes only itself, grants nothing,
and reads nothing, and the service stamps the kernel-attested identity
rather than believing the caller's.

The owning program drives it, for the same reason it drives `pressure`.
Once per turn of its event loop it calls `cachereport::publish_if_due()`,
which samples the registered caches and sends only when the sample
*differs* from the last one sent and the minimum interval has elapsed —
the comparison is the change detection, so there is no dirty flag to
forget. When the interval suppresses a change,
`cachereport::wait_deadline_ns()` returns the nanoseconds remaining and
the program folds that into its wait timeout, so exactly one bounded wait
is armed and only while something is genuinely pending. An idle process
arms nothing and sends nothing: its last report is still true, because a
process that is not running is not changing what it holds. There is no
timer and no poll loop on the path. `cachereport::ReportGuard`, held for the
scope in which the caches are registered, removes the process's rows on every
way out — a clean return, a fail-loud exit, a panic unwind — so a monitor never
shows memory nobody holds. Every publisher holds that one guard rather than its
own copy of the same three lines.

The report is **handed over, never awaited** (`submit` module, below). Its
publishers are a desktop compositor, a file manager and the font service, each
owing somebody a frame or an answer, so a blocking `ipc_call` here parked the
caller off the run queue for a full cross-process round trip — twice a second
per publisher — which the desktop showed as a stutter through every gesture.
The withdrawal is the one report that waits, because the kernel drops a posted
request whose poster has exited and a withdrawal that did not land would leave
a monitor showing memory nobody holds.

## Submitting a figure without waiting (`submit` module)

`tairix_rt::submit::Submission` is the one shape for a statement a process
makes about itself: `call_post` hands the request over and returns, the caller
carries on, and `call_reap` collects the verdict without blocking on a later
pass. One submission is outstanding at a time — a restatement of a figure has
nothing to say until the last one has landed — and each carries a deadline, so
a wedged or absent service costs one abandoned ticket rather than a blocked
loop. `cachereport` above and the desktop session's frame accounting both go
out over it, so neither hand-rolls the ticket bookkeeping.

## Targets

The `_start` trampoline, stack-canary symbols, and panic handler are compiled
in only for the three native Tier-1 targets (`x86_64-unknown-none`,
`aarch64-unknown-none`, `riscv64gc-unknown-none-elf`), selected by a
build-script-emitted cfg (`rt_native_<arch>`) rather than `cfg(target_arch)`
so the instruction-set choice stays out of the source the §17.2 `cfg-check`
guards. `wasm32` has no trap instruction and is out of scope
(`plans/CCOMPAT.md` §1). On the host only the syscall-wrapper marshalling is
compiled, unit-tested through the trap crate's injectable seam.

## Stability tier

`experimental` — `abi-v1` is **not** frozen yet (`plans/CCOMPAT.md` §0). The
exposed syscall-wrapper surface grows as TAIRiX programs need it: the
standard-stream wrappers (`stdout`, `stderr`, `stdinfo`, `stdin`, `AGENTS.md`
§20), `spawn` / `spawn_at` / `console_count` / `wait` / `yield_now` / `exit`,
the anonymous-memory pair (`mem_map`, `mem_unmap`) and the `mem_map`-backed
`#[global_allocator]` they power, the resource-limit pair (`rlimit_get`,
`rlimit_set`), the session wrappers (`set_input_mode`, `users_db_read`,
`key_inject`, `keyboard_read`, `display_acquire` / `display_release`,
`ipc_send`), the user-space-driver wrappers (`mmio_map`, `dma_alloc`,
`resource_grants`), and the monotonic clock (`clock_get`) plus the timed park
built on it. That park is one definition with two entry points (`AGENTS.md`
§2.2): `park_ns` for anything that already holds a nanosecond span — an
animation's next frame, a timed retry — and the `ClockDelay` `Delay` facility
for a driver's microsecond settle window. It genuinely sleeps, parking on the
process's lazily created, memberless sleep wait-set (`waitset_wait` with the
remaining window as its deadline) so the kernel's one-shot timer wakes it,
and degrades to a cooperative yield wait only if the kernel refuses a
wait-set. `park_forever` is its unbounded counterpart.

`latency_watch` is the one diagnostic wrapper: an interactive surface calls
it once with `tairix_abi::latency::DEFAULT_FRAME_BUDGET_NS` before its event
loop, and the kernel then reports any span that overruns, naming the syscall
that spent the budget and the user stack that led there
(`docs/src/architecture/latency-diagnostics.md`). It returns the budget
actually armed, which is `0` on an image that compiles the diagnostics out —
an answer rather than a failure, so a surface neither branches on the image
it runs in nor reports anything to the user.
