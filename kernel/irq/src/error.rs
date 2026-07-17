//! Failure surface for [`crate::IrqTable`] operations.
//!
//! The [`IrqError`] enum is the *internal* failure surface;
//! `kernel/core::syscalls::KernelSyscallHandlers` translates each
//! variant into the stable `tairix_abi::Errno` documented in
//! `docs/src/security/irq.md` (the failure-mode table). The
//! translation is intentionally one-to-one so the security audit
//! trail can correlate a syscall-handler-side rejection to the
//! exact kernel-side cause.

use tairix_abi::Errno;

/// Failure modes of [`crate::IrqTable::bind`] and
/// [`crate::IrqTable::fire`].
///
/// Mapped to ABI errnos at the syscall boundary; see
/// [`Self::to_errno`].
#[derive(Copy, Clone, Debug, Eq, PartialEq)]
pub enum IrqError {
    /// `line` argument exceeded the table's configured
    /// `max_line`. The platform-specific upper bound comes from the
    /// architecture port (e.g. the maximum IO-APIC redirection
    /// entry index on x86_64). Maps to [`Errno::OutOfRange`].
    LineOutOfRange,
    /// A binding for `line` already exists. The contract in
    /// `docs/src/security/irq.md` is one binding per
    /// `(task, line)`; the table additionally refuses two bindings
    /// for the same `line` regardless of task, because hardware
    /// interrupts are not shareable in `abi-v1` (PCI MSI/MSI-X
    /// allocates a dedicated GSI per queue). Maps to
    /// [`Errno::OutOfRange`] — the closest stable variant meaning
    /// "the operation was inapplicable to the current state".
    LineAlreadyBound,
    /// The controller-side mask write failed. The arch port
    /// reported the line was not programmable through its
    /// controller interface (e.g. an architecture without an
    /// implementation of the [`crate::IrqController`] trait).
    /// Maps to [`Errno::NotImplemented`].
    ArchUnsupported,
}

impl IrqError {
    /// Translate to the ABI errno the syscall handler returns.
    #[must_use]
    pub const fn to_errno(self) -> Errno {
        match self {
            Self::LineOutOfRange | Self::LineAlreadyBound => Errno::OutOfRange,
            Self::ArchUnsupported => Errno::NotImplemented,
        }
    }
}

/// Failure modes of [`crate::IrqController::mask`].
///
/// Separate from [`IrqError`] so an architecture port without a
/// programmable interrupt controller can declare so explicitly
/// (the production wiring on aarch64 / riscv64 / wasm32 returns
/// [`Self::Unsupported`], surfaced at the syscall boundary as
/// `Errno::NotImplemented`). On x86_64 the IO-APIC implementation
/// returns [`Self::OutOfRange`] when the line exceeds
/// `max_redirection_entry`.
#[derive(Copy, Clone, Debug, Eq, PartialEq)]
pub enum MaskError {
    /// The architecture has no programmable controller wired in
    /// this build. Always returned by the placeholder
    /// `IrqController` impls on aarch64 / riscv64 / wasm32.
    Unsupported,
    /// The line is outside the controller's addressable range.
    OutOfRange,
}
