//! In-kernel signed-driver-load gate (`plans/PI.md` P10 5c-ii; PLAN Stage
//! 4.HW item 5).
//!
//! The [`driver_catalog`](crate::driver_catalog) registry decides *which*
//! driver binds a discovered hardware node. This module
//! is the load *mechanism*: it admits a registered in-kernel driver through
//! the signed-manifest `drvhost::Host::load` gate — Ed25519 signature
//! verification against the build's embedded driver-signing key, the
//! `CAP_DRV_LOAD` hard gate, and the `CAP_DRV_KERNEL` gate that every
//! `kind = InKernel` manifest additionally demands —
//! rather than calling a driver's `register()` directly. Every capability
//! and input check stays kernel-side and the load fails closed. The gate is generic over hardware: it admits any driver in the
//! [`IN_KERNEL_DRIVERS`](crate::driver_catalog::IN_KERNEL_DRIVERS) registry,
//! never a hard-coded device list.
//!
//! # In-kernel, statically-linked drivers
//!
//! keeps drivers in user space wherever feasible and
//! sanctions the drivers that must run in-kernel (`kind = InKernel`). Those
//! drivers are linked into the kernel image and brought up through this
//! gate: the spawner is an in-process register that invokes the verified
//! driver's own `register()` entry point (an admission check returning a
//! marker handle) on the granted-capability host view the gate built. The
//! signed image carries no program payload — there is no separate binary to
//! spawn — but it carries the driver's real signed manifest (kind,
//! capability request, and the driver crate's own bind table), so the
//! *trust decision* is identical to the user-space load path. For the
//! remaining in-kernel floor — the bootstrap storage drivers — the real
//! register-window mapping and DMA carve run afterwards over the consuming
//! service's own capability-gated host (`crate::aarch64::root_unlock`), not
//! this admission view.
//!
//! The baked, signed manifest images and the build's driver-signing public
//! key are produced by `build.rs` (`emit_signed_driver_manifests`) and
//! owned by the [`driver_catalog`](crate::driver_catalog) registry.

use rustos_abi::{Errno, ABI_VERSION_CURRENT};
use rustos_caps::CapabilitySet;
use rustos_crypto::Ed25519PublicKey;
use rustos_drvhost::{
    DriverEntry, DriverHandle, Host, HostConfig, HostError, ImageSource, SpawnContext,
    SpawnRegisterError,
};
use rustos_drvhost::{DriverSpawner, Sink};
use rustos_kernel_syscall::SYSCALL_TABLE_HASH;

use crate::driver_catalog::{driver_for, KERNEL_DRIVER_SIGNER_PUBKEY};

/// [`ImageSource`] over a single baked, signed manifest image.
struct BakedImageSource {
    image: &'static [u8],
}

impl ImageSource for BakedImageSource {
    fn read(&self, _path: &str, buf: &mut alloc::vec::Vec<u8>) -> Result<(), Errno> {
        buf.extend_from_slice(self.image);
        Ok(())
    }
}

/// [`DriverSpawner`] that completes one verified manifest's registration
/// by invoking the statically-linked driver's `register()` entry on the
/// granted-capability host view the gate built (the in-process register,
/// `plans/PI.md` P10 5c-ii).
struct InProcessRegister {
    entry: DriverEntry,
}

impl DriverSpawner for InProcessRegister {
    fn spawn_and_register(
        &self,
        ctx: &SpawnContext<'_>,
    ) -> Result<DriverHandle, SpawnRegisterError> {
        (self.entry)(ctx.host).map_err(SpawnRegisterError::Register)
    }
}

/// Admits a registered in-kernel driver through the signed-manifest
/// `drvhost::Host::load` gate against the build's embedded driver-signing
/// trust anchor.
pub struct KernelDriverLoader<'s> {
    /// The build's driver-signing public key — the kernel's sole driver
    /// trust anchor (`build.rs`).
    trusted: [Ed25519PublicKey; 1],
    /// Audit sink every `Host::load` decision is logged through.
    sink: &'s dyn Sink,
}

impl<'s> KernelDriverLoader<'s> {
    /// Build a loader trusting only the build's embedded driver-signing
    /// key, logging every decision to `sink`.
    ///
    /// Returns [`None`] fail-closed if the embedded
    /// key bytes are not a valid Ed25519 point — a corrupted build
    /// rather than an admissible state; the caller then starts no driver.
    #[must_use]
    pub fn new(sink: &'s dyn Sink) -> Option<Self> {
        let key = Ed25519PublicKey::from_bytes(&KERNEL_DRIVER_SIGNER_PUBKEY).ok()?;
        Some(Self {
            trusted: [key],
            sink,
        })
    }

