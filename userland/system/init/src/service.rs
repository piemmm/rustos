//! Service descriptions and the seams init uses to reach the outside world.
//!
//! A [`ServiceSpec`] is the static description of one system service: its
//! name, the binary that runs it, the service account it runs as, and the
//! services it must start after. The [`Spawner`] and [`Reaper`] traits are
//! the two seams through which the otherwise-pure [`Init`](crate::Init)
//! manager launches a verified binary and learns that a child has exited. On
//! a running kernel they are backed by syscalls; in tests they are in-memory
//! fixtures.
//!
//! The **kernel is the single authority over a service's capabilities**: it
//! reads the service binary's signed bundle manifest at load time and grants
//! the intersection of the manifest's request with the service account's
//! ceiling. The manager names only *what* to run and *as whom* (the
//! [`ServiceSpec::account`] uid); it never decodes a manifest or computes a
//! grant on the launch path, so there is no second capability-derivation
//! path to drift from the kernel's authoritative one.

use alloc::string::String;
use alloc::vec::Vec;

use tairix_abi::{
    decode_capability_ids, ActivationMode, CapabilityId, Duration64, Errno, ManifestHeader,
    ReadinessKind, ReadyCondition, RestartPolicy, MANIFEST_MAX_CAPABILITIES,
};
use tairix_caps::CapabilitySet;

