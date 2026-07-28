//! Service descriptions and the seams init uses to reach the outside world.
//!
//! A [`ServiceSpec`] is the static description of one system service: its
//! name, the binary that runs it, the signed manifest that declares the
//! capabilities it requests, and the services it must start after. The
//! [`Spawner`] and [`Reaper`] traits are the two seams through which the
//! otherwise-pure [`Init`](crate::Init) manager launches a verified binary
//! and learns that a child has exited. On a running kernel they are backed
//! by syscalls; in tests they are in-memory fixtures.

use alloc::string::String;
use alloc::vec::Vec;

use tairix_abi::{
    decode_capability_ids, CapabilityId, Errno, ManifestHeader, ReadinessKind, ReadyCondition,
    MANIFEST_MAX_CAPABILITIES,
};
use tairix_caps::CapabilitySet;

/// Decode a service binary's signed manifest into the capability set it
/// requests.
///
/// This is the one place a manifest's [`ManifestHeader`] prefix and
/// capability body are turned into a [`CapabilitySet`]; both the bring-up
/// path ([`Init::start_all`](crate::Init::start_all), which intersects the
/// request with the system authority) and the enrolment path
/// ([`registry::enrol`](crate::registry::enrol), which refuses a request that
/// exceeds the enroller's ceiling) decode through it, so the two can never
/// disagree about what a manifest asks for (no second copy of the decode).
///
/// It does **not** verify the manifest's signature — that is the
/// [`Spawner`]'s job at launch time; this reads only the already-trusted
/// bytes to learn the *requested* authority.
///
/// # Errors
///
/// * [`Errno::AbiVersionUnsupported`] if the manifest targets an ABI version
///   other than `accepted_abi_version`.
/// * The [`Errno`] from [`ManifestHeader::from_bytes`] /
///   [`decode_capability_ids`] if the header or capability body is malformed
///   or truncated. Every failure is fail-closed: a manifest that does not
///   decode cleanly grants nothing.
pub fn decode_manifest_capabilities(
    manifest: &[u8],
    accepted_abi_version: u32,
) -> Result<CapabilitySet, Errno> {
    let header = ManifestHeader::from_bytes(manifest)?;
    if header.abi_version != accepted_abi_version {
        return Err(Errno::AbiVersionUnsupported);
    }
    let count = usize::from(header.capability_count);
    let body = manifest
        .get(ManifestHeader::WIRE_LEN..)
        .ok_or(Errno::BufferTooSmall)?;
    let mut scratch = [CapabilityId::FS_MOUNT; MANIFEST_MAX_CAPABILITIES as usize];
    let decoded = decode_capability_ids(body, count, &mut scratch)?;
    let mut set = CapabilitySet::empty();
    for cap in &scratch[..decoded] {
        set.insert(*cap);
    }
    Ok(set)
}

/// Process identifier issued by the kernel when a service is spawned.
///
/// A newtype rather than a bare `u64` so that a PID cannot be confused with
/// any other identifier init handles (capability ids, event ids).
#[repr(transparent)]
#[derive(Copy, Clone, Debug, Eq, PartialEq, Hash, Ord, PartialOrd)]
pub struct Pid(u64);

impl Pid {
    /// Construct a [`Pid`] from its raw kernel value.
    #[must_use]
    pub const fn new(raw: u64) -> Self {
        Self(raw)
    }

    /// Raw kernel value.
    #[must_use]
    pub const fn as_u64(self) -> u64 {
        self.0
    }
}

/// Static description of one long-running system service.
///
/// The `manifest` is the signed manifest bytes of the service binary (the
/// [`ManifestHeader`] prefix followed by its
/// capability body). [`Init`](crate::Init) decodes it to learn the
/// capabilities the service requests; it does **not** verify the signature
/// — that is the [`Spawner`]'s responsibility at launch time.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ServiceSpec {
    name: String,
    binary_path: String,
    manifest: Vec<u8>,
    dependencies: Vec<String>,
    readiness: ReadinessKind,
    requires: Vec<ReadyCondition>,
    provides: Vec<ReadyCondition>,
}

impl ServiceSpec {
    /// Describe a service.
    ///
    /// The service is created with the default readiness kind
    /// ([`ReadinessKind::Immediate`]) and no required or provided readiness
    /// conditions; use [`with_readiness`](Self::with_readiness),
    /// [`requiring`](Self::requiring), and [`providing`](Self::providing) to
    /// set them.
    ///
    /// * `name` — unique identifier used to express dependencies and to
    ///   label audit records.
    /// * `binary_path` — logical path of the service binary handed to the
    ///   [`Spawner`] (e.g. `/System/Services/sysinfod.app/Run`).
    /// * `manifest` — the service binary's signed manifest bytes.
    /// * `dependencies` — names of services that must be started before
    ///   this one.
    #[must_use]
    pub fn new(
        name: impl Into<String>,
        binary_path: impl Into<String>,
        manifest: impl Into<Vec<u8>>,
        dependencies: impl Into<Vec<String>>,
    ) -> Self {
        Self {
            name: name.into(),
            binary_path: binary_path.into(),
            manifest: manifest.into(),
            dependencies: dependencies.into(),
            readiness: ReadinessKind::Immediate,
            requires: Vec::new(),
            provides: Vec::new(),
        }
    }

    /// Set how this service reaches readiness (spawn-implies-ready versus
    /// notify), consuming and returning `self` for chaining.
    #[must_use]
    pub fn with_readiness(mut self, readiness: ReadinessKind) -> Self {
        self.readiness = readiness;
        self
    }

