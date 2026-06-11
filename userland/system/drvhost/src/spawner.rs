//! [`DriverSpawner`] — run a verified driver image's registration in its
//! own protection domain.
//!
//! This is the host's hand-off seam for `PLAN.md` Stage 4.HW: once a
//! `.rxe` image has cleared every verification gate (manifest, ABI
//! version, syscall-table hash, trust anchor, signature, capability
//! subset), the host passes the verified manifest, the image payload,
//! and the granted-capability [`DriverHost`] view to the spawner, which
//! completes the driver's registration and reports the outcome. The
//! production implementation spawns the payload into its own process
//! (`kernel/mem::build_process_image` → spawn) and completes the
//! `register()` handshake over IPC; test and QEMU-vertical
//! implementations invoke a known in-process entry point directly so
//! the host's verification and lifecycle paths are exercised without a
//! scheduler.
//!
//! The seam deliberately returns the *outcome* of registration rather
//! than an entry point for the host to call: the host never holds a
//! pointer into the driver image, so the in-image function-pointer
//! binding the former resolver performed cannot reappear.

use rustos_abi::{DriverError, DriverHandle, DriverHost, DriverManifest};

/// Canonical `register` entry-point signature (`AGENTS.md` §8 / §10):
///
/// ```text
/// pub fn register(host: &dyn DriverHost) -> Result<DriverHandle, DriverError>;
/// ```
///
/// Every driver crate exports exactly this signature; in-process
/// [`DriverSpawner`] implementations (tests, QEMU verticals) hold one
/// and invoke it with [`SpawnContext::host`]. A function pointer (not a
/// closure) is used deliberately so the type is `'static` and [`Copy`].
pub type DriverEntry = fn(host: &dyn DriverHost) -> Result<DriverHandle, DriverError>;

/// Everything the host hands a [`DriverSpawner`] for one verified image.
///
/// All borrows live for the duration of the `spawn_and_register` call;
/// the spawner must not retain them.
pub struct SpawnContext<'a> {
    /// The verified, signature-checked manifest.
    pub manifest: &'a DriverManifest,
    /// The image payload following the manifest header and capability
    /// body — in production, the driver program the spawner loads into
    /// a fresh process.
    pub payload: &'a [u8],
    /// Driver-visible host view carrying exactly the granted capability
    /// set (the manifest's request intersected with the caller's set)
    /// and the per-driver virtio host, if one was minted. In-process
    /// implementations pass this to the driver's `register` entry.
    pub host: &'a dyn DriverHost,
}

/// Why a spawn-and-register hand-off failed.
#[derive(Copy, Clone, Debug, Eq, PartialEq)]
pub enum SpawnRegisterError {
    /// The spawner has no driver program for this manifest. The host
    /// surfaces [`crate::HostError::UnknownDriver`].
    NoDriver,
    /// The driver's `register` ran and reported failure. The host
    /// surfaces [`crate::HostError::DriverRegisterFailed`].
    Register(DriverError),
}

/// Complete a verified driver image's registration.
///
/// # Errors
///
/// [`SpawnRegisterError::NoDriver`] if the spawner cannot bind the
/// manifest to a driver program; [`SpawnRegisterError::Register`] if
/// the driver's `register` entry reported failure.
///
/// # Capabilities
///
/// None checked here. The host has already verified the manifest and
/// intersected its requested capabilities with the caller's set before
/// the spawner is called; [`SpawnContext::host`] exposes exactly that
/// granted set (`AGENTS.md` §5.2).
pub trait DriverSpawner {
    /// Run the verified image's registration, returning the handle the
    /// driver reported. The returned handle is informational — the host
    /// mints its own unforgeable handle on success.
    fn spawn_and_register(
        &self,
        ctx: &SpawnContext<'_>,
    ) -> Result<DriverHandle, SpawnRegisterError>;
}

#[cfg(test)]
mod tests {
    use super::*;
    use rustos_abi::DriverKind;

    fn ok_entry(_host: &dyn DriverHost) -> Result<DriverHandle, DriverError> {
        DriverHandle::from_raw(99)
    }

    struct Single;
    impl DriverSpawner for Single {
        fn spawn_and_register(
            &self,
            ctx: &SpawnContext<'_>,
        ) -> Result<DriverHandle, SpawnRegisterError> {
            let entry: DriverEntry = ok_entry;
            entry(ctx.host).map_err(SpawnRegisterError::Register)
        }
    }

    struct Empty;
    impl DriverSpawner for Empty {
        fn spawn_and_register(
            &self,
            _ctx: &SpawnContext<'_>,
        ) -> Result<DriverHandle, SpawnRegisterError> {
            Err(SpawnRegisterError::NoDriver)
        }
    }

    fn sample_manifest() -> DriverManifest {
        DriverManifest {
            magic: rustos_abi::DRIVER_MANIFEST_MAGIC,
            abi_version: rustos_abi::ABI_VERSION_CURRENT,
            kind: DriverKind::UserSpace,
            bind_key_count: 0,
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
    fn in_process_spawner_reports_registered_handle() {
        let s = Single;
        let m = sample_manifest();
        let ctx = SpawnContext {
            manifest: &m,
            payload: b"",
            host: &StubHost,
        };
        let handle = s.spawn_and_register(&ctx).expect("registration succeeds");
        assert_eq!(handle.as_u64(), 99);
    }

    #[test]
    fn empty_spawner_reports_no_driver() {
        let s = Empty;
        let m = sample_manifest();
        let ctx = SpawnContext {
            manifest: &m,
            payload: b"",
            host: &StubHost,
        };
        assert_eq!(
            s.spawn_and_register(&ctx),
            Err(SpawnRegisterError::NoDriver)
        );
    }
}