/// Default graceful-stop grace period a service is given to exit on its own
/// before the manager force-terminates it, when its manifest declares none.
///
/// This is a policy default, not a resource capacity: it is the interval a
/// well-behaved service needs to flush and exit cleanly. A service with a
/// longer or shorter shutdown declares its own grace in its manifest.
pub const DEFAULT_STOP_GRACE: Duration64 = Duration64::from_secs(5);

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
/// `account` is the uid of the compiled-in service account the service runs
/// as. The kernel resolves that account's capability ceiling and grants the
/// service `bundle-manifest ∩ ceiling` at load time, so the [`ServiceSpec`]
/// carries no manifest bytes: the manager names the binary and the account,
/// and the kernel — the single capability authority — decides the grant.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ServiceSpec {
    name: String,
    binary_path: String,
    account: u32,
    dependencies: Vec<String>,
    readiness: ReadinessKind,
    requires: Vec<ReadyCondition>,
    provides: Vec<ReadyCondition>,
    activation: ActivationMode,
    restart: RestartPolicy,
    stop_grace: Duration64,
    connect_capability: Option<CapabilityId>,
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
    /// * `account` — uid of the service account the service runs as; the
    ///   kernel resolves its capability ceiling at load time.
    /// * `dependencies` — names of services that must be started before
    ///   this one.
    #[must_use]
    pub fn new(
        name: impl Into<String>,
        binary_path: impl Into<String>,
        account: u32,
        dependencies: impl Into<Vec<String>>,
    ) -> Self {
        Self {
            name: name.into(),
            binary_path: binary_path.into(),
            account,
            dependencies: dependencies.into(),
            readiness: ReadinessKind::Immediate,
            requires: Vec::new(),
            provides: Vec::new(),
            activation: ActivationMode::Permanent,
            restart: RestartPolicy::Never,
            stop_grace: DEFAULT_STOP_GRACE,
            connect_capability: None,
        }
    }

    /// Set the service's activation mode (permanent versus on-demand with an
    /// idle-linger span), consuming and returning `self` for chaining.
    #[must_use]
    pub fn with_activation(mut self, activation: ActivationMode) -> Self {
        self.activation = activation;
        self
    }

    /// Set the graceful-stop grace period this service is given to exit on
    /// its own before a forced terminate, consuming and returning `self`.
    #[must_use]
    pub fn with_stop_grace(mut self, stop_grace: Duration64) -> Self {
        self.stop_grace = stop_grace;
        self
    }

    /// Set what the manager does when this service's process exits
    /// (never / on abnormal exit / always), consuming and returning `self`.
    ///
    /// The default is [`RestartPolicy::Never`]: a service is brought back
    /// only when its manifest asks for it. A restart is always bounded by
    /// the manager's crash-loop budget and backoff — never a blind retry.
    #[must_use]
    pub fn with_restart(mut self, restart: RestartPolicy) -> Self {
        self.restart = restart;
        self
    }

    /// Set the capability a client must hold to connect to this service's
    /// reserved endpoint, consuming and returning `self`.
    ///
    /// `None` (the default) means the endpoint requires no capability beyond
    /// being a principal that can address the manager — the right choice for
    /// a broadly shared helper like the font service. A service guarding a
    /// privileged endpoint names the capability here, and the manager
    /// refuses a connect from a client that does not hold it before touching
    /// any state (fail closed).
    #[must_use]
    pub fn with_connect_capability(mut self, capability: CapabilityId) -> Self {
        self.connect_capability = Some(capability);
        self
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

    /// The uid of the service account this service runs as; the kernel
    /// resolves its capability ceiling and derives the grant at load time.
    #[must_use]
    pub fn account(&self) -> u32 {
        self.account
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

    /// How this service is activated — permanent, or on-demand with an
    /// idle-linger span.
    #[must_use]
    pub fn activation(&self) -> ActivationMode {
        self.activation
    }

    /// What the manager does when this service's process exits — never,
    /// on an abnormal exit only, or always.
    #[must_use]
    pub fn restart(&self) -> RestartPolicy {
        self.restart
    }

    /// The graceful-stop grace period this service is given to exit on its
    /// own before a forced terminate.
    #[must_use]
    pub fn stop_grace(&self) -> Duration64 {
        self.stop_grace
    }

    /// The capability a client must hold to connect to this service's
    /// reserved endpoint, or `None` if the endpoint requires none.
    #[must_use]
    pub fn connect_capability(&self) -> Option<CapabilityId> {
        self.connect_capability
    }
}

/// Launches a verified service binary as its own service account.
///
/// The implementation owns the trusted load pipeline (`rxe` envelope decode,
/// signature verification, syscall-table-hash match — the same checks
/// `drvhost` runs for drivers) and switches the child onto the service
/// account [`ServiceSpec::account`] names. The **kernel** is the single
/// capability authority: at load time it reads the signed bundle manifest and
/// grants the intersection of the manifest's request with that account's
/// ceiling. The manager therefore names only *what* to run and *as whom*; it
/// never computes or passes a capability set, so no second, divergent
/// capability-derivation path can fall out of step with the kernel's (and the
/// account's ceiling still bounds the grant — no ambient authority).
pub trait Spawner {
    /// Launch `spec`'s binary as the service account it names.
    ///
    /// # Errors
    ///
    /// Returns the implementation's [`Errno`] verbatim if the binary cannot
    /// be loaded, verified, or executed. [`Init`](crate::Init) records it as
    /// [`StartFailure::SpawnFailed`](crate::StartFailure::SpawnFailed) and
    /// skips the dependents of the failed service.
    fn spawn(&self, spec: &ServiceSpec) -> Result<Pid, Errno>;
}

/// Identifier of a client connected (or waiting to connect) to a service's
/// reserved endpoint through on-demand activation.
///
/// Like [`Pid`] it is a newtype rather than a bare `u64` so a connection id
/// cannot be confused with any other identifier. It is issued and attested
/// by the kernel/IPC layer for the connecting principal, never chosen by the
/// client, so a client can only ever refer to its *own* connection when it
/// asks the manager to connect or disconnect.
#[repr(transparent)]
#[derive(Copy, Clone, Debug, Eq, PartialEq, Hash, Ord, PartialOrd)]
pub struct ClientId(u64);

impl ClientId {
    /// Construct a [`ClientId`] from its raw kernel-attested value.
    #[must_use]
    pub const fn new(raw: u64) -> Self {
        Self(raw)
    }

    /// Raw kernel-attested value.
    #[must_use]
    pub const fn as_u64(self) -> u64 {
        self.0
    }
}

/// Stops a running service that the manager supervises.
///
/// Idle-stop and shutdown are two-phase and never a blind kill: the manager
/// first asks the service to exit on its own ([`request_stop`](Self::request_stop),
/// the graceful phase), and only if it has not exited within its grace
/// period does it force the process down ([`force_terminate`](Self::force_terminate)).
/// On a running kernel both are backed by the `signal` syscall; in tests
/// they are recorded by an in-memory fixture.
pub trait Stopper {
    /// Ask the service running as `pid` to exit gracefully (the analogue of
    /// `SIGTERM`). The service is expected to flush and exit within its
    /// grace period, at which point the [`Reaper`] observes the exit.
    ///
    /// # Errors
    ///
    /// Returns the implementation's [`Errno`] verbatim if the request could
    /// not be delivered (for example the process is already gone).
    fn request_stop(&self, pid: Pid) -> Result<(), Errno>;

    /// Force the service running as `pid` down immediately (the analogue of
    /// `SIGKILL`), used only after the graceful grace period has elapsed
    /// without the service exiting.
    ///
    /// # Errors
    ///
    /// Returns the implementation's [`Errno`] verbatim if the process could
    /// not be terminated (for example it has already exited).
    fn force_terminate(&self, pid: Pid) -> Result<(), Errno>;
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
        let spec = ServiceSpec::new("svc", "/System/Services/svc", 10, deps);
        assert_eq!(spec.name(), "svc");
        assert_eq!(spec.binary_path(), "/System/Services/svc");
        assert_eq!(spec.account(), 10);
        assert_eq!(spec.dependencies(), &["a", "b"]);
        // A bare spec is immediate-readiness with no conditions.
        assert_eq!(spec.readiness(), super::ReadinessKind::Immediate);
        assert!(spec.requires().is_empty());
        assert!(spec.provides().is_empty());
    }

    #[test]
    fn service_spec_readiness_builders_set_metadata() {
        use super::{ReadinessKind, ReadyCondition};
        let spec = ServiceSpec::new("net", "/System/Services/net", 11, Vec::new())
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
