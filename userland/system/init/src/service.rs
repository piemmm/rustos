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

use alloc::collections::VecDeque;
use alloc::string::String;
use alloc::vec::Vec;
use core::cell::RefCell;

use tairix_abi::{
    decode_capability_ids, ActivationMode, CapabilityId, Duration64, Errno, ManifestHeader,
    ReadinessKind, ReadyCondition, RestartPolicy, ServiceLimit, ServiceManifest,
    MANIFEST_MAX_CAPABILITIES,
};
use tairix_caps::CapabilitySet;

use crate::registry::{validate_service_name, EnrolError};

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
    limits: Vec<ServiceLimit>,
    watchdog: Duration64,
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
            limits: Vec::new(),
            watchdog: Duration64::ZERO,
        }
    }

    /// Build a [`ServiceSpec`] from a service's decoded signed unit-metadata
    /// record (`plans/NEW-SERVICEMANAGER.md` §3.1 discovery).
    ///
    /// This is the bridge from *discovery* — a bundle scanned off
    /// `/System/Services` whose [`ServiceManifest`] has been decoded and
    /// validated by the ABI layer — to the *registration* the manager
    /// consumes ([`Init::register_enrolled`](crate::Init::register_enrolled)).
    /// The caller supplies the service's `name` and its `binary_path` (the
    /// loader knows both from the discovered bundle); every other field —
    /// account, readiness, activation, restart policy, stop grace, connect
    /// capability, dependencies, and the required/provided readiness
    /// conditions — comes from the signed manifest, so a tampered unit
    /// setting is a load refusal upstream, never a silent behaviour change
    /// here.
    ///
    /// The manifest's *structure* was already validated when it was decoded;
    /// this additionally applies the manager's **name policy**
    /// ([`validate_service_name`]) to the service name and to every
    /// dependency name, so a manifest can never smuggle a path-traversal- or
    /// case-collision-shaped dependency into the dependency graph. The
    /// structural [`ServiceManifest`] name bound is looser than this policy on
    /// purpose; the policy is the one authoritative check, applied here rather
    /// than duplicated.
    ///
    /// # Errors
    ///
    /// [`EnrolError::NameEmpty`], [`EnrolError::NameTooLong`], or
    /// [`EnrolError::NameInvalid`] if the service name or any dependency name
    /// violates the name policy. Fails closed — no partial spec is produced.
    pub fn from_manifest(
        name: impl Into<String>,
        binary_path: impl Into<String>,
        manifest: &ServiceManifest<'_>,
    ) -> Result<Self, EnrolError> {
        let name = name.into();
        validate_service_name(&name)?;
        let mut dependencies: Vec<String> = Vec::new();
        for dependency in manifest.dependencies() {
            validate_service_name(dependency)?;
            dependencies.push(String::from(dependency));
        }
        let requires: Vec<ReadyCondition> = manifest.requires().collect();
        let provides: Vec<ReadyCondition> = manifest.provides().collect();
        let mut spec = Self::new(name, binary_path, manifest.account(), dependencies)
            .with_readiness(manifest.readiness())
            .with_activation(manifest.activation())
            .with_restart(manifest.restart())
            .with_stop_grace(manifest.stop_grace())
            .with_watchdog(manifest.watchdog())
            .requiring(requires)
            .providing(provides)
            .with_limits(manifest.limits().collect::<Vec<ServiceLimit>>());
        if let Some(capability) = manifest.connect_capability() {
            spec = spec.with_connect_capability(capability);
        }
        Ok(spec)
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

    /// Set the per-service resource limits, consuming and returning `self`.
    ///
    /// The limits are unit metadata the manager threads to the kernel at
    /// spawn, where they are enforced; the manager itself does not interpret
    /// them. An empty list (the default) leaves the service governed by the
    /// discovered, growable default policy.
    #[must_use]
    pub fn with_limits(mut self, limits: impl Into<Vec<ServiceLimit>>) -> Self {
        self.limits = limits.into();
        self
    }

    /// Set the liveness-watchdog interval (the analogue of systemd's
    /// `WatchdogSec`), consuming and returning `self`.
    ///
    /// A running service that opts in must renew a heartbeat to the manager
    /// at least this often ([`Init::heartbeat`](crate::Init::heartbeat)); if
    /// it does not, the manager concludes its process has wedged, forces it
    /// down, and applies its [`RestartPolicy`] exactly as for any other
    /// unexpected exit. [`Duration64::ZERO`] (the default) disables the
    /// watchdog.
    #[must_use]
    pub fn with_watchdog(mut self, watchdog: Duration64) -> Self {
        self.watchdog = watchdog;
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

    /// The liveness-watchdog interval, or [`Duration64::ZERO`] if this service
    /// opts out of the liveness watchdog. See [`with_watchdog`](Self::with_watchdog).
    #[must_use]
    pub fn watchdog(&self) -> Duration64 {
        self.watchdog
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

    /// The per-service resource limits the kernel enforces at spawn, in
    /// strictly ascending [`tairix_abi::LimitKind`] order (empty if the
    /// service imposes none).
    #[must_use]
    pub fn limits(&self) -> &[ServiceLimit] {
        &self.limits
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

/// The live [`Reaper`] PID 1's supervision loop fills.
///
/// PID 1 does exactly one `wait`-any per loop turn and learns of one exited
/// child at a time, whereas [`Init::reap`](crate::Init::reap) *pulls* from a
/// [`Reaper`] until it drains. This mailbox bridges the two without a second
/// `wait`: the loop [`deposit`](Self::deposit)s each child that is not one of
/// its own login sessions, then calls `reap`, which drains exactly that child
/// (a known service exit or an inherited orphan) and stops.
///
/// It never itself blocks or waits — the kernel `wait` the loop already made
/// is the only wait — so it is not a busy-poll: [`collect`](Reaper::collect)
/// simply pops the queue and returns `None` when empty. The queue is a
/// [`VecDeque`] (not a single slot) purely so a future loop that harvests a
/// burst of exits in one turn cannot lose one; today the loop deposits one at
/// a time.
///
/// Interior mutability ([`RefCell`]) is required because [`Init`](crate::Init)
/// borrows its reaper as `&dyn Reaper` (shared) for its whole lifetime while
/// the loop must still push into it. Single-threaded PID 1 never re-enters a
/// borrow, so the `RefCell` can never observe a conflicting borrow.
#[derive(Debug, Default)]
pub struct LoopReaper {
    pending: RefCell<VecDeque<ReapedChild>>,
}

impl LoopReaper {
    /// Create an empty mailbox.
    #[must_use]
    pub fn new() -> Self {
        Self {
            pending: RefCell::new(VecDeque::new()),
        }
    }

    /// Record one exited child for the next [`Init::reap`](crate::Init::reap)
    /// to drain. Called by PID 1's loop for a reaped pid that is not one of
    /// its own login sessions.
    pub fn deposit(&self, child: ReapedChild) {
        self.pending.borrow_mut().push_back(child);
    }
}

impl Reaper for LoopReaper {
    fn collect(&self) -> Option<ReapedChild> {
        self.pending.borrow_mut().pop_front()
    }
}

#[cfg(test)]
mod tests {
    use super::{LoopReaper, Pid, ReapedChild, Reaper, ServiceSpec};
    use alloc::vec;
    use alloc::vec::Vec;

    #[test]
    fn loop_reaper_yields_deposited_children_in_order_then_none() {
        let reaper = LoopReaper::new();
        assert_eq!(reaper.collect(), None);
        reaper.deposit(ReapedChild {
            pid: Pid::new(10),
            exit_code: 0,
        });
        reaper.deposit(ReapedChild {
            pid: Pid::new(20),
            exit_code: 3,
        });
        assert_eq!(
            reaper.collect(),
            Some(ReapedChild {
                pid: Pid::new(10),
                exit_code: 0,
            })
        );
        assert_eq!(
            reaper.collect(),
            Some(ReapedChild {
                pid: Pid::new(20),
                exit_code: 3,
            })
        );
        // Drained: a `reap` that keeps pulling terminates.
        assert_eq!(reaper.collect(), None);
    }

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

    #[test]
    fn from_manifest_builds_a_spec_from_signed_unit_metadata() {
        use tairix_abi::{
            ActivationMode, CapabilityId, Duration64, LimitKind, ReadinessKind, ReadyCondition,
            ResourceLimit, RestartPolicy, ServiceLimit, ServiceManifest, ServiceUnit,
        };

        let limits = [
            ServiceLimit {
                kind: LimitKind::OpenStreams,
                limit: ResourceLimit::new(32, 64).expect("well-formed"),
            },
            ServiceLimit {
                kind: LimitKind::Processes,
                limit: ResourceLimit::UNLIMITED,
            },
        ];
        let unit = ServiceUnit {
            account: 12,
            readiness: ReadinessKind::Notify,
            activation: ActivationMode::on_demand(Duration64::from_secs(20)),
            restart: RestartPolicy::OnFailure,
            stop_grace: Duration64::from_secs(9),
            connect_capability: Some(CapabilityId::SYSINFO_GLOBAL),
            requires: &[ReadyCondition::NetworkUp],
            provides: &[ReadyCondition::SeatAvailable],
            dependencies: &["netstack", "sysinfod"],
            limits: &limits,
            watchdog: Duration64::from_secs(15),
        };
        let mut buf = [0u8; 256];
        let len = unit.encode(&mut buf).expect("encode");
        let manifest = ServiceManifest::from_bytes(&buf[..len]).expect("decode");

        let spec = ServiceSpec::from_manifest("fontd", "/System/Services/fontd.app/Run", &manifest)
            .expect("well-formed manifest builds a spec");
        assert_eq!(spec.name(), "fontd");
        assert_eq!(spec.binary_path(), "/System/Services/fontd.app/Run");
        assert_eq!(spec.account(), 12);
        assert_eq!(spec.readiness(), ReadinessKind::Notify);
        assert_eq!(
            spec.activation(),
            ActivationMode::on_demand(Duration64::from_secs(20))
        );
        assert_eq!(spec.restart(), RestartPolicy::OnFailure);
        assert_eq!(spec.stop_grace(), Duration64::from_secs(9));
        assert_eq!(
            spec.connect_capability(),
            Some(CapabilityId::SYSINFO_GLOBAL)
        );
        assert_eq!(spec.dependencies(), &["netstack", "sysinfod"]);
        assert_eq!(spec.requires(), &[ReadyCondition::NetworkUp]);
        assert_eq!(spec.provides(), &[ReadyCondition::SeatAvailable]);
        assert_eq!(spec.limits(), &limits);
        assert_eq!(spec.watchdog(), Duration64::from_secs(15));
    }

    #[test]
    fn from_manifest_applies_the_name_policy_to_the_service_and_dependencies() {
        use tairix_abi::{
            ActivationMode, Duration64, ReadinessKind, ReadyCondition, RestartPolicy,
            ServiceManifest, ServiceUnit,
        };

        // A manifest whose *structure* is valid but whose dependency name
        // violates the manager's name policy (a path-traversal shape) is
        // refused: the structural ABI bound is looser than the policy, and
        // the policy is applied here, fail closed.
        let bad_dep = ServiceUnit {
            account: 0,
            readiness: ReadinessKind::Immediate,
            activation: ActivationMode::Permanent,
            restart: RestartPolicy::Never,
            stop_grace: Duration64::ZERO,
            connect_capability: None,
            requires: &[] as &[ReadyCondition],
            provides: &[] as &[ReadyCondition],
            dependencies: &["../escape"],
            limits: &[],
            watchdog: Duration64::ZERO,
        };
        let mut buf = [0u8; 128];
        let len = bad_dep.encode(&mut buf).expect("encode");
        let manifest = ServiceManifest::from_bytes(&buf[..len]).expect("decode");
        assert_eq!(
            ServiceSpec::from_manifest("svc", "/System/Services/svc.app/Run", &manifest),
            Err(super::EnrolError::NameInvalid),
        );

        // An invalid *service* name is likewise refused, before the spec is
        // assembled.
        let ok = ServiceUnit {
            dependencies: &[] as &[&str],
            ..bad_dep
        };
        let len = ok.encode(&mut buf).expect("encode");
        let manifest = ServiceManifest::from_bytes(&buf[..len]).expect("decode");
        assert_eq!(
            ServiceSpec::from_manifest("Bad Name", "/System/Services/x.app/Run", &manifest),
            Err(super::EnrolError::NameInvalid),
        );
    }
}