    /// Declare the named readiness conditions that must be satisfied before
    /// this service may start, consuming and returning `self`.
    #[must_use]
    pub fn requiring(mut self, requires: impl Into<Vec<ReadyCondition>>) -> Self {
        self.requires = requires.into();
        self
    }

    /// Declare the named readiness conditions this service satisfies once it
    /// becomes ready, consuming and returning `self`.
    #[must_use]
    pub fn providing(mut self, provides: impl Into<Vec<ReadyCondition>>) -> Self {
        self.provides = provides.into();
        self
    }

    /// Unique service name.
    #[must_use]
    pub fn name(&self) -> &str {
        &self.name
    }

    /// Logical path of the service binary.
    #[must_use]
    pub fn binary_path(&self) -> &str {
        &self.binary_path
    }

    /// Signed manifest bytes of the service binary.
    #[must_use]
    pub fn manifest(&self) -> &[u8] {
        &self.manifest
    }

    /// Names of services that must start before this one.
    #[must_use]
    pub fn dependencies(&self) -> &[String] {
        &self.dependencies
    }

    /// How this service reaches readiness.
    #[must_use]
    pub fn readiness(&self) -> ReadinessKind {
        self.readiness
    }

    /// The named readiness conditions that gate this service's start.
    #[must_use]
    pub fn requires(&self) -> &[ReadyCondition] {
        &self.requires
    }

    /// The named readiness conditions this service satisfies once ready.
    #[must_use]
    pub fn provides(&self) -> &[ReadyCondition] {
        &self.provides
    }
}

/// Launches a verified service binary with a fixed capability ceiling.
///
/// The implementation owns the trusted load pipeline (`rxe` envelope
/// decode, signature verification, syscall-table-hash match — the same
/// checks `drvhost` runs for drivers) and executes the
/// binary with **at most** the `granted` capability set. [`Init`](crate::Init)
/// has already intersected the manifest's request with the system authority,
/// so `granted` is the ceiling, never a floor: the spawner must not add to
/// it (no ambient authority).
pub trait Spawner {
    /// Launch `spec`'s binary with the capability set `granted`.
    ///
    /// # Errors
    ///
    /// Returns the implementation's [`Errno`] verbatim if the binary cannot
    /// be loaded, verified, or executed. [`Init`](crate::Init) records it as
    /// [`StartFailure::SpawnFailed`](crate::StartFailure::SpawnFailed) and
    /// skips the dependents of the failed service.
    fn spawn(&self, spec: &ServiceSpec, granted: &CapabilitySet) -> Result<Pid, Errno>;
}

/// One child process that has exited and is ready to be reaped.
#[derive(Copy, Clone, Debug, Eq, PartialEq)]
pub struct ReapedChild {
    /// Identifier of the exited process.
    pub pid: Pid,
    /// Process exit code.
    pub exit_code: i32,
}

/// Source of exited-child notifications for PID 1.
///
/// Every PID 1 must reap the zombies of the whole system — both the
/// services it started and the orphans it inherits when their parent dies
/// (init owns `/System/Services`). The kernel-backed
/// implementation drains the wait queue; a test fixture returns a fixed
/// script.
pub trait Reaper {
    /// Return the next exited child, or `None` when none are pending.
    ///
    /// Must report each exited child exactly once and must return `None`
    /// once the pending set is drained, so [`Init::reap`](crate::Init::reap)
    /// terminates.
    fn collect(&self) -> Option<ReapedChild>;
}

#[cfg(test)]
mod tests {
    use super::{Pid, ReapedChild, ServiceSpec};
    use alloc::vec;
    use alloc::vec::Vec;

    #[test]
    fn pid_round_trips() {
        assert_eq!(Pid::new(42).as_u64(), 42);
    }

    #[test]
    fn service_spec_exposes_fields() {
        let deps: Vec<_> = vec!["a".into(), "b".into()];
        let spec = ServiceSpec::new("svc", "/System/Services/svc", vec![1u8, 2, 3], deps);
        assert_eq!(spec.name(), "svc");
        assert_eq!(spec.binary_path(), "/System/Services/svc");
        assert_eq!(spec.manifest(), &[1, 2, 3]);
        assert_eq!(spec.dependencies(), &["a", "b"]);
        // A bare spec is immediate-readiness with no conditions.
        assert_eq!(spec.readiness(), super::ReadinessKind::Immediate);
        assert!(spec.requires().is_empty());
        assert!(spec.provides().is_empty());
    }

    #[test]
    fn service_spec_readiness_builders_set_metadata() {
        use super::{ReadinessKind, ReadyCondition};
        let spec = ServiceSpec::new("net", "/System/Services/net", Vec::new(), Vec::new())
            .with_readiness(ReadinessKind::Notify)
            .requiring([ReadyCondition::FilesystemsMounted])
            .providing([ReadyCondition::NetworkUp]);
        assert_eq!(spec.readiness(), ReadinessKind::Notify);
        assert_eq!(spec.requires(), &[ReadyCondition::FilesystemsMounted]);
        assert_eq!(spec.provides(), &[ReadyCondition::NetworkUp]);
    }

    #[test]
    fn reaped_child_is_plain_data() {
        let c = ReapedChild {
            pid: Pid::new(7),
            exit_code: -1,
        };
        assert_eq!(c.pid.as_u64(), 7);
        assert_eq!(c.exit_code, -1);
    }
}