    /// Admit the in-kernel driver registered at `path`, granting it the
    /// intersection of its manifest request with `caller_caps`.
    ///
    /// Runs the full `drvhost::Host::load` pipeline: manifest parse,
    /// syscall-table-hash match against the kernel's compiled-in
    /// [`SYSCALL_TABLE_HASH`], trust-anchor + Ed25519 signature
    /// verification, the `CAP_DRV_LOAD` / `CAP_DRV_KERNEL` capability
    /// gates, bind-table validation, and the in-process `register()`
    /// hand-off — every check kernel-side, failing closed at the first
    /// failure.
    ///
    /// # Errors
    ///
    /// [`HostError::UnknownDriver`] if `path` is not a registered in-kernel
    /// driver; otherwise the first failure the load pipeline surfaces.
    pub fn admit(
        &self,
        path: &str,
        caller_caps: &CapabilitySet,
    ) -> Result<DriverHandle, HostError> {
        let Some(driver) = driver_for(path) else {
            return Err(HostError::UnknownDriver);
        };
        let source = BakedImageSource {
            image: driver.image,
        };
        let spawner = InProcessRegister {
            entry: driver.register,
        };
        let mut host = Host::new(HostConfig {
            trusted_signers: &self.trusted,
            syscall_table_hash: SYSCALL_TABLE_HASH,
            accepted_abi_version: ABI_VERSION_CURRENT,
            source: &source,
            spawner: &spawner,
            sink: self.sink,
            virtio_host_factory: None,
            mmio_mapper: None,
        });
        host.load(path, caller_caps)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    use crate::driver_catalog::IN_KERNEL_DRIVERS;
    use rustos_abi::CapabilityId;
    use rustos_drvhost::{Event, Sink};

    /// No-op audit sink: the admission tests assert the load *result*,
    /// not the audit stream.
    struct NullSink;
    impl Sink for NullSink {
        fn write_event(&self, _event: &Event<'_>) {}
    }

    fn caps(ids: &[CapabilityId]) -> CapabilitySet {
        let mut set = CapabilitySet::empty();
        for id in ids {
            set.insert(*id);
        }
        set
    }

    /// A caller carrying both the universal load capability and the
    /// in-kernel-driver capability every registered manifest (kind
    /// `InKernel`) demands.
    fn kernel_loader_caps() -> CapabilitySet {
        caps(&[CapabilityId::DRV_LOAD, CapabilityId::DRV_KERNEL])
    }

    #[test]
    fn every_in_kernel_driver_admits_through_the_signed_gate() {
        // Each baked image must verify against the embedded trust anchor
        // and clear the full pipeline: this proves the build signed every
        // registered driver with the key the kernel embeds (`plans/PI.md`
        // P10 5c-ii).
        let sink = NullSink;
        let loader = KernelDriverLoader::new(&sink).expect("embedded signer key is valid");
        let caps = kernel_loader_caps();
        for driver in &IN_KERNEL_DRIVERS {
            loader
                .admit(driver.path, &caps)
                .unwrap_or_else(|e| panic!("{} must admit through the gate: {e:?}", driver.path));
        }
    }

    #[test]
    fn admission_without_cap_drv_load_fails_closed() {
        let sink = NullSink;
        let loader = KernelDriverLoader::new(&sink).expect("embedded signer key is valid");
        // No CAP_DRV_LOAD: the hard gate refuses before any image work.
        let err = loader
            .admit(IN_KERNEL_DRIVERS[0].path, &CapabilitySet::empty())
            .expect_err("missing CAP_DRV_LOAD must fail closed");
        assert!(matches!(err, HostError::LoadCapabilityMissing));
    }

    #[test]
    fn in_kernel_driver_without_cap_drv_kernel_fails_closed() {
        let sink = NullSink;
        let loader = KernelDriverLoader::new(&sink).expect("embedded signer key is valid");
        // CAP_DRV_LOAD but not CAP_DRV_KERNEL: every registered manifest is
        // `kind = InKernel`, so the gate refuses.
        let err = loader
            .admit(IN_KERNEL_DRIVERS[1].path, &caps(&[CapabilityId::DRV_LOAD]))
            .expect_err("InKernel without CAP_DRV_KERNEL must fail closed");
        assert!(matches!(err, HostError::KernelKindForbidden));
    }

    #[test]
    fn an_unknown_path_is_refused() {
        let sink = NullSink;
        let loader = KernelDriverLoader::new(&sink).expect("embedded signer key is valid");
        let err = loader
            .admit("/System/Drivers/not_in_kernel", &kernel_loader_caps())
            .expect_err("an unknown path has no registered driver");
        assert!(matches!(err, HostError::UnknownDriver));
    }
}
