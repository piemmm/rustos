//! CCOMPAT stage CC5 fixture: the Rust startup/runtime shim the end-to-end C
//! program links against.
//!
//! The C program (`csrc/main.c`) is compiled by `clang` into a freestanding
//! relocatable object that references crt0's `_start`, the `ros_sys_*` syscall
//! stubs, and the compiler-emitted stack-canary symbols. None of those are in
//! the C object — they live in the curated *System runtime / C ABI* class
//! (`lib/crt0` + `lib/abi-sys`, `AGENTS.md` §16.4). This crate is built as a
//! `staticlib`, so that runtime is bundled into one `.a` the consuming QEMU
//! test (`tests/integration/c_program_qemu_riscv64`) links with the C object
//! to produce a single PIE image — exactly the linking a non-Rust app does
//! against the OS-provided shared library, but statically for the test
//! fixture.
//!
//! The crate names the two runtime crates only for their link-time side effect
//! (`extern crate`): rustc emits a dependency's archive members only when the
//! crate is referenced, so without these the crt0 `_start` and the `ros_sys_*`
//! stubs would be dropped from the static archive. The link script's
//! `ENTRY(_start)` then roots the trampoline.
//!
//! On the host (`cargo build --workspace`, clippy, fmt) the crate is an inert
//! empty archive so the workspace tooling still covers it; the runtime body
//! compiles only for the three native Tier-1 targets (`target_os = "none"`).

#![cfg_attr(target_os = "none", no_std)]
#![deny(missing_docs)]

// --- Freestanding runtime (native Tier-1 targets) -----------------------
#[cfg(target_os = "none")]
mod runtime {
    // Pull crt0 (`_start`, `__stack_chk_guard`/`__stack_chk_fail`) and the
    // `ros_sys_*` syscall stubs into the static archive. Named solely for the
    // link-time side effect, so the rust-2018 "unused extern crate" idiom lint
    // does not apply.
    #[allow(unused_extern_crates)]
    extern crate rustos_abi_sys;
    #[allow(unused_extern_crates)]
    extern crate rustos_crt0;

    /// Exit code used if the program ever panics. A hosted program has no
    /// unwinder, so a panic is unrecoverable; terminate through the `exit`
    /// syscall rather than returning into corrupt state (`AGENTS.md` §2.9 —
    /// fail closed). The C program is panic-free; this only satisfies the
    /// `no_std` contract for the Rust crates linked in.
    const EXIT_RUNTIME_PANIC: i32 = 70;

    /// Panic handler for the freestanding image.
    #[panic_handler]
    fn panic(_info: &core::panic::PanicInfo<'_>) -> ! {
        rustos_abi_sys::sys_exit(EXIT_RUNTIME_PANIC)
    }
}
