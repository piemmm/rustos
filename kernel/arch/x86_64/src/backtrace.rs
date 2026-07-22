//! x86_64 post-mortem CPU-state capture.
//!
//! Implements the Arch HAL
//! [`tairix_arch_api::CpuStateCapture`] surface for
//! x86_64: a read-only register snapshot and the System V frame-pointer
//! layout the neutral unwinder in `kernel/core` follows.
//!
//! # Frame layout
//!
//! With frame pointers forced (`.cargo/config.toml` carries
//! `-C force-frame-pointers=yes` for `x86_64-unknown-none`), every
//! function maintains `rbp` as its frame pointer. The prologue pushes the
//! caller's `rbp` and the `call` instruction pushed the return address
//! just above it, so relative to the current `rbp`:
//!
//! * the caller's saved `rbp` is at `[rbp + 0]`,
//! * the return address into the caller is at `[rbp + 8]`.
//!
//! # Stack bounds
//!
//! The bootstrap processor runs on the linker-reserved boot stack
//! (`boot_stack_bottom .. boot_stack_top` in `boot.s`, kept mapped by the
//! preserved identity map). `stack_bounds` returns those bounds
//! when the captured `sp` lies within them and `None` otherwise, so the
//! unwinder degrades to registers + program counter on a stack the port
//! cannot vouch for rather than reading memory that might be unmapped
//! (fail closed — never a fault inside the fault handler).

use tairix_arch_api::{
    Backtrace, BacktraceProfile, CpuStateCapture, FrameLayout, RegisterSnapshot, StackBounds,
};

#[cfg(all(target_arch = "x86_64", target_os = "none"))]
extern "C" {
    /// Lowest address of the BSP boot stack (see `boot.s`).
    static boot_stack_bottom: u8;
    /// Exclusive top of the BSP boot stack (see `boot.s`).
    static boot_stack_top: u8;
}

/// x86_64 implementation of the Arch HAL post-mortem-capture surface.
///
/// Zero-sized: capturing registers needs no per-instance state.
#[derive(Debug, Default, Clone, Copy)]
pub struct Backtracer;

impl Backtracer {
    /// Construct the x86_64 post-mortem-capture handle.
    #[must_use]
    pub const fn new() -> Self {
        Self
    }

    /// The System V x86_64 frame-pointer layout (see the module docs).
    pub const LAYOUT: FrameLayout = FrameLayout {
        saved_fp_offset: 0,
        return_addr_offset: 8,
    };

    /// The honest declaration for x86_64: both capabilities supported.
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

    #[cfg(all(target_arch = "x86_64", target_os = "none"))]
    fn capture(&self) -> RegisterSnapshot {
        let (rip, rsp, rbp): (u64, u64, u64);
        let (rax, rbx, rcx, rdx): (u64, u64, u64, u64);
        let (rsi, rdi, r8, r9): (u64, u64, u64, u64);
        let (r10, r11, r12, r13): (u64, u64, u64, u64);
        let (r14, r15): (u64, u64);
        // The reads are split across several `asm!` blocks so no single
        // block asks the register allocator for more scratch outputs than
        // the ABI leaves free. Each block is independently sound:
        // SAFETY: every instruction copies one register (or the current
        // `rip` via `lea`) into an output operand; none writes memory,
        // changes flags, or faults. `rbp`/`rsp` are read, not clobbered, so
        // the frame-pointer build's reservation of `rbp` is respected.
        unsafe {
            core::arch::asm!(
                "lea {rip}, [rip]",
                "mov {rsp}, rsp",
                "mov {rbp}, rbp",
                rip = out(reg) rip,
                rsp = out(reg) rsp,
                rbp = out(reg) rbp,
                options(nomem, nostack, preserves_flags),
            );
            core::arch::asm!(
                "mov {rax}, rax",
                "mov {rbx}, rbx",
                "mov {rcx}, rcx",
                "mov {rdx}, rdx",
                rax = out(reg) rax,
                rbx = out(reg) rbx,
                rcx = out(reg) rcx,
                rdx = out(reg) rdx,
                options(nomem, nostack, preserves_flags),
            );
            core::arch::asm!(
                "mov {rsi}, rsi",
                "mov {rdi}, rdi",
                "mov {r8}, r8",
                "mov {r9}, r9",
                rsi = out(reg) rsi,
                rdi = out(reg) rdi,
                r8 = out(reg) r8,
                r9 = out(reg) r9,
                options(nomem, nostack, preserves_flags),
            );
            core::arch::asm!(
                "mov {r10}, r10",
                "mov {r11}, r11",
                "mov {r12}, r12",
                "mov {r13}, r13",
                "mov {r14}, r14",
                "mov {r15}, r15",
                r10 = out(reg) r10,
                r11 = out(reg) r11,
                r12 = out(reg) r12,
                r13 = out(reg) r13,
                r14 = out(reg) r14,
                r15 = out(reg) r15,
                options(nomem, nostack, preserves_flags),
            );
        }
        RegisterSnapshot::new(rip, rsp, rbp)
            .with("rax", rax)
            .with("rbx", rbx)
            .with("rcx", rcx)
            .with("rdx", rdx)
            .with("rsi", rsi)
            .with("rdi", rdi)
            .with("rbp", rbp)
            .with("rsp", rsp)
            .with("r8", r8)
            .with("r9", r9)
            .with("r10", r10)
            .with("r11", r11)
            .with("r12", r12)
            .with("r13", r13)
            .with("r14", r14)
            .with("r15", r15)
    }

    #[cfg(not(all(target_arch = "x86_64", target_os = "none")))]
    fn capture(&self) -> RegisterSnapshot {
        // Off-target (host unit tests): the real capture is the
        // target-gated asm path above. Returning an empty snapshot here is
        // honest — the register file of a bare-metal x86_64 kernel is not
        // meaningfully readable from the host test process — and the QEMU
        // panic vertical proves the on-target capture is non-trivial.
        RegisterSnapshot::new(0, 0, 0)
    }

    #[cfg(all(target_arch = "x86_64", target_os = "none"))]
    fn stack_bounds(&self) -> Option<StackBounds> {
        let sp: u64;
        // SAFETY: reading `rsp` into an output operand has no side effects
        // and cannot fault.
        unsafe {
            core::arch::asm!("mov {}, rsp", out(reg) sp, options(nomem, nostack, preserves_flags));
        }
        // SAFETY: taking the address of the extern boot-stack symbols is a
        // link-time constant; we never dereference them.
        let low = core::ptr::addr_of!(boot_stack_bottom) as u64;
        let high = core::ptr::addr_of!(boot_stack_top) as u64;
        StackBounds::enclosing(sp, low, high)
    }

    #[cfg(not(all(target_arch = "x86_64", target_os = "none")))]
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
    fn frame_layout_is_system_v() {
        let l = Backtracer::new().frame_layout().expect("supported");
        assert_eq!(l.saved_fp_offset, 0);
        assert_eq!(l.return_addr_offset, 8);
    }
}
