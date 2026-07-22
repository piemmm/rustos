//! wasm32 post-mortem CPU-state capture — an honest `Unsupported`.
//!
//! Implements the Arch HAL
//! [`tairix_arch_api::CpuStateCapture`] surface for
//! wasm32. WebAssembly has no architectural register file a guest can
//! read and no frame-pointer chain in linear memory to walk: the call
//! stack is host-managed and a `wasm32-unknown-unknown` panic traps to the
//! JavaScript harness (`crate::panic`), which surfaces the failure with
//! the host's own stack trace. There is therefore genuinely nothing for
//! this port to capture or unwind.
//!
//! So both capabilities are a justified
//! `Unsupported` —
//! the same honest-declaration shape the memory-tagging and side-channel
//! surfaces use — never a faked no-op that pretends to capture registers
//! it cannot read.

use tairix_arch_api::{
    Backtrace, BacktraceProfile, CpuStateCapture, FrameLayout, RegisterSnapshot, StackBounds,
};

/// wasm32 implementation of the Arch HAL post-mortem-capture surface.
///
/// Zero-sized: an unsupporting port carries no per-instance state.
#[derive(Debug, Default, Clone, Copy)]
pub struct Backtracer;

impl Backtracer {
    /// Construct the wasm32 post-mortem-capture handle.
    #[must_use]
    pub const fn new() -> Self {
        Self
    }

    /// The honest declaration for wasm32 (see the module docs).
    #[must_use]
    pub const fn declared_profile() -> BacktraceProfile {
        BacktraceProfile {
            register_capture: Backtrace::Unsupported(
                "WebAssembly exposes no readable architectural register file to the guest",
            ),
            frame_unwind: Backtrace::Unsupported(
                "the call stack is host-managed; a wasm32 panic traps to the JS harness, which \
                 provides the host stack trace",
            ),
        }
    }
}

impl CpuStateCapture for Backtracer {
    fn profile(&self) -> BacktraceProfile {
        Self::declared_profile()
    }

    fn frame_layout(&self) -> Option<FrameLayout> {
        // No frame-pointer chain to walk in linear memory.
        None
    }

    fn capture(&self) -> RegisterSnapshot {
        // Honest empty snapshot — no registers to read (never faked).
        RegisterSnapshot::new(0, 0, 0)
    }

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
    fn declared_profile_is_honest_unsupported() {
        let p = Backtracer::new().profile();
        assert_eq!(p.validate(), Ok(()));
        assert!(matches!(p.register_capture, Backtrace::Unsupported(_)));
        assert!(matches!(p.frame_unwind, Backtrace::Unsupported(_)));
        // No layout and an empty snapshot back the Unsupported claim.
        assert!(Backtracer::new().frame_layout().is_none());
        let snap = Backtracer::new().capture();
        assert_eq!((snap.pc, snap.sp, snap.fp), (0, 0, 0));
        assert!(snap.named().is_empty());
    }
}
