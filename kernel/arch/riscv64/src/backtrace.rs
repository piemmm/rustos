//! riscv64 post-mortem CPU-state capture.
//!
//! Implements the Arch HAL
//! [`tairix_arch_api::CpuStateCapture`] surface for
//! riscv64: a read-only register snapshot and the frame-pointer layout the
//! neutral unwinder in `kernel/core` follows.
//!
//! # Frame layout
//!
//! With frame pointers forced (`.cargo/config.toml` carries
//! `-C force-frame-pointers=yes` for `riscv64gc-unknown-none-elf`), a
//! function's prologue stores the return address `ra` and the caller's
//! frame pointer `s0` at the top of its frame and sets `s0` (the frame
//! pointer, `x8`) to just above them. The RISC-V convention places the
//! pair immediately below `s0`:
//!
//! * the return address is at `[s0 - 8]`,
//! * the caller's saved `s0` is at `[s0 - 16]`.
//!
//! # Stack bounds
//!
//! The bootstrap hart runs on the linker-reserved boot stack
//! (`__boot_stack_bottom .. __boot_stack_top` in `boot.s`).
//! `stack_bounds` returns those bounds when the captured `sp` lies
//! within them and `None` otherwise, so the unwinder degrades to
//! registers + program counter on a stack the port cannot vouch for
//! rather than reading memory that might be unmapped (fail closed — never
//! a fault inside the fault handler).

use tairix_arch_api::{
    Backtrace, BacktraceProfile, CpuStateCapture, FrameLayout, RegisterSnapshot, StackBounds,
};

#[cfg(all(target_arch = "riscv64", target_os = "none"))]
extern "C" {
    /// Lowest address of the boot-hart boot stack (see `boot.s`).
    static __boot_stack_bottom: u8;
    /// Exclusive top of the boot-hart boot stack (see `boot.s`).
    static __boot_stack_top: u8;
}

/// riscv64 implementation of the Arch HAL post-mortem-capture surface.
///
/// Zero-sized: capturing registers needs no per-instance state.
#[derive(Debug, Default, Clone, Copy)]
pub struct Backtracer;

impl Backtracer {
    /// Construct the riscv64 post-mortem-capture handle.
    #[must_use]
    pub const fn new() -> Self {
        Self
    }

    /// The RISC-V frame-pointer layout (see the module docs).
    pub const LAYOUT: FrameLayout = FrameLayout {
        saved_fp_offset: -16,
        return_addr_offset: -8,
    };

    /// The honest declaration for riscv64: both capabilities supported.
    #[must_use]
    pub const fn declared_profile() -> BacktraceProfile {
        BacktraceProfile {
            register_capture: Backtrace::Supported,
            frame_unwind: Backtrace::Supported,
        }
    }
}

impl CpuStateCapture for Backtracer {
    fn profile(&self) -> BacktraceProfile {
        Self::declared_profile()
    }

    fn frame_layout(&self) -> Option<FrameLayout> {
        Some(Self::LAYOUT)
    }

    #[cfg(all(target_arch = "riscv64", target_os = "none"))]
    fn capture(&self) -> RegisterSnapshot {
        let (pc, sp, fp, ra): (u64, u64, u64, u64);
        let (a0, a1, a2, a3): (u64, u64, u64, u64);
        let (a4, a5, a6, a7): (u64, u64, u64, u64);
        // SAFETY: `auipc rd, 0` yields the current PC; `mv` copies `sp`/a
        // GP register into an output operand. None writes memory, changes
        // flags, or faults. Reading `s0`/`sp` does not clobber them, so the
        // frame-pointer build's reservation of `s0` is respected.
        unsafe {
            core::arch::asm!(
                "auipc {pc}, 0",
                "mv {sp}, sp",
                "mv {fp}, s0",
                "mv {ra}, ra",
                pc = out(reg) pc,
                sp = out(reg) sp,
                fp = out(reg) fp,
                ra = out(reg) ra,
                options(nomem, nostack, preserves_flags),
            );
            core::arch::asm!(
                "mv {a0}, a0",
                "mv {a1}, a1",
                "mv {a2}, a2",
                "mv {a3}, a3",
                a0 = out(reg) a0,
                a1 = out(reg) a1,
                a2 = out(reg) a2,
                a3 = out(reg) a3,
                options(nomem, nostack, preserves_flags),
            );
            core::arch::asm!(
                "mv {a4}, a4",
                "mv {a5}, a5",
                "mv {a6}, a6",
                "mv {a7}, a7",
                a4 = out(reg) a4,
                a5 = out(reg) a5,
                a6 = out(reg) a6,
                a7 = out(reg) a7,
                options(nomem, nostack, preserves_flags),
            );
        }
        RegisterSnapshot::new(pc, sp, fp)
            .with("ra", ra)
            .with("sp", sp)
            .with("s0", fp)
            .with("a0", a0)
            .with("a1", a1)
            .with("a2", a2)
            .with("a3", a3)
            .with("a4", a4)
            .with("a5", a5)
            .with("a6", a6)
            .with("a7", a7)
    }

    #[cfg(not(all(target_arch = "riscv64", target_os = "none")))]
    fn capture(&self) -> RegisterSnapshot {
        // Off-target (host unit tests): the real capture is the
        // target-gated asm path above; the QEMU panic vertical proves the
        // on-target capture is non-trivial.
        RegisterSnapshot::new(0, 0, 0)
    }

    #[cfg(all(target_arch = "riscv64", target_os = "none"))]
    fn stack_bounds(&self) -> Option<StackBounds> {
        let sp: u64;
        // SAFETY: reading `sp` into an output operand has no side effects
        // and cannot fault.
        unsafe {
            core::arch::asm!("mv {}, sp", out(reg) sp, options(nomem, nostack, preserves_flags));
        }
        // SAFETY: taking the address of the extern boot-stack symbols is a
        // link-time constant; we never dereference them.
        let low = core::ptr::addr_of!(__boot_stack_bottom) as u64;
        let high = core::ptr::addr_of!(__boot_stack_top) as u64;
        StackBounds::enclosing(sp, low, high)
    }

    #[cfg(not(all(target_arch = "riscv64", target_os = "none")))]
    fn stack_bounds(&self) -> Option<StackBounds> {
        None
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use tairix_arch_api::backtrace::conformance;

    #[test]
    fn passes_backtrace_conformance() {
        conformance::run_all(&Backtracer::new());
        let dynamic: &dyn CpuStateCapture = &Backtracer::new();
        conformance::run_all(dynamic);
    }

    #[test]
    fn declared_profile_is_honest_and_release_ready() {
        let p = Backtracer::new().profile();
        assert_eq!(p.validate(), Ok(()));
        assert!(matches!(p.register_capture, Backtrace::Supported));
        assert!(matches!(p.frame_unwind, Backtrace::Supported));
    }

    #[test]
    fn frame_layout_matches_riscv_convention() {
        let l = Backtracer::new().frame_layout().expect("supported");
        assert_eq!(l.saved_fp_offset, -16);
        assert_eq!(l.return_addr_offset, -8);
    }
}
