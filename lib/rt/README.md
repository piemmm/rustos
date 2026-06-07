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
    rustos_rt::console_write(b"hello\n");
    0
}

rustos_rt::entry!(main);
```

`_start` validates the kernel-supplied startup vector, installs the
per-process stack canary (`AGENTS.md` §19.2), calls `main`, and routes its
return value through the `exit` syscall.

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
exposed syscall-wrapper surface grows as RustOS programs need it; today it is
`console_write`, `yield_now`, and `exit`.
