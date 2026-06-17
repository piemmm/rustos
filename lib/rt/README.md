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
