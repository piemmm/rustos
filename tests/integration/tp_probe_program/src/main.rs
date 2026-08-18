//! Adversarial riscv64 U-mode fixture: write a hostile value into the psABI
//! thread pointer (`tp`, x4) before every syscall, and check the program's own
//! value survives each trap.
//!
//! `tp` is simultaneously the RISC-V thread pointer U-mode code may write
//! freely and the riscv64 kernel port's per-hart identity anchor
//! (`tairix_arch_riscv64::smp::current_hartid`). A trap vector that let the
//! U-mode value survive into the handler would let this program name a
//! *different* hart and steer the kernel onto that core's per-CPU state. The
//! consuming vertical (`tests/integration/tp_isolation_qemu_riscv64`) asserts,
//! from inside the syscall dispatch callback, that the kernel still reads its
//! own hart identity while this program is shouting a hostile one — and that
//! the program gets its own value back on return, which is what makes the
//! register usable as a per-thread pointer at all.
//!
//! Two rounds with different sentinels run so a vector that merely *zeroed*
//! `tp` on entry, or that restored a stale value, fails too.
//!
//! It is a **pure-Rust** program: it links the Rust userland runtime
//! `tairix-rt` (which provides `_start`, the stack canary, the panic handler,
//! and the syscall wrappers), never the C ABI. It is built
//! position-independent and converted to an `rxe` blob by the consuming test's
//! build script. On the host, and on any target that is not freestanding
//! riscv64, it is an inert stub so `cargo build --workspace`, clippy, and fmt
//! still cover the crate.

#![cfg_attr(tp_probe, no_std)]
#![cfg_attr(tp_probe, no_main)]
#![deny(missing_docs)]

// --- Adversarial riscv64 program ----------------------------------------
#[cfg(tp_probe)]
mod program {
    /// First hostile thread-pointer value. A plausible small hart id in its low
    /// bits (`1`) so a kernel that truncated it to a `CpuId` would resolve a
    /// *real* sibling CPU rather than an unmapped one that falls back safely,
    /// with high bits set so a partial save is still visible.
    const SENTINEL_ONE: u64 = 0xDEAD_BEEF_0000_0001;

    /// Second hostile value, distinct in every byte from [`SENTINEL_ONE`], so a
    /// vector that restored a stale or zeroed `tp` fails the second round.
    const SENTINEL_TWO: u64 = 0x0123_4567_89AB_CDEF;

    /// Exit code for a `tp` value the kernel failed to give back.
    const EXIT_TP_CLOBBERED: i32 = 71;

    /// Overwrite the thread pointer with `value`.
    ///
    /// The one instruction with no architecture-neutral spelling: `tp` is a
    /// fixed psABI register, not something Rust can name. This is exactly the
    /// hostile act the vertical exists to defend against, so the fixture must
    /// be able to perform it.
    fn set_tp(value: u64) {
        // SAFETY: writing `tp` sets a plain unprivileged integer register.
        // Nothing in this program (nor in `tairix-rt`, which uses no
        // thread-local storage) reads `tp`, so clobbering it cannot corrupt the
        // program's own state; it only makes the register hostile from the
        // kernel's point of view. The write has no memory effects and cannot
        // fault.
        unsafe {
            core::arch::asm!("mv tp, {}", in(reg) value, options(nomem, nostack, preserves_flags));
        }
    }

    /// Read the thread pointer back.
    fn tp() -> u64 {
        let value: u64;
        // SAFETY: reading `tp` has no side effects and cannot fault.
        unsafe {
            core::arch::asm!("mv {}, tp", out(reg) value, options(nomem, nostack, preserves_flags));
        }
        value
    }

    /// Poison `tp` with `sentinel`, trap into the kernel, and report whether
    /// the program's own value came back intact.
    fn round(sentinel: u64) -> bool {
        set_tp(sentinel);
        // Any syscall will do; `yield_now` is unprivileged, takes no pointer
        // argument, and returns to the next instruction, so the whole round
        // trip is one clean U->S->U transition.
        tairix_rt::yield_now();
        tp() == sentinel
    }

    /// Program entry point. `tairix-rt`'s `_start` calls it once the runtime is
    /// set up and routes its return value through the `exit` syscall.
    fn main() -> i32 {
        if !round(SENTINEL_ONE) || !round(SENTINEL_TWO) {
            return EXIT_TP_CLOBBERED;
        }
        0
    }

    tairix_rt::entry!(main);
}

// --- Host stub ----------------------------------------------------------
//
// On the host (`cargo build --workspace`, clippy, fmt) and on every target that
// is not freestanding riscv64, the program body is not compiled, so this inert
// `main` keeps the crate building under the host tooling. It performs no I/O.
#[cfg(not(tp_probe))]
fn main() {}
