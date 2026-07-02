# rustos-rt

The pure-Rust userland runtime: the `_start` entry trampoline, idiomatic
`abi-v1` syscall wrappers, the `entry!` macro, the per-process stack-canary
symbols, and the panic handler that a **first-party RustOS program written in
Rust** links. RustOS is Rust-only (`AGENTS.md` §1), so its own programs use
this runtime.

## Relationship to the C ABI (`crt0` + `abi-sys`)

`rustos-crt0` and `rustos-abi-sys` are the curated *System runtime / C ABI*
class (`AGENTS.md` §9, §16.4): a libc-equivalent that exists **solely** so a
program **not** written in Rust (C, …) can call `abi-v1`. They are not for
RustOS's own code. `rustos-rt` is the Rust counterpart. Both build on the one
shared syscall trap (`rustos-abi-trap`, `AGENTS.md` §2.2), so the trap assembly
is not duplicated, and neither is a privileged path — every capability and
input check happens kernel-side (`AGENTS.md` §5.4).

## Using it

A program is `#![no_std]`, `#![no_main]`, declares its `main`, and hands it to
`entry!`:

```rust
#![no_std]
#![no_main]

fn main() -> i32 {
    rustos_rt::stdout(b"hello\n");
    0
}

rustos_rt::entry!(main);
```

`_start` validates the kernel-supplied startup vector, installs the
per-process stack canary (`AGENTS.md` §19.2), calls `main`, and routes its
return value through the `exit` syscall.

## Heap (`#[global_allocator]`)

On the three native targets `rustos-rt` registers a `#[global_allocator]`
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

`rustos_rt::io` is the ergonomic `std::io`-style layer a program programs
against instead of hand-marshalling byte slices: one fd-generic `Read`/`Write`
trait pair (with looping `read_exact`/`write_all`/`write_fmt`), the buffering
built on them (`BufReader` with `read_line`/`read_until`/`lines`, `BufWriter`
coalescing small writes), and the four well-known standard streams (`Stdin`,
`Stdout`, `Stderr`, `StdInfo`) plus a non-owning `Stream` over any inherited
descriptor. It is a pure layer over the existing `stream_read`/`stream_write`
traps — no new syscall, capability, or `lib/abi` type — so the standard streams
today and any file / pipe / tty / resource-reference fd a sibling subsystem
later opens all share one I/O vocabulary (`AGENTS.md` §2.2). `StdInfo` (fd 3)
writes are best-effort per `AGENTS.md` §20.1; every path is fail-closed, never a
panic. See `docs/src/lib/rt-io.md` and `plans/IO.md`.

## Filesystem (`File`, `Dir`)

`rustos-rt` exposes the userland filesystem surface (`PREREQUISITES.md` P-A):
thin `fs_open`/`fs_close`/`fs_read`/`fs_write`/`fs_readdir`/`fs_stat_raw`/
`fs_truncate`/`fs_sync`/`fs_mkdir`/`fs_unlink`/`fs_rename` wrappers over the
`abi-v1` syscalls, the working-directory pair (`fs_chdir`/`fs_getcwd`, against
which relative paths resolve, `.junie/PREREQUISITES2.md` P2), plus the
ergonomic `File` and `Dir` handles a program normally uses.
`File` owns its descriptor and releases it with `fs_close` on `Drop`, so a
handle is never leaked; `File::read_at` / `write_at` split a transfer larger
than `rustos_abi::FS_IO_MAX` across successive syscalls. A program names a
descriptor, never a device (`AGENTS.md` §20). Every capability, identity, and
per-inode check stays kernel-side behind the secured VFS (`AGENTS.md` §5.4); a
refusal surfaces as the raw `-errno`. The `open` / `create` / `open_dir` free
functions are the common-case openers (read-only, write+create+truncate, and
directory-listing respectively).

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
exposed syscall-wrapper surface grows as RustOS programs need it: the
standard-stream wrappers (`stdout`, `stderr`, `stdinfo`, `stdin`, `AGENTS.md`
§20), `spawn` / `spawn_at` / `console_count` / `wait` / `yield_now` / `exit`,
the anonymous-memory pair (`mem_map`, `mem_unmap`) and the `mem_map`-backed
`#[global_allocator]` they power, the resource-limit pair (`rlimit_get`,
`rlimit_set`), the session wrappers (`set_echo`, `users_db_read`,
`key_inject`, `keyboard_read`, `display_acquire` / `display_release`,
`ipc_send`), the user-space-driver wrappers (`mmio_map`, `dma_alloc`,
`resource_grants`), and the monotonic clock (`clock_get`) plus the
`ClockDelay` `Delay` facility (`AGENTS.md` §2.2) built on it.
