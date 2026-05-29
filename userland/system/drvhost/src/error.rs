//! Host-level errors returned across the [`crate::Host`] public surface.
//!
//! Every load / unload / reload outcome maps to exactly one variant of
//! [`HostError`]; the variants are deliberately disjoint from the kernel
//! [`rustos_abi::Errno`] so that a mis-routed value cannot be confused
//! between layers (`AGENTS.md` §5.4 — fail closed, never silently widen).
//!
//! The set is `#[non_exhaustive]` so new failure modes can be added
//! without breaking downstream `match` arms.

use core::fmt;

use rustos_abi::{DriverError, Errno};

/// One failure outcome from a [`crate::Host`] operation.
#[derive(Copy, Clone, Debug, Eq, PartialEq)]
#[non_exhaustive]
pub enum HostError {
    /// The supplied image bytes are shorter than a [`rustos_abi::DriverManifest`].
    ImageTruncated,
    /// The manifest decoded cleanly but with content the ABI rejects
    /// (bad magic, unsupported `abi_version`, out-of-range field, …).
    ManifestInvalid(DriverError),
    /// The manifest's pinned `syscall_table_hash` disagrees with the host's
    /// compiled-in hash. Indicates an ABI generation mismatch.
    SyscallHashMismatch,
    /// The manifest is signed by a public key that is not on the host's
    /// trust anchor list. Cryptographically distinct from
    /// [`Self::SignatureInvalid`] so audit consumers can tell the two
    /// apart.
    UntrustedSigner,
    /// Ed25519 verification of the manifest signature failed.
    SignatureInvalid,
    /// A capability identifier inside the manifest body is outside the
    /// ABI's identifier space ([`rustos_abi::CAPABILITY_ID_MAX`]).
    CapabilityOutOfRange,
    /// The driver requested at least one capability the caller does not
    /// hold. Surfaces the subset-only delegation rule
    /// (`AGENTS.md` §5.2).
    CapabilityEscalation,
    /// The driver declared `kind = InKernel` but the caller does not
    /// hold [`rustos_abi::CapabilityId::DRV_KERNEL`].
    KernelKindForbidden,
    /// The caller does not hold [`rustos_abi::CapabilityId::DRV_LOAD`].
    LoadCapabilityMissing,
    /// The configured [`crate::EntryResolver`] could not turn the verified
    /// image payload into a driver `register` entry point.
    UnknownDriver,
    /// The driver's `register` entry point itself rejected the host.
    DriverRegisterFailed(DriverError),
    /// A [`rustos_abi::DriverHandle`] was supplied to `unload` / `reload`
    /// that the host has no record of.
    HandleNotFound,
    /// An [`crate::ImageSource`] read failed; the inner [`Errno`] is the
    /// source-supplied reason.
    SourceFailed(Errno),
}

impl HostError {
    /// Map a [`HostError`] to a kernel [`Errno`] for syscall returns.
    ///
    /// The mapping is total: every variant has a stable counterpart in
    /// the `abi-v1` error surface, so the host never has to invent an
    /// `Errno` value at runtime.
    #[must_use]
    pub const fn as_errno(self) -> Errno {
        match self {
            Self::ImageTruncated => Errno::BufferTooSmall,
            Self::ManifestInvalid(_) => Errno::BadMagic,
            Self::SyscallHashMismatch | Self::UntrustedSigner | Self::SignatureInvalid => {
                Errno::SignatureInvalid
            }
            Self::CapabilityOutOfRange => Errno::OutOfRange,
            Self::CapabilityEscalation
            | Self::KernelKindForbidden
            | Self::LoadCapabilityMissing => Errno::PermissionDenied,
            Self::UnknownDriver | Self::HandleNotFound => Errno::NotFound,
            Self::DriverRegisterFailed(_) => Errno::NotImplemented,
            Self::SourceFailed(e) => e,
        }
    }
}

impl fmt::Display for HostError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let s = match self {
            Self::ImageTruncated => "rxe image is shorter than the manifest header",
            Self::ManifestInvalid(_) => "rxe manifest header rejected by abi-v1 decoder",
            Self::SyscallHashMismatch => "manifest syscall_table_hash does not match host",
            Self::UntrustedSigner => "manifest signer pubkey is not on the host trust anchor list",
            Self::SignatureInvalid => "manifest ed25519 signature verification failed",
            Self::CapabilityOutOfRange => "manifest capability id exceeds abi-v1 maximum",
            Self::CapabilityEscalation => "manifest requests capabilities the caller does not hold",
            Self::KernelKindForbidden => "in-kernel driver requires CAP_DRV_KERNEL",
            Self::LoadCapabilityMissing => "caller does not hold CAP_DRV_LOAD",
            Self::UnknownDriver => "resolver could not bind manifest to a driver entry",
            Self::DriverRegisterFailed(_) => "driver register() entry point returned error",
            Self::HandleNotFound => "no loaded driver with the supplied handle",
            Self::SourceFailed(_) => "image source read failed",
        };
        f.write_str(s)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn errno_mapping_is_total_and_stable() {
        assert_eq!(HostError::ImageTruncated.as_errno(), Errno::BufferTooSmall);
        assert_eq!(
            HostError::ManifestInvalid(DriverError::BadMagic).as_errno(),
            Errno::BadMagic
        );
        assert_eq!(
            HostError::SyscallHashMismatch.as_errno(),
            Errno::SignatureInvalid
        );
        assert_eq!(
            HostError::UntrustedSigner.as_errno(),
            Errno::SignatureInvalid
        );
        assert_eq!(
            HostError::SignatureInvalid.as_errno(),
            Errno::SignatureInvalid
        );
        assert_eq!(
            HostError::CapabilityOutOfRange.as_errno(),
            Errno::OutOfRange
        );
        assert_eq!(
            HostError::CapabilityEscalation.as_errno(),
            Errno::PermissionDenied
        );
        assert_eq!(
            HostError::KernelKindForbidden.as_errno(),
            Errno::PermissionDenied
        );
        assert_eq!(
            HostError::LoadCapabilityMissing.as_errno(),
            Errno::PermissionDenied
        );
        assert_eq!(HostError::UnknownDriver.as_errno(), Errno::NotFound);
        assert_eq!(HostError::HandleNotFound.as_errno(), Errno::NotFound);
        assert_eq!(
            HostError::DriverRegisterFailed(DriverError::DeviceFault).as_errno(),
            Errno::NotImplemented
        );
        assert_eq!(
            HostError::SourceFailed(Errno::NotFound).as_errno(),
            Errno::NotFound
        );
    }

    extern crate alloc;
    use alloc::format;

    #[test]
    fn display_is_stable() {
        // Sampled here so a careless rename of the human-readable text
        // in one place is caught by the test that downstream log
        // consumers grep against.
        assert_eq!(
            format!("{}", HostError::CapabilityEscalation),
            "manifest requests capabilities the caller does not hold",
        );
        assert_eq!(
            format!("{}", HostError::KernelKindForbidden),
            "in-kernel driver requires CAP_DRV_KERNEL",
        );
    }
}
