//! aarch64 post-mortem CPU-state capture.
//!
//! Implements the Arch HAL
//! [`tairix_arch_api::CpuStateCapture`] surface for
//! aarch64: a read-only register snapshot and the AAPCS64 frame-pointer
//! layout the neutral unwinder in `kernel/core` follows.
//!
//! # Frame layout
//!
//! With frame pointers forced (`.cargo/config.toml` carries
//! `-C force-frame-pointers=yes` for `aarch64-unknown-none`), a function's
//! prologue stores the pair `{x29, x30}` at the base of its frame and sets
//! `x29` (the frame pointer) to point at that saved pair. Relative to the
//! current `x29`:
//!
//! * the caller's saved `x29` is at `[x29 + 0]`,
//! * the return address (saved `x30` / `lr`) is at `[x29 + 8]`.
//!
//! # Stack bounds
//!
//! The bootstrap processor runs on the linker-reserved boot stack
//! (`__boot_stack_bottom .. __boot_stack_top` in `boot.s`).
//! `stack_bounds` returns those bounds when the captured `sp` lies
//! within them and `None` otherwise, so the unwinder degrades to
//! registers + program counter on a stack the port cannot vouch for
//! rather than reading memory that might be unmapped (fail closed — never
//! a fault inside the fault handler).

use tairix_arch_api::{
    Backtrace, BacktraceProfile, CpuStateCapture, FrameLayout, RegisterSnapshot, StackBounds,
};

#[cfg(all(target_arch = "aarch64", target_os = "none"))]
extern "C" {
    /// Lowest address of the BSP boot stack (see `boot.s`).
    static __boot_stack_bottom: u8;
    /// Exclusive top of the BSP boot stack (see `boot.s`).
    static __boot_stack_top: u8;
}

/// aarch64 implementation of the Arch HAL post-mortem-capture surface.
///
/// Zero-sized: capturing registers needs no per-instance state.
#[derive(Debug, Default, Clone, Copy)]
pub struct Backtracer;

impl Backtracer {
    /// Construct the aarch64 post-mortem-capture handle.
    #[must_use]
    pub const fn new() -> Self {
        Self
    }

    /// The AAPCS64 frame-pointer layout (see the module docs).
    pub const LAYOUT: FrameLayout = FrameLayout {
        saved_fp_offset: 0,
        return_addr_offset: 8,
    };

    /// The honest declaration for aarch64: both capabilities supported.
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

    #[cfg(all(target_arch = "aarch64", target_os = "none"))]
    fn capture(&self) -> RegisterSnapshot {
        let (pc, sp, fp, lr): (u64, u64, u64, u64);
        let (x0, x1, x2, x3): (u64, u64, u64, u64);
        let (x4, x5, x6, x7): (u64, u64, u64, u64);
        // SAFETY: `adr` yields the current PC, `mov` copies `sp`/a GP
        // register into an output operand; none writes memory, changes
        // flags, or faults. Reading `x29`/`sp` does not clobber them, so
        // the frame-pointer build's reservation of `x29` is respected.
        unsafe {
            core::arch::asm!(
                "adr {pc}, .",
                "mov {sp}, sp",
                "mov {fp}, x29",
                "mov {lr}, x30",
                pc = out(reg) pc,
                sp = out(reg) sp,
                fp = out(reg) fp,
                lr = out(reg) lr,
                options(nomem, nostack, preserves_flags),
            );
            core::arch::asm!(
                "mov {x0}, x0",
                "mov {x1}, x1",
                "mov {x2}, x2",
                "mov {x3}, x3",
                x0 = out(reg) x0,
                x1 = out(reg) x1,
                x2 = out(reg) x2,
                x3 = out(reg) x3,
                options(nomem, nostack, preserves_flags),
            );
            core::arch::asm!(
                "mov {x4}, x4",
                "mov {x5}, x5",
                "mov {x6}, x6",
                "mov {x7}, x7",
                x4 = out(reg) x4,
                x5 = out(reg) x5,
                x6 = out(reg) x6,
                x7 = out(reg) x7,
                options(nomem, nostack, preserves_flags),
            );
        }
        RegisterSnapshot::new(pc, sp, fp)
            .with("x0", x0)
            .with("x1", x1)
            .with("x2", x2)
            .with("x3", x3)
            .with("x4", x4)
            .with("x5", x5)
            .with("x6", x6)
            .with("x7", x7)
            .with("x29", fp)
            .with("x30", lr)
            .with("sp", sp)
    }

    #[cfg(not(all(target_arch = "aarch64", target_os = "none")))]
    fn capture(&self) -> RegisterSnapshot {
        // Off-target (host unit tests): the real capture is the
        // target-gated asm path above; the QEMU panic vertical proves the
        // on-target capture is non-trivial.
        RegisterSnapshot::new(0, 0, 0)
    }

    #[cfg(all(target_arch = "aarch64", target_os = "none"))]
    fn stack_bounds(&self) -> Option<StackBounds> {
        let sp: u64;
        // SAFETY: reading `sp` into an output operand has no side effects
        // and cannot fault.
        unsafe {
            core::arch::asm!("mov {}, sp", out(reg) sp, options(nomem, nostack, preserves_flags));
        }
        // SAFETY: taking the address of the extern boot-stack symbols is a
        // link-time constant; we never dereference them.
        let low = core::ptr::addr_of!(__boot_stack_bottom) as u64;
        let high = core::ptr::addr_of!(__boot_stack_top) as u64;
        StackBounds::enclosing(sp, low, high)
    }

    #[cfg(not(all(target_arch = "aarch64", target_os = "none")))]
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
    fn frame_layout_is_aapcs64() {
        let l = Backtracer::new().frame_layout().expect("supported");
        assert_eq!(l.saved_fp_offset, 0);
        assert_eq!(l.return_addr_offset, 8);
    }
}
