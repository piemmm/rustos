//! [`EntryResolver`] — turn a verified image payload into a driver
//! `register` entry point.
//!
//! In a production deployment the driver image is a real `.rxe`
//! executable and its `register` symbol is resolved by the userland
//! dynamic linker once the image has been loaded into its own
//! process. That mechanism is deferred to a later Stage (image loader +
//! per-process address space). For the Stage 4 deliverable the host
//! defines a stable seam — the trait below — that production and test
//! callers can both implement: the host itself only ever invokes the
//! resolver after the manifest has cleared every verification gate.

use rustos_abi::{DriverError, DriverHandle, DriverHost, DriverManifest};

/// Canonical `register` entry point signature (`AGENTS.md` §8 / §10):
///
/// ```text
/// pub fn register(host: &dyn DriverHost) -> Result<DriverHandle, DriverError>;
/// ```
///
/// A function pointer (not a closure) is used deliberately so the type
/// is `'static`, [`Copy`], and trivially fits inside a `LoadedRecord`.
pub type DriverEntry = fn(host: &dyn DriverHost) -> Result<DriverHandle, DriverError>;

/// Bind a verified manifest + payload to a driver `register` entry
/// point.
///
/// Implementations are free to key the resolution on any subset of the
/// manifest they choose; the tests in this crate key on the
/// `signer_pubkey` field, the QEMU integration test keys on a fixed
/// `(signer_pubkey, payload_first_word)` tuple, and a future
/// production implementation will load the image into a freshly
/// minted process and `dlsym("register")`.
///
/// # Errors
///
/// Return `None` if the resolver cannot bind the manifest to a known
/// driver entry; the host then surfaces
/// [`crate::HostError::UnknownDriver`] to its caller.
///
/// # Capabilities
///
/// None. The host has already verified the manifest before the
/// resolver is called.
pub trait EntryResolver {
    /// Bind `manifest` + `payload` to an entry point, or return `None`
    /// if the resolver has no driver registered for this manifest.
    fn resolve(&self, manifest: &DriverManifest, payload: &[u8]) -> Option<DriverEntry>;
}

#[cfg(test)]
mod tests {
    use super::*;
    use rustos_abi::DriverKind;

    fn ok_entry(_host: &dyn DriverHost) -> Result<DriverHandle, DriverError> {
        DriverHandle::from_raw(99)
    }

    struct Single;
    impl EntryResolver for Single {
        fn resolve(&self, _m: &DriverManifest, _p: &[u8]) -> Option<DriverEntry> {
            Some(ok_entry as DriverEntry)
        }
    }

    struct Empty;
    impl EntryResolver for Empty {
        fn resolve(&self, _m: &DriverManifest, _p: &[u8]) -> Option<DriverEntry> {
            None
        }
    }

    fn sample_manifest() -> DriverManifest {
        DriverManifest {
            magic: rustos_abi::DRIVER_MANIFEST_MAGIC,
            abi_version: rustos_abi::ABI_VERSION_CURRENT,
            kind: DriverKind::UserSpace,
            reserved0: 0,
            capability_count: 0,
            syscall_table_hash: [0u8; 32],
            signer_pubkey: [0u8; 32],
            signature: [0u8; 64],
        }
    }

    struct StubHost;
    impl DriverHost for StubHost {
        fn has_capability(&self, _cap: rustos_abi::CapabilityId) -> bool {
            true
        }
        fn kind(&self) -> DriverKind {
            DriverKind::UserSpace
        }
    }

    #[test]
    fn single_resolver_returns_entry() {
        let r = Single;
        let m = sample_manifest();
        let entry = r.resolve(&m, b"").expect("resolver binds");
        let handle = entry(&StubHost).expect("entry returns Ok");
        assert_eq!(handle.as_u64(), 99);
    }

    #[test]
    fn empty_resolver_returns_none() {
        let r = Empty;
        let m = sample_manifest();
        assert!(r.resolve(&m, b"").is_none());
    }
}
