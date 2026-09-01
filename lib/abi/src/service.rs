//! Service-lifecycle and readiness vocabulary shared between a long-running
//! service, the service manager (PID 1 `init` and the per-user managers),
//! and the future status query (`plans/NEW-SERVICEMANAGER.md`).
//!
//! A service manager that treats "spawned" as "up" cannot honestly start a
//! dependent that needs "the network is up" — the dependency is merely
//! *running*, not yet *ready*. This module owns the three frozen pieces that
//! close that gap:
//!
//! * [`ServiceState`] — the manager's per-service lifecycle
//!   (`inactive → starting → ready → running → stopping → stopped | failed`).
//!   It is the vocabulary the engine tracks and the status API reports; a
//!   dependent is released only when its dependency is [`ServiceState::is_ready`].
//! * [`ReadyCondition`] — the closed set of **named readiness conditions /
//!   targets** (`network-up`, `filesystems-mounted`, …). A service declares
//!   the conditions it requires; the manager releases it only once all are
//!   satisfied. This generalises the headless case: a GUI-only service that
//!   requires `display-present` simply never activates on a headless boot,
//!   because nothing ever satisfies that condition.
//! * [`ReadyNotice`] + [`LifecycleSignal`] — the readiness-notification wire
//!   record (an `sd_notify` analogue): a service announces "I am ready" (or
//!   "I have failed to come up") to the manager over its supervised channel.
//!   The notice carries **no service identity** — the manager binds the
//!   report to the kernel-attested sender of the message, never a
//!   caller-supplied name — so a service can announce only its own
//!   transition, never another's.
//!
//! The module is `no_std`, allocation-free, and operates on borrowed byte
//! slices, so the same code runs in the kernel, in a user-space manager, and
//! in a WebAssembly userland binary unchanged. Like every `lib/abi` surface
//! it is versioned and freezes on the first release; `abi-v1` is not frozen
//! yet, so the enums may still grow in place (a new named condition, a new
//! self-announceable signal) rather than in an `abi-v2`.

use crate::le::{put_u16, put_u32, read_u16, read_u32};
use crate::rlimit::{LimitKind, ResourceLimit};
use crate::{CapabilityId, Duration64, Errno};

/// Magic number identifying an `abi-v1` service readiness notice (`"SVC1"`
/// little-endian).
pub const SERVICE_NOTICE_MAGIC: u32 = u32::from_le_bytes(*b"SVC1");

/// The `service-v1` readiness-protocol version.
pub const SERVICE_VERSION_V1: u16 = 1;

/// Directory holding the **administrator's** service enrolment overrides.
pub const SERVICE_OVERRIDES_DIR: &str = "/System/Settings/Services";

/// Absolute path of the administrator's service enrolment overrides: one
/// `<service> enabled|disabled` line per service whose enrolment differs from
/// the image's own.
///
/// On the writable encrypted root, so it is readable only after the root is
/// unlocked and holds only what was changed — a system update shipping a
/// different default then reaches every service the administrator has not
/// spoken about. It is the only enrolment layer on disk: no document under
/// `/System` is reliably readable at the instant the service manager must
/// decide what to bring up, so the image's own layer travels in the manager's
/// startup configuration instead.
pub const SERVICE_OVERRIDES_PATH: &str = "/System/Settings/Services/overrides";

/// Whether a service is **enrolled** — eligible to be brought up — or not.
///
/// The closed two-state vocabulary the enrolment record, the control protocol,
/// and the status query all speak, so the wire byte and the document word have
/// one definition rather than one per layer. It is deliberately *not* a
/// lifecycle state ([`ServiceState`]): enrolment says whether a service may
/// run at all, never whether it is running now.
#[repr(u8)]
#[derive(Copy, Clone, Debug, Eq, PartialEq, Hash)]
pub enum ServiceEnrolment {
    /// Eligible to be brought up.
    Enabled = 1,
    /// Not eligible; the manager never starts it.
    Disabled = 2,
}

impl ServiceEnrolment {
    /// The stable wire discriminant.
    #[must_use]
    pub const fn as_u8(self) -> u8 {
        self as u8
    }

    /// Classify a wire discriminant, or `None` outside the closed set (wire
    /// corruption — the caller fails closed).
    #[must_use]
    pub const fn from_u8(value: u8) -> Option<Self> {
        match value {
            1 => Some(Self::Enabled),
            2 => Some(Self::Disabled),
            _ => None,
        }
    }

    /// The stable word used in the enrolment documents and in status output.
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Enabled => "enabled",
            Self::Disabled => "disabled",
        }
    }

    /// Classify a document word, or `None` for anything else (fail closed).
    #[must_use]
    pub fn from_name(word: &str) -> Option<Self> {
        match word {
            "enabled" => Some(Self::Enabled),
            "disabled" => Some(Self::Disabled),
            _ => None,
        }
    }

    /// Whether this enrols the service.
    #[must_use]
    pub const fn is_enabled(self) -> bool {
        matches!(self, Self::Enabled)
    }
}

/// A point in one service's lifecycle, tracked by the manager and reported
/// through the status query.
///
/// The progression is monotonic during bring-up
/// (`Inactive → Starting → Ready → Running`) and, at teardown,
/// `Running → Stopping → Stopped`; `Failed` is the terminal state a service
/// reaches from `Starting` (it never signalled ready) or from a crash. A
/// dependent is admitted only once every dependency it names is
/// [`is_ready`](Self::is_ready).
#[repr(u8)]
#[derive(Copy, Clone, Debug, Eq, PartialEq, Hash, Ord, PartialOrd)]
pub enum ServiceState {
    /// Registered but not yet started: its dependencies or required
    /// readiness conditions are not all satisfied, so the manager has not
    /// spawned it. The fail-closed resting state — a service that can never
    /// be admitted (an unmet condition, a failed dependency) stays here.
    Inactive = 0,
    /// Spawned, but has not yet announced readiness. Its dependents are
    /// **not** released while it is here: "spawned" is not "ready".
    Starting = 1,
    /// Announced readiness ([`LifecycleSignal::Ready`]). The transient point
    /// at which the manager releases the service's dependents and any named
    /// conditions it provides; the manager then promotes it to
    /// [`Running`](Self::Running).
    Ready = 2,
    /// Ready and under steady-state supervision.
    Running = 3,
    /// A graceful stop is in progress: the manager has asked it to exit and
    /// is awaiting the grace period before a forced terminate.
    Stopping = 4,
    /// Exited cleanly (or completed a graceful stop). A terminal state.
    Stopped = 5,
    /// Failed to come up or crashed. A terminal state; the manager skips the
    /// dependents that were blocked on it.
    Failed = 6,
}

impl ServiceState {
    /// Every lifecycle state, in progression order.
    pub const ALL: [ServiceState; 7] = [
        ServiceState::Inactive,
        ServiceState::Starting,
        ServiceState::Ready,
        ServiceState::Running,
        ServiceState::Stopping,
        ServiceState::Stopped,
        ServiceState::Failed,
    ];

    /// The stable wire discriminant.
    #[must_use]
    pub const fn as_u8(self) -> u8 {
        self as u8
    }

    /// Classify a wire discriminant, or `None` if it is outside the closed
    /// set (wire corruption — the caller fails closed).
    #[must_use]
    pub const fn from_u8(value: u8) -> Option<Self> {
        match value {
            0 => Some(ServiceState::Inactive),
            1 => Some(ServiceState::Starting),
            2 => Some(ServiceState::Ready),
            3 => Some(ServiceState::Running),
            4 => Some(ServiceState::Stopping),
            5 => Some(ServiceState::Stopped),
            6 => Some(ServiceState::Failed),
            _ => None,
        }
    }

    /// The stable lowercase identifier used in audit records and status
    /// output.
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            ServiceState::Inactive => "inactive",
            ServiceState::Starting => "starting",
            ServiceState::Ready => "ready",
            ServiceState::Running => "running",
            ServiceState::Stopping => "stopping",
            ServiceState::Stopped => "stopped",
            ServiceState::Failed => "failed",
        }
    }

    /// `true` once the service has announced readiness — [`Ready`](Self::Ready)
    /// or [`Running`](Self::Running). This, not "has been spawned", is the
    /// gate a dependent waits on.
    #[must_use]
    pub const fn is_ready(self) -> bool {
        matches!(self, ServiceState::Ready | ServiceState::Running)
    }

    /// `true` for a terminal state — [`Stopped`](Self::Stopped) or
    /// [`Failed`](Self::Failed) — from which the service makes no further
    /// transition without being started again.
    #[must_use]
    pub const fn is_terminal(self) -> bool {
        matches!(self, ServiceState::Stopped | ServiceState::Failed)
    }
}

/// One of the closed set of **named readiness conditions** a service may
/// require before it starts, and that a providing service (or the manager)
/// satisfies.
///
/// Conditions decouple readiness from a specific service name: `netstack`
/// *provides* [`NetworkUp`](Self::NetworkUp) when it reports ready, and any
/// number of services may *require* it without naming `netstack`. A
/// condition no running configuration ever satisfies (for example
/// [`DisplayPresent`](Self::DisplayPresent) on a headless boot) simply keeps
/// its requiring services [`ServiceState::Inactive`] forever — the headless
/// case falls out of the model rather than being special-cased.
#[repr(u16)]
#[derive(Copy, Clone, Debug, Eq, PartialEq, Hash, Ord, PartialOrd)]
pub enum ReadyCondition {
    /// The network stack is up and can carry traffic (provided by
    /// `netstack`).
    NetworkUp = 0,
    /// The system's filesystems are mounted and writable.
    FilesystemsMounted = 1,
    /// Every boot-floor service has reached readiness — the point the
    /// manager considers the system booted.
    BootComplete = 2,
    /// A usable display is present (a display driver bound, a framebuffer
    /// available). Never satisfied on a headless boot.
    DisplayPresent = 3,
    /// A seat (a display plus its input devices) is available for a session
    /// (provided by `seatmgr`).
    SeatAvailable = 4,
}

impl ReadyCondition {
    /// Every named condition, in wire-discriminant order.
    pub const ALL: [ReadyCondition; 5] = [
        ReadyCondition::NetworkUp,
        ReadyCondition::FilesystemsMounted,
        ReadyCondition::BootComplete,
        ReadyCondition::DisplayPresent,
        ReadyCondition::SeatAvailable,
    ];

    /// The stable wire discriminant.
    #[must_use]
    pub const fn as_u16(self) -> u16 {
        self as u16
    }

    /// Classify a wire discriminant, or `None` if it is outside the closed
    /// set (wire corruption or an unknown future condition — fail closed).
    #[must_use]
    pub const fn from_u16(value: u16) -> Option<Self> {
        match value {
            0 => Some(ReadyCondition::NetworkUp),
            1 => Some(ReadyCondition::FilesystemsMounted),
            2 => Some(ReadyCondition::BootComplete),
            3 => Some(ReadyCondition::DisplayPresent),
            4 => Some(ReadyCondition::SeatAvailable),
            _ => None,
        }
    }

    /// The stable canonical name used in manifests, audit records, and
    /// status output.
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            ReadyCondition::NetworkUp => "network-up",
            ReadyCondition::FilesystemsMounted => "filesystems-mounted",
            ReadyCondition::BootComplete => "boot-complete",
            ReadyCondition::DisplayPresent => "display-present",
            ReadyCondition::SeatAvailable => "seat-available",
        }
    }

    /// Resolve a canonical condition name, or `None` if it names no known
    /// condition (fail closed — a manifest naming an unknown condition is
    /// refused, never silently ignored).
    #[must_use]
    pub fn from_name(name: &str) -> Option<Self> {
        Self::ALL.into_iter().find(|c| c.as_str() == name)
    }
}

/// How a service signals that it has finished starting — the analogue of
/// systemd's `Type=simple` versus `Type=notify`.
///
/// This is unit metadata (it will be read from a service's signed manifest):
/// it tells the manager whether reaching *ready* is implied by a successful
/// spawn or must be announced explicitly, so the manager never releases a
/// dependent against a service that is running but not yet actually up.
#[repr(u8)]
#[derive(Copy, Clone, Debug, Eq, PartialEq, Hash, Default)]
pub enum ReadinessKind {
    /// The service is considered [`ServiceState::Ready`] the instant its
    /// spawn succeeds; it sends no readiness notice. The default, and the
    /// right choice for a service with no startup work a dependent must wait
    /// for.
    #[default]
    Immediate = 0,
    /// The service stays [`ServiceState::Starting`] until it sends a
    /// [`ReadyNotice`] carrying [`LifecycleSignal::Ready`]. The manager
    /// releases its dependents and satisfies the conditions it provides only
    /// then, so a dependent that needs the service *functional* (not merely
    /// spawned) waits for the real thing.
    Notify = 1,
}

impl ReadinessKind {
    /// The stable wire discriminant.
    #[must_use]
    pub const fn as_u8(self) -> u8 {
        self as u8
    }

    /// Classify a wire discriminant, or `None` if it is outside the closed
    /// set (fail closed — an unknown readiness kind is a manifest defect).
    #[must_use]
    pub const fn from_u8(value: u8) -> Option<Self> {
        match value {
            0 => Some(ReadinessKind::Immediate),
            1 => Some(ReadinessKind::Notify),
            _ => None,
        }
    }

    /// The stable identifier used in manifests and status output.
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            ReadinessKind::Immediate => "immediate",
            ReadinessKind::Notify => "notify",
        }
    }
}

/// How the service manager decides *when* a service runs — the analogue of
/// systemd's permanently-enabled unit versus a socket-activated one.
///
/// This is unit metadata (it is read from a service's signed manifest,
/// `plans/NEW-SERVICEMANAGER.md` §3.5): it tells the manager whether a
/// service stays up for the whole life of the system or is started on demand
/// when a client first connects to its reserved endpoint and idle-stopped
/// again after a period with no connected clients. The linger period is
/// carried here rather than hard-coded, so the same engine serves a
/// permanent web server and a short-lived shared helper.
///
/// Like the rest of this module it is versioned and freezes on the first
/// release; `abi-v1` is not frozen yet, so the set may still grow in place.
#[repr(u8)]
#[derive(Copy, Clone, Debug, Eq, PartialEq, Hash, Default)]
pub enum ActivationMode {
    /// The service runs for the whole life of the system: the manager brings
    /// it up during boot and never idle-stops it. The right choice for a
    /// service that must always be reachable (a network stack, a web
    /// server). The default.
    #[default]
    Permanent,
    /// The service is started on demand — when a client first connects to
    /// its reserved endpoint — and idle-stopped once it has had no connected
    /// clients for `linger`. The right choice for a shared, sandboxed helper
    /// only needed while something is using it (the font service `fontd`).
    OnDemand {
        /// How long the manager waits, after the last client disconnects,
        /// before it idle-stops the service. The manager arms a single
        /// one-shot timer for it (never a poll); a new connection before it
        /// expires cancels the pending stop. A non-positive span means "stop
        /// as soon as idle".
        linger: Duration64,
    },
}

impl ActivationMode {
    /// Construct an on-demand mode with the given idle-linger span.
    #[must_use]
    pub const fn on_demand(linger: Duration64) -> Self {
        Self::OnDemand { linger }
    }

    /// `true` for [`OnDemand`](Self::OnDemand) — a service the manager may
    /// start on connect and idle-stop when its client count falls to zero.
    #[must_use]
    pub const fn is_on_demand(self) -> bool {
        matches!(self, Self::OnDemand { .. })
    }

    /// The idle-linger span for an [`OnDemand`](Self::OnDemand) service, or
    /// `None` for a [`Permanent`](Self::Permanent) one (which never lingers).
    #[must_use]
    pub const fn linger(self) -> Option<Duration64> {
        match self {
            Self::OnDemand { linger } => Some(linger),
            Self::Permanent => None,
        }
    }

    /// The stable identifier used in manifests, audit records, and status
    /// output.
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Permanent => "permanent",
            Self::OnDemand { .. } => "on-demand",
        }
    }
}

/// What the manager does when a supervised service's process exits — the
/// analogue of systemd's `Restart=` unit setting.
///
/// This is unit metadata (it is read from a service's signed manifest,
/// `plans/NEW-SERVICEMANAGER.md` §3.7): it tells the manager whether an
/// exited service is left down, brought back only after an abnormal exit, or
/// always brought back. A restart is never a blind retry-until-it-works: the
/// manager reuses the bounded crash-loop budget so a service that dies the
/// instant it starts is abandoned rather than relaunched forever, and each
/// relaunch waits a bounded, exponentially-growing backoff.
///
/// Like the rest of this module it is versioned and freezes on the first
/// release; `abi-v1` is not frozen yet, so the set may still grow in place.
#[repr(u8)]
#[derive(Copy, Clone, Debug, Eq, PartialEq, Hash, Default)]
pub enum RestartPolicy {
    /// The service is **never** restarted: once its process exits, for any
    /// reason, it stays down. The default — a service is brought back only
    /// when its manifest asks for it, never implicitly.
    #[default]
    Never = 0,
    /// The service is restarted only after an **abnormal** exit (a non-zero
    /// exit code or a crash), within the crash-loop budget and backoff. A
    /// clean exit (code 0) or a manager-initiated graceful stop is honoured
    /// and leaves the service down.
    OnFailure = 1,
    /// The service is **always** restarted when its process exits — clean or
    /// not — within the crash-loop budget and backoff. The right choice for a
    /// daemon whose clean exit is itself unexpected. A manager-initiated
    /// graceful stop (idle-stop, shutdown) is still honoured: the manager
    /// asked it to go, so it stays down.
    Always = 2,
}

impl RestartPolicy {
    /// Every policy, in wire-discriminant order.
    pub const ALL: [RestartPolicy; 3] = [
        RestartPolicy::Never,
        RestartPolicy::OnFailure,
        RestartPolicy::Always,
    ];

    /// The stable wire discriminant.
    #[must_use]
    pub const fn as_u8(self) -> u8 {
        self as u8
    }

    /// Classify a wire discriminant, or `None` if it is outside the closed
    /// set (fail closed — an unknown policy is a manifest defect).
    #[must_use]
    pub const fn from_u8(value: u8) -> Option<Self> {
        match value {
            0 => Some(RestartPolicy::Never),
            1 => Some(RestartPolicy::OnFailure),
            2 => Some(RestartPolicy::Always),
            _ => None,
        }
    }

    /// The stable identifier used in manifests, audit records, and status
    /// output.
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            RestartPolicy::Never => "never",
            RestartPolicy::OnFailure => "on-failure",
            RestartPolicy::Always => "always",
        }
    }

    /// Whether a service with this policy should be restarted after an exit
    /// with `exit_code` that the manager did **not** itself initiate.
    ///
    /// A manager-initiated graceful stop is never a candidate for restart and
    /// is handled by the caller before this is consulted; this decides only
    /// the *unexpected* exit of a still-wanted service.
    #[must_use]
    pub const fn should_restart(self, exit_code: i32) -> bool {
        match self {
            RestartPolicy::Never => false,
            RestartPolicy::OnFailure => exit_code != 0,
            RestartPolicy::Always => true,
        }
    }
}

/// The lifecycle transition a service may **announce about itself** through
/// a [`ReadyNotice`].
///
/// A service only ever reports its *own* progress, and only the transitions
/// it is the authority on: that it has come up ([`Ready`](Self::Ready)) or
/// that it has failed to ([`Failed`](Self::Failed)). Every other transition
/// in [`ServiceState`] is the manager's to make (it spawns, it stops, it
/// observes an exit), so it is deliberately not announceable — an illegal
/// self-report is unrepresentable rather than validated away.
#[repr(u8)]
#[derive(Copy, Clone, Debug, Eq, PartialEq, Hash)]
pub enum LifecycleSignal {
    /// "I am up." The manager transitions the sender `Starting → Ready`,
    /// releases its dependents and the conditions it provides, then promotes
    /// it to [`ServiceState::Running`].
    Ready = 1,
    /// "I could not come up." The manager transitions the sender
    /// `Starting → Failed` and skips the dependents blocked on it. A service
    /// that determines during start that it cannot proceed reports this
    /// rather than spinning or exiting silently.
    Failed = 2,
}

impl LifecycleSignal {
    /// The stable wire discriminant.
    #[must_use]
    pub const fn as_u8(self) -> u8 {
        self as u8
    }

    /// Classify a wire discriminant, or `None` if it is outside the closed
    /// set (a service may announce only these transitions — fail closed).
    #[must_use]
    pub const fn from_u8(value: u8) -> Option<Self> {
        match value {
            1 => Some(LifecycleSignal::Ready),
            2 => Some(LifecycleSignal::Failed),
            _ => None,
        }
    }

    /// The stable identifier used in audit records.
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            LifecycleSignal::Ready => "ready",
            LifecycleSignal::Failed => "failed",
        }
    }
}

/// A readiness notification a service sends to its manager — the `sd_notify`
/// analogue.
///
/// The frame carries only the [`LifecycleSignal`] the service announces
/// about itself. It carries **no identity**: the manager attributes the
/// notice to the kernel-attested sender of the message, never a field the
/// sender supplies, so one service can never announce another's readiness.
#[derive(Copy, Clone, Debug, Eq, PartialEq)]
pub struct ReadyNotice {
    /// The transition the sending service announces about itself.
    pub signal: LifecycleSignal,
}

impl ReadyNotice {
    /// Encoded size on the wire: magic (4), version (2), signal (1), and a
    /// reserved byte that must be zero.
    pub const WIRE_LEN: usize = 8;

    /// Wire offset of the version field.
    const OFF_VERSION: usize = 4;
    /// Wire offset of the signal discriminant.
    const OFF_SIGNAL: usize = 6;
    /// Wire offset of the reserved byte.
    const OFF_RESERVED: usize = 7;

    /// Build a notice announcing `signal`.
    #[must_use]
    pub const fn new(signal: LifecycleSignal) -> Self {
        Self { signal }
    }

    /// Encode `self` little-endian.
    #[must_use]
    pub fn to_le_bytes(&self) -> [u8; Self::WIRE_LEN] {
        let mut out = [0u8; Self::WIRE_LEN];
        put_u32(&mut out, 0, SERVICE_NOTICE_MAGIC);
        out[Self::OFF_VERSION..Self::OFF_VERSION + 2]
            .copy_from_slice(&SERVICE_VERSION_V1.to_le_bytes());
        out[Self::OFF_SIGNAL] = self.signal.as_u8();
        out
    }

    /// Decode a notice from `bytes`, failing closed on any malformed input.
    ///
    /// # Errors
    ///
    /// * [`Errno::BufferTooSmall`] — `bytes` cannot hold a whole notice.
    /// * [`Errno::BadMagic`] — wrong magic or a non-zero reserved byte.
    /// * [`Errno::AbiVersionUnsupported`] — not `service-v1`.
    /// * [`Errno::OutOfRange`] — a signal outside the closed
    ///   [`LifecycleSignal`] set.
    pub fn from_bytes(bytes: &[u8]) -> Result<Self, Errno> {
        if bytes.len() < Self::WIRE_LEN {
            return Err(Errno::BufferTooSmall);
        }
        if read_u32(bytes, 0) != SERVICE_NOTICE_MAGIC {
            return Err(Errno::BadMagic);
        }
        if u16::from_le_bytes([bytes[Self::OFF_VERSION], bytes[Self::OFF_VERSION + 1]])
            != SERVICE_VERSION_V1
        {
            return Err(Errno::AbiVersionUnsupported);
        }
        if bytes[Self::OFF_RESERVED] != 0 {
            return Err(Errno::BadMagic);
        }
        let signal = LifecycleSignal::from_u8(bytes[Self::OFF_SIGNAL]).ok_or(Errno::OutOfRange)?;
        Ok(Self { signal })
    }
}

/// Magic number identifying an `abi-v1` service unit-metadata record
/// (`"SUM1"` little-endian).
pub const SERVICE_MANIFEST_MAGIC: u32 = u32::from_le_bytes(*b"SUM1");

/// Maximum number of dependency names a service unit-metadata record may
/// carry.
///
/// A fixed **validation bound** (an anti-flood limit on untrusted encoded
/// input), not a scalable capacity: a service with a genuinely unbounded
/// dependency list is a packaging defect, not a workload to grow for. The
/// value is generous relative to any real service's fan-out.
pub const SERVICE_MANIFEST_MAX_DEPENDENCIES: usize = 64;

/// Maximum number of named readiness conditions a service unit-metadata
/// record may `require` (and, independently, `provide`).
///
/// A fixed validation bound. There are only a handful of named
/// [`ReadyCondition`]s, so a record naming more than this is malformed
/// rather than a workload to grow for.
pub const SERVICE_MANIFEST_MAX_CONDITIONS: usize = 16;

/// Maximum encoded byte length of one dependency name in a service
/// unit-metadata record.
///
/// A **structural** validation bound on the wire (anti-flood), deliberately
/// looser than the manager's own strict service-name policy: the manager
/// re-validates every decoded name against that policy before it registers a
/// service, so this bound only caps how many bytes the decoder will walk, it
/// does not decide which names are admissible.
pub const SERVICE_MANIFEST_MAX_NAME_LEN: usize = 128;

/// Maximum number of per-service resource limits a unit-metadata record may
/// carry.
///
/// Equal to [`LimitKind::COUNT`]: the limits section is canonical — strictly
/// ascending by [`LimitKind`] discriminant, so each governed resource appears
/// at most once and a record can never name more distinct limits than there
/// are resource kinds. A fixed validation bound (anti-flood on the wire), not
/// a scalable capacity.
pub const SERVICE_MANIFEST_MAX_LIMITS: usize = LimitKind::COUNT;

/// Fixed prefix byte length of a [`ServiceManifest`] record (everything
/// before the variable `requires`/`provides`/dependency/limits body).
const SERVICE_MANIFEST_PREFIX_LEN: usize = 62;

/// Wire length of one encoded per-service resource limit: a `u32`
/// [`LimitKind`] discriminant followed by a [`ResourceLimit`] soft/hard pair.
const SERVICE_LIMIT_WIRE_LEN: usize = 4 + ResourceLimit::WIRE_LEN;

/// `flags` bit set when the record's activation mode is on-demand (a linger
/// span applies); clear for a permanent service.
const SERVICE_MANIFEST_FLAG_ON_DEMAND: u16 = 1 << 0;
/// `flags` bit set when the record carries a non-null connect capability.
const SERVICE_MANIFEST_FLAG_CONNECT_CAP: u16 = 1 << 1;
/// The bits `flags` may legally set; any other bit fails the record closed.
const SERVICE_MANIFEST_FLAGS_KNOWN: u16 =
    SERVICE_MANIFEST_FLAG_ON_DEMAND | SERVICE_MANIFEST_FLAG_CONNECT_CAP;

// Prefix field offsets.
const SM_OFF_VERSION: usize = 4;
const SM_OFF_FLAGS: usize = 6;
const SM_OFF_ACCOUNT: usize = 8;
const SM_OFF_READINESS: usize = 12;
const SM_OFF_RESTART: usize = 13;
const SM_OFF_RESERVED0: usize = 14;
const SM_OFF_CONNECT_CAP: usize = 16;
const SM_OFF_REQUIRES_COUNT: usize = 18;
const SM_OFF_PROVIDES_COUNT: usize = 20;
const SM_OFF_DEPENDENCY_COUNT: usize = 22;
const SM_OFF_LINGER: usize = 24;
const SM_OFF_STOP_GRACE: usize = 36;
const SM_OFF_LIMITS_COUNT: usize = 48;
const SM_OFF_WATCHDOG: usize = 50;

/// The unit metadata one service declares in its signed bundle manifest — the
/// analogue of a systemd `.service` unit file, but a compact, fail-closed
/// binary record rather than a hand-parsed text file.
///
/// It is the description the service manager needs to *manage* a discovered
/// service — how it reaches readiness, whether it is permanent or on-demand,
/// what it does when it exits, how long it may take to stop, which endpoint
/// capability a client must hold to connect, the account it runs as, and the
/// dependencies and named readiness conditions it needs and provides — as
/// opposed to the *capability request* the [`ManifestHeader`](crate::ManifestHeader)
/// body carries. Because it lives inside the service's signed manifest, any
/// tampering with a service's activation or restart policy breaks the
/// signature and is a load refusal, never a silent privilege or behaviour
/// change.
///
/// [`ServiceUnit`] is the owned-by-reference **encoder** input; a
/// [`ServiceManifest`] is a **decoder** view borrowing an already-validated
/// byte buffer, so the same record round-trips between the trusted tooling
/// that writes a bundle and the manager that reads one. The decoder validates
/// the *whole* record up front (magic, version, reserved bytes, known flag
/// bits, every enum discriminant, every count against its bound, every
/// dependency name as bounded UTF-8, and an exact overall length), so every
/// accessor below is infallible and the record fails closed on the first
/// malformed byte.
///
/// The wire layout (little-endian) is a fixed 62-byte prefix followed by the
/// variable body:
///
/// ```text
///  0  magic u32              = SERVICE_MANIFEST_MAGIC
///  4  version u16            = SERVICE_VERSION_V1
///  6  flags u16              (on-demand, connect-cap-present; other bits 0)
///  8  account u32
/// 12  readiness u8           (ReadinessKind)
/// 13  restart u8             (RestartPolicy)
/// 14  reserved0 u16          = 0
/// 16  connect_capability u16 (0 unless the connect-cap flag is set)
/// 18  requires_count u16
/// 20  provides_count u16
/// 22  dependency_count u16
/// 24  linger     Duration64  (12 bytes; must be zero unless on-demand)
/// 36  stop_grace Duration64  (12 bytes)
/// 48  limits_count u16
/// 50  watchdog   Duration64  (12 bytes; zero = no liveness watchdog)
/// --- body ---
///     requires:      requires_count × u16 (ReadyCondition discriminants)
///     provides:      provides_count × u16
///     dependencies:  dependency_count × (u16 name_len ‖ name_len UTF-8 bytes)
///     limits:        limits_count × (u32 LimitKind ‖ u64 soft ‖ u64 hard),
///                    strictly ascending by kind (canonical, deduplicated)
/// ```
#[derive(Copy, Clone, Debug, Eq, PartialEq)]
pub struct ServiceUnit<'a> {
    /// uid of the service account the service runs as (the kernel resolves
    /// its capability ceiling and derives the grant at load time).
    pub account: u32,
    /// How the service reaches readiness (spawn-implies-ready versus notify).
    pub readiness: ReadinessKind,
    /// How the manager activates the service (permanent versus on-demand with
    /// an idle-linger span).
    pub activation: ActivationMode,
    /// What the manager does when the service's process exits.
    pub restart: RestartPolicy,
    /// Graceful-stop grace period the service is given to exit on its own
    /// before a forced terminate.
    pub stop_grace: Duration64,
    /// Capability a client must hold to connect to the service's reserved
    /// endpoint, or `None` for an endpoint that requires none.
    pub connect_capability: Option<CapabilityId>,
    /// Named readiness conditions that must be satisfied before the service
    /// may start.
    pub requires: &'a [ReadyCondition],
    /// Named readiness conditions the service satisfies once it becomes ready.
    pub provides: &'a [ReadyCondition],
    /// Names of the services that must start before this one.
    pub dependencies: &'a [&'a str],
    /// Optional per-service resource limits, in strictly ascending
    /// [`LimitKind`] order (each resource governed at most once). Empty means
    /// the service inherits the discovered, growable default policy uncapped;
    /// a present entry caps that resource for the service and its children.
    pub limits: &'a [ServiceLimit],
    /// Liveness-watchdog interval — the analogue of systemd's `WatchdogSec`.
    /// While the service is running it must renew a heartbeat to its manager
    /// at least this often; if it does not, the manager concludes the process
    /// has *wedged* (as opposed to cleanly exiting or being stopped), forces
    /// it down, and applies the service's [`RestartPolicy`] exactly as for any
    /// other unexpected exit. [`Duration64::ZERO`] (the default) disables the
    /// watchdog: a service that does not opt in is never judged on liveness.
    /// Must be non-negative; a negative interval fails the record closed.
    pub watchdog: Duration64,
}

/// One per-service resource limit: which resource it governs and its
/// soft/hard bound.
///
/// Carried in a service's signed unit metadata so the manager can hand the
/// bound to the kernel at spawn, where it is enforced (per-task storage,
/// inheritance, and the [`crate::CapabilityId::RLIMIT_RAISE`] gate on raising
/// a hard bound). It is *unit metadata*, not a capacity the manager itself
/// interprets: an empty limit list leaves the service governed by the
/// discovered growable default.
#[derive(Copy, Clone, Debug, Eq, PartialEq)]
pub struct ServiceLimit {
    /// The resource this limit governs.
    pub kind: LimitKind,
    /// The soft/hard bound imposed on that resource.
    pub limit: ResourceLimit,
}

impl ServiceUnit<'_> {
    /// The exact number of bytes [`encode`](Self::encode) will write.
    ///
    /// # Errors
    ///
    /// Returns [`Errno::OutOfRange`] if a count exceeds its bound
    /// ([`SERVICE_MANIFEST_MAX_CONDITIONS`] /
    /// [`SERVICE_MANIFEST_MAX_DEPENDENCIES`]) or a dependency name is empty or
    /// longer than [`SERVICE_MANIFEST_MAX_NAME_LEN`] — the same fail-closed
    /// checks [`encode`](Self::encode) applies, surfaced without a buffer so a
    /// caller can size one exactly.
    pub fn encoded_len(&self) -> Result<usize, Errno> {
        self.validate()?;
        let mut len = SERVICE_MANIFEST_PREFIX_LEN;
        len += self.requires.len() * 2;
        len += self.provides.len() * 2;
        for name in self.dependencies {
            len += 2 + name.len();
        }
        len += self.limits.len() * SERVICE_LIMIT_WIRE_LEN;
        Ok(len)
    }

    /// Validate the record's bounds without encoding it.
    fn validate(&self) -> Result<(), Errno> {
        if self.requires.len() > SERVICE_MANIFEST_MAX_CONDITIONS
            || self.provides.len() > SERVICE_MANIFEST_MAX_CONDITIONS
            || self.dependencies.len() > SERVICE_MANIFEST_MAX_DEPENDENCIES
        {
            return Err(Errno::OutOfRange);
        }
        for name in self.dependencies {
            if name.is_empty() || name.len() > SERVICE_MANIFEST_MAX_NAME_LEN {
                return Err(Errno::OutOfRange);
            }
        }
        if self.limits.len() > SERVICE_MANIFEST_MAX_LIMITS {
            return Err(Errno::OutOfRange);
        }
        // Canonical form: strictly ascending by kind (which also forbids a
        // duplicate resource) and every bound well-formed. Rejecting a
        // non-canonical or malformed list here keeps encode/decode symmetric
        // and the round-trip stable.
        let mut previous: Option<u32> = None;
        for entry in self.limits {
            if !entry.limit.is_well_formed() {
                return Err(Errno::OutOfRange);
            }
            let discriminant = entry.kind.as_u32();
            if let Some(prev) = previous {
                if discriminant <= prev {
                    return Err(Errno::OutOfRange);
                }
            }
            previous = Some(discriminant);
        }
        // A liveness watchdog counts forward from a heartbeat; a negative
        // interval is meaningless and fails the record closed rather than
        // being silently clamped. Zero is the legitimate "no watchdog".
        if self.watchdog < Duration64::ZERO {
            return Err(Errno::OutOfRange);
        }
        Ok(())
    }

    /// Encode `self` little-endian into `buf`, returning the number of bytes
    /// written.
    ///
    /// # Errors
    ///
    /// * [`Errno::OutOfRange`] — a count exceeds its bound, or a dependency
    ///   name is empty or too long (see [`encoded_len`](Self::encoded_len)).
    /// * [`Errno::BufferTooSmall`] — `buf` cannot hold the whole record.
    pub fn encode(&self, buf: &mut [u8]) -> Result<usize, Errno> {
        let total = self.encoded_len()?;
        if buf.len() < total {
            return Err(Errno::BufferTooSmall);
        }
        let out = &mut buf[..total];
        out.fill(0);
        put_u32(out, 0, SERVICE_MANIFEST_MAGIC);
        put_u16(out, SM_OFF_VERSION, SERVICE_VERSION_V1);
        let mut flags = 0u16;
        if self.activation.is_on_demand() {
            flags |= SERVICE_MANIFEST_FLAG_ON_DEMAND;
        }
        if self.connect_capability.is_some() {
            flags |= SERVICE_MANIFEST_FLAG_CONNECT_CAP;
        }
        put_u16(out, SM_OFF_FLAGS, flags);
        put_u32(out, SM_OFF_ACCOUNT, self.account);
        out[SM_OFF_READINESS] = self.readiness.as_u8();
        out[SM_OFF_RESTART] = self.restart.as_u8();
        if let Some(cap) = self.connect_capability {
            put_u16(out, SM_OFF_CONNECT_CAP, cap.as_u16());
        }
        // Counts are bounded by `validate` (well under `u16::MAX`); the
        // `try_from` never fails here but keeps the narrowing checked rather
        // than a truncating cast.
        let requires_len = u16::try_from(self.requires.len()).map_err(|_| Errno::OutOfRange)?;
        let provides_len = u16::try_from(self.provides.len()).map_err(|_| Errno::OutOfRange)?;
        let dependency_len =
            u16::try_from(self.dependencies.len()).map_err(|_| Errno::OutOfRange)?;
        let limits_len = u16::try_from(self.limits.len()).map_err(|_| Errno::OutOfRange)?;
        put_u16(out, SM_OFF_REQUIRES_COUNT, requires_len);
        put_u16(out, SM_OFF_PROVIDES_COUNT, provides_len);
        put_u16(out, SM_OFF_DEPENDENCY_COUNT, dependency_len);
        put_u16(out, SM_OFF_LIMITS_COUNT, limits_len);
        let linger = self.activation.linger().unwrap_or(Duration64::ZERO);
        out[SM_OFF_LINGER..SM_OFF_LINGER + Duration64::WIRE_LEN]
            .copy_from_slice(&linger.to_le_bytes());
        out[SM_OFF_STOP_GRACE..SM_OFF_STOP_GRACE + Duration64::WIRE_LEN]
            .copy_from_slice(&self.stop_grace.to_le_bytes());
        out[SM_OFF_WATCHDOG..SM_OFF_WATCHDOG + Duration64::WIRE_LEN]
            .copy_from_slice(&self.watchdog.to_le_bytes());

        let mut off = SERVICE_MANIFEST_PREFIX_LEN;
        for condition in self.requires {
            put_u16(out, off, condition.as_u16());
            off += 2;
        }
        for condition in self.provides {
            put_u16(out, off, condition.as_u16());
            off += 2;
        }
        for name in self.dependencies {
            // Bounded by `validate` to `SERVICE_MANIFEST_MAX_NAME_LEN`; the
            // checked narrowing never fails here.
            let name_len = u16::try_from(name.len()).map_err(|_| Errno::OutOfRange)?;
            put_u16(out, off, name_len);
            off += 2;
            out[off..off + name.len()].copy_from_slice(name.as_bytes());
            off += name.len();
        }
        for entry in self.limits {
            put_u32(out, off, entry.kind.as_u32());
            off += 4;
            out[off..off + ResourceLimit::WIRE_LEN].copy_from_slice(&entry.limit.encode());
            off += ResourceLimit::WIRE_LEN;
        }
        Ok(total)
    }
}

/// A validated, borrowed view over a service unit-metadata record — the
/// decoder counterpart of [`ServiceUnit`].
///
/// [`from_bytes`](Self::from_bytes) validates the whole record up front and
/// fails closed on any malformed byte, so every accessor is infallible.
#[derive(Copy, Clone, Debug, Eq, PartialEq)]
pub struct ServiceManifest<'a> {
    bytes: &'a [u8],
    account: u32,
    readiness: ReadinessKind,
    restart: RestartPolicy,
    activation: ActivationMode,
    connect_capability: Option<CapabilityId>,
    stop_grace: Duration64,
    watchdog: Duration64,
    requires_start: usize,
    requires_count: usize,
    provides_start: usize,
    provides_count: usize,
    dependencies_start: usize,
    dependencies_count: usize,
    limits_start: usize,
    limits_count: usize,
}

impl<'a> ServiceManifest<'a> {
    /// Decode and fully validate a record from `bytes`.
    ///
    /// # Errors
    ///
    /// Every failure is fail-closed — a record that does not decode cleanly
    /// yields no partial metadata:
    ///
    /// * [`Errno::BufferTooSmall`] — shorter than the fixed prefix, or a
    ///   declared section runs past the end of `bytes`.
    /// * [`Errno::BadMagic`] — wrong magic, or a reserved byte/field or an
    ///   unknown flag bit is non-zero, or the connect-capability field is
    ///   non-zero while its flag is clear, or `linger` is non-zero for a
    ///   permanent service.
    /// * [`Errno::AbiVersionUnsupported`] — not `service-v1`.
    /// * [`Errno::OutOfRange`] — an enum discriminant is outside its closed
    ///   set, a count exceeds its bound, a dependency name is empty or too
    ///   long, the watchdog interval is negative, or trailing bytes remain
    ///   after the record.
    pub fn from_bytes(bytes: &'a [u8]) -> Result<Self, Errno> {
        if bytes.len() < SERVICE_MANIFEST_PREFIX_LEN {
            return Err(Errno::BufferTooSmall);
        }
        if read_u32(bytes, 0) != SERVICE_MANIFEST_MAGIC {
            return Err(Errno::BadMagic);
        }
        if read_u16(bytes, SM_OFF_VERSION) != SERVICE_VERSION_V1 {
            return Err(Errno::AbiVersionUnsupported);
        }
        if read_u16(bytes, SM_OFF_RESERVED0) != 0 {
            return Err(Errno::BadMagic);
        }
        let flags = read_u16(bytes, SM_OFF_FLAGS);
        if flags & !SERVICE_MANIFEST_FLAGS_KNOWN != 0 {
            return Err(Errno::BadMagic);
        }
        let account = read_u32(bytes, SM_OFF_ACCOUNT);
        let readiness = ReadinessKind::from_u8(bytes[SM_OFF_READINESS]).ok_or(Errno::OutOfRange)?;
        let restart = RestartPolicy::from_u8(bytes[SM_OFF_RESTART]).ok_or(Errno::OutOfRange)?;

        let connect_raw = read_u16(bytes, SM_OFF_CONNECT_CAP);
        let connect_capability = if flags & SERVICE_MANIFEST_FLAG_CONNECT_CAP != 0 {
            Some(CapabilityId::from_raw(connect_raw)?)
        } else {
            if connect_raw != 0 {
                return Err(Errno::BadMagic);
            }
            None
        };

        let linger = Duration64::from_bytes(&bytes[SM_OFF_LINGER..])?;
        let stop_grace = Duration64::from_bytes(&bytes[SM_OFF_STOP_GRACE..])?;
        let watchdog = Duration64::from_bytes(&bytes[SM_OFF_WATCHDOG..])?;
        if watchdog < Duration64::ZERO {
            return Err(Errno::OutOfRange);
        }
        let activation = if flags & SERVICE_MANIFEST_FLAG_ON_DEMAND != 0 {
            ActivationMode::on_demand(linger)
        } else {
            if linger != Duration64::ZERO {
                return Err(Errno::BadMagic);
            }
            ActivationMode::Permanent
        };

        let requires_count = usize::from(read_u16(bytes, SM_OFF_REQUIRES_COUNT));
        let provides_count = usize::from(read_u16(bytes, SM_OFF_PROVIDES_COUNT));
        let dependencies_count = usize::from(read_u16(bytes, SM_OFF_DEPENDENCY_COUNT));
        let limits_count = usize::from(read_u16(bytes, SM_OFF_LIMITS_COUNT));
        if requires_count > SERVICE_MANIFEST_MAX_CONDITIONS
            || provides_count > SERVICE_MANIFEST_MAX_CONDITIONS
            || dependencies_count > SERVICE_MANIFEST_MAX_DEPENDENCIES
            || limits_count > SERVICE_MANIFEST_MAX_LIMITS
        {
            return Err(Errno::OutOfRange);
        }

        let mut off = SERVICE_MANIFEST_PREFIX_LEN;
        let requires_start = off;
        off = validate_conditions(bytes, off, requires_count)?;
        let provides_start = off;
        off = validate_conditions(bytes, off, provides_count)?;
        let dependencies_start = off;
        off = validate_dependencies(bytes, off, dependencies_count)?;
        let limits_start = off;
        off = validate_limits(bytes, off, limits_count)?;
        if off != bytes.len() {
            return Err(Errno::OutOfRange);
        }

        Ok(Self {
            bytes,
            account,
            readiness,
            restart,
            activation,
            connect_capability,
            stop_grace,
            watchdog,
            requires_start,
            requires_count,
            provides_start,
            provides_count,
            dependencies_start,
            dependencies_count,
            limits_start,
            limits_count,
        })
    }

    /// uid of the service account the service runs as.
    #[must_use]
    pub const fn account(&self) -> u32 {
        self.account
    }

    /// How the service reaches readiness.
    #[must_use]
    pub const fn readiness(&self) -> ReadinessKind {
        self.readiness
    }

    /// What the manager does when the service's process exits.
    #[must_use]
    pub const fn restart(&self) -> RestartPolicy {
        self.restart
    }

    /// How the manager activates the service.
    #[must_use]
    pub const fn activation(&self) -> ActivationMode {
        self.activation
    }

    /// The capability a client must hold to connect to the service's reserved
    /// endpoint, or `None` if the endpoint requires none.
    #[must_use]
    pub const fn connect_capability(&self) -> Option<CapabilityId> {
        self.connect_capability
    }

    /// The graceful-stop grace period the service is given to exit on its own.
    #[must_use]
    pub const fn stop_grace(&self) -> Duration64 {
        self.stop_grace
    }

    /// The liveness-watchdog interval (the analogue of systemd's
    /// `WatchdogSec`), or [`Duration64::ZERO`] if the service opts out of the
    /// liveness watchdog. See [`ServiceUnit::watchdog`].
    #[must_use]
    pub const fn watchdog(&self) -> Duration64 {
        self.watchdog
    }

    /// The named readiness conditions that gate this service's start.
    #[must_use]
    pub fn requires(&self) -> Conditions<'a> {
        Conditions {
            bytes: self.bytes,
            off: self.requires_start,
            remaining: self.requires_count,
        }
    }

    /// The named readiness conditions this service satisfies once ready.
    #[must_use]
    pub fn provides(&self) -> Conditions<'a> {
        Conditions {
            bytes: self.bytes,
            off: self.provides_start,
            remaining: self.provides_count,
        }
    }

    /// The names of the services that must start before this one.
    #[must_use]
    pub fn dependencies(&self) -> Dependencies<'a> {
        Dependencies {
            bytes: self.bytes,
            off: self.dependencies_start,
            remaining: self.dependencies_count,
        }
    }

    /// The per-service resource limits, in strictly ascending [`LimitKind`]
    /// order (empty if the service imposes none).
    #[must_use]
    pub fn limits(&self) -> Limits<'a> {
        Limits {
            bytes: self.bytes,
            off: self.limits_start,
            remaining: self.limits_count,
        }
    }
}

/// Walk a `count`-long `u16` condition section, validating each discriminant,
/// and return the offset just past it. Fails closed if the section runs off
/// the end or a discriminant is outside the closed [`ReadyCondition`] set.
fn validate_conditions(bytes: &[u8], mut off: usize, count: usize) -> Result<usize, Errno> {
    for _ in 0..count {
        let end = off.checked_add(2).ok_or(Errno::OutOfRange)?;
        if end > bytes.len() {
            return Err(Errno::BufferTooSmall);
        }
        ReadyCondition::from_u16(read_u16(bytes, off)).ok_or(Errno::OutOfRange)?;
        off = end;
    }
    Ok(off)
}

/// Walk a `count`-long length-prefixed dependency-name section, validating
/// each name as non-empty, in-bound, valid UTF-8, and return the offset just
/// past it. Fails closed on a truncated section or a malformed name.
fn validate_dependencies(bytes: &[u8], mut off: usize, count: usize) -> Result<usize, Errno> {
    for _ in 0..count {
        let len_end = off.checked_add(2).ok_or(Errno::OutOfRange)?;
        if len_end > bytes.len() {
            return Err(Errno::BufferTooSmall);
        }
        let name_len = usize::from(read_u16(bytes, off));
        if name_len == 0 || name_len > SERVICE_MANIFEST_MAX_NAME_LEN {
            return Err(Errno::OutOfRange);
        }
        let name_end = len_end.checked_add(name_len).ok_or(Errno::OutOfRange)?;
        if name_end > bytes.len() {
            return Err(Errno::BufferTooSmall);
        }
        core::str::from_utf8(&bytes[len_end..name_end]).map_err(|_| Errno::OutOfRange)?;
        off = name_end;
    }
    Ok(off)
}

/// Walk a `count`-long resource-limit section, validating each entry's kind
/// discriminant, its soft/hard well-formedness, and the strictly-ascending
/// (canonical, deduplicated) kind order, and return the offset just past it.
/// Fails closed on a truncated section, an unknown kind, a malformed bound,
/// or a non-canonical order.
fn validate_limits(bytes: &[u8], mut off: usize, count: usize) -> Result<usize, Errno> {
    let mut previous: Option<u32> = None;
    for _ in 0..count {
        let end = off
            .checked_add(SERVICE_LIMIT_WIRE_LEN)
            .ok_or(Errno::OutOfRange)?;
        if end > bytes.len() {
            return Err(Errno::BufferTooSmall);
        }
        let discriminant = read_u32(bytes, off);
        LimitKind::from_u32(discriminant)?;
        // `decode` enforces `soft <= hard`, so a malformed bound fails closed.
        ResourceLimit::decode(&bytes[off + 4..end])?;
        if let Some(prev) = previous {
            if discriminant <= prev {
                return Err(Errno::OutOfRange);
            }
        }
        previous = Some(discriminant);
        off = end;
    }
    Ok(off)
}

/// Iterator over a [`ServiceManifest`]'s `requires`/`provides` conditions.
///
/// The backing bytes were validated by
/// [`ServiceManifest::from_bytes`], so iteration is total; the decode below
/// never fails for a value that survived validation.
#[derive(Clone, Debug)]
pub struct Conditions<'a> {
    bytes: &'a [u8],
    off: usize,
    remaining: usize,
}

impl Iterator for Conditions<'_> {
    type Item = ReadyCondition;

    fn next(&mut self) -> Option<Self::Item> {
        if self.remaining == 0 {
            return None;
        }
        let value = read_u16(self.bytes, self.off);
        self.off += 2;
        self.remaining -= 1;
        ReadyCondition::from_u16(value)
    }

    fn size_hint(&self) -> (usize, Option<usize>) {
        (self.remaining, Some(self.remaining))
    }
}

/// Iterator over a [`ServiceManifest`]'s dependency names.
///
/// The backing bytes were validated by
/// [`ServiceManifest::from_bytes`] (each name is non-empty, in-bound, valid
/// UTF-8), so iteration is total.
#[derive(Clone, Debug)]
pub struct Dependencies<'a> {
    bytes: &'a [u8],
    off: usize,
    remaining: usize,
}

/// Iterator over a [`ServiceManifest`]'s per-service resource limits.
///
/// The backing bytes were validated by [`ServiceManifest::from_bytes`] (each
/// kind is a known discriminant and each bound is well-formed), so iteration
/// is total; the decode below never fails for a value that survived
/// validation.
#[derive(Clone, Debug)]
pub struct Limits<'a> {
    bytes: &'a [u8],
    off: usize,
    remaining: usize,
}

impl Iterator for Limits<'_> {
    type Item = ServiceLimit;

    fn next(&mut self) -> Option<Self::Item> {
        if self.remaining == 0 {
            return None;
        }
        let kind = LimitKind::from_u32(read_u32(self.bytes, self.off)).ok()?;
        let limit = ResourceLimit::decode(self.bytes.get(self.off + 4..)?).ok()?;
        self.off += SERVICE_LIMIT_WIRE_LEN;
        self.remaining -= 1;
        Some(ServiceLimit { kind, limit })
    }

    fn size_hint(&self) -> (usize, Option<usize>) {
        (self.remaining, Some(self.remaining))
    }
}

impl<'a> Iterator for Dependencies<'a> {
    type Item = &'a str;

    fn next(&mut self) -> Option<Self::Item> {
        if self.remaining == 0 {
            return None;
        }
        let name_len = usize::from(read_u16(self.bytes, self.off));
        let start = self.off + 2;
        let end = start + name_len;
        self.off = end;
        self.remaining -= 1;
        core::str::from_utf8(self.bytes.get(start..end)?).ok()
    }

    fn size_hint(&self) -> (usize, Option<usize>) {
        (self.remaining, Some(self.remaining))
    }
}

#[cfg(test)]
mod tests {
    use super::{
        ActivationMode, LifecycleSignal, ReadinessKind, ReadyCondition, ReadyNotice, RestartPolicy,
        ServiceLimit, ServiceManifest, ServiceState, ServiceUnit, SERVICE_MANIFEST_MAGIC,
        SERVICE_MANIFEST_MAX_CONDITIONS, SERVICE_MANIFEST_MAX_DEPENDENCIES,
        SERVICE_MANIFEST_MAX_LIMITS, SERVICE_MANIFEST_MAX_NAME_LEN, SERVICE_NOTICE_MAGIC,
        SERVICE_VERSION_V1,
    };
    use crate::rlimit::{LimitKind, ResourceLimit};
    use crate::{CapabilityId, Duration64, Errno};

    #[test]
    fn readiness_kind_round_trips_and_defaults_immediate() {
        assert_eq!(ReadinessKind::default(), ReadinessKind::Immediate);
        for kind in [ReadinessKind::Immediate, ReadinessKind::Notify] {
            assert_eq!(ReadinessKind::from_u8(kind.as_u8()), Some(kind));
        }
        assert_eq!(ReadinessKind::from_u8(2), None);
        assert_eq!(ReadinessKind::Immediate.as_str(), "immediate");
        assert_eq!(ReadinessKind::Notify.as_str(), "notify");
    }

    #[test]
    fn activation_mode_defaults_permanent_and_carries_linger() {
        use super::ActivationMode;
        use crate::Duration64;

        assert_eq!(ActivationMode::default(), ActivationMode::Permanent);
        assert!(!ActivationMode::Permanent.is_on_demand());
        assert_eq!(ActivationMode::Permanent.linger(), None);
        assert_eq!(ActivationMode::Permanent.as_str(), "permanent");

        let linger = Duration64::from_secs(30);
        let mode = ActivationMode::on_demand(linger);
        assert!(mode.is_on_demand());
        assert_eq!(mode.linger(), Some(linger));
        assert_eq!(mode.as_str(), "on-demand");
    }

    #[test]
    fn restart_policy_round_trips_and_decides_restart() {
        use super::RestartPolicy;

        assert_eq!(RestartPolicy::default(), RestartPolicy::Never);
        for (i, policy) in RestartPolicy::ALL.into_iter().enumerate() {
            assert_eq!(policy.as_u8() as usize, i);
            assert_eq!(RestartPolicy::from_u8(policy.as_u8()), Some(policy));
        }
        assert_eq!(RestartPolicy::from_u8(3), None);
        assert_eq!(RestartPolicy::Never.as_str(), "never");
        assert_eq!(RestartPolicy::OnFailure.as_str(), "on-failure");
        assert_eq!(RestartPolicy::Always.as_str(), "always");

        // `never` never restarts; `on-failure` only on a non-zero code;
        // `always` on any code.
        assert!(!RestartPolicy::Never.should_restart(0));
        assert!(!RestartPolicy::Never.should_restart(1));
        assert!(!RestartPolicy::OnFailure.should_restart(0));
        assert!(RestartPolicy::OnFailure.should_restart(1));
        assert!(RestartPolicy::OnFailure.should_restart(-9));
        assert!(RestartPolicy::Always.should_restart(0));
        assert!(RestartPolicy::Always.should_restart(1));
    }

    #[test]
    fn magic_and_version_are_frozen() {
        assert_eq!(SERVICE_NOTICE_MAGIC, u32::from_le_bytes(*b"SVC1"));
        assert_eq!(SERVICE_VERSION_V1, 1);
    }

    #[test]
    fn service_state_discriminants_are_frozen_and_round_trip() {
        for (i, state) in ServiceState::ALL.into_iter().enumerate() {
            assert_eq!(state.as_u8() as usize, i);
            assert_eq!(ServiceState::from_u8(state.as_u8()), Some(state));
        }
        assert_eq!(ServiceState::from_u8(7), None);
        // Readiness gate: only ready/running release dependents.
        assert!(ServiceState::Ready.is_ready());
        assert!(ServiceState::Running.is_ready());
        assert!(!ServiceState::Starting.is_ready());
        assert!(!ServiceState::Inactive.is_ready());
        assert!(ServiceState::Stopped.is_terminal());
        assert!(ServiceState::Failed.is_terminal());
        assert!(!ServiceState::Running.is_terminal());
    }

    #[test]
    fn ready_condition_discriminants_and_names_are_frozen() {
        for (i, cond) in ReadyCondition::ALL.into_iter().enumerate() {
            assert_eq!(cond.as_u16() as usize, i);
            assert_eq!(ReadyCondition::from_u16(cond.as_u16()), Some(cond));
            assert_eq!(ReadyCondition::from_name(cond.as_str()), Some(cond));
        }
        assert_eq!(ReadyCondition::from_u16(5), None);
        assert_eq!(ReadyCondition::from_name("no-such-condition"), None);
        assert_eq!(ReadyCondition::NetworkUp.as_str(), "network-up");
        assert_eq!(ReadyCondition::SeatAvailable.as_str(), "seat-available");
    }

    #[test]
    fn lifecycle_signal_round_trips() {
        for signal in [LifecycleSignal::Ready, LifecycleSignal::Failed] {
            assert_eq!(LifecycleSignal::from_u8(signal.as_u8()), Some(signal));
        }
        // Zero is reserved (no service self-announces "inactive").
        assert_eq!(LifecycleSignal::from_u8(0), None);
        assert_eq!(LifecycleSignal::from_u8(3), None);
    }

    #[test]
    fn notice_round_trips() {
        for signal in [LifecycleSignal::Ready, LifecycleSignal::Failed] {
            let notice = ReadyNotice::new(signal);
            let bytes = notice.to_le_bytes();
            assert_eq!(bytes.len(), ReadyNotice::WIRE_LEN);
            assert_eq!(ReadyNotice::from_bytes(&bytes), Ok(notice));
        }
    }

    #[test]
    fn notice_decode_fails_closed() {
        let good = ReadyNotice::new(LifecycleSignal::Ready).to_le_bytes();

        assert_eq!(
            ReadyNotice::from_bytes(&good[..ReadyNotice::WIRE_LEN - 1]),
            Err(Errno::BufferTooSmall)
        );
        let mut bad_magic = good;
        bad_magic[0] ^= 0xFF;
        assert_eq!(ReadyNotice::from_bytes(&bad_magic), Err(Errno::BadMagic));

        let mut bad_version = good;
        bad_version[4] = 9;
        assert_eq!(
            ReadyNotice::from_bytes(&bad_version),
            Err(Errno::AbiVersionUnsupported)
        );

        let mut bad_signal = good;
        bad_signal[6] = 0; // not a self-announceable signal
        assert_eq!(ReadyNotice::from_bytes(&bad_signal), Err(Errno::OutOfRange));

        let mut dirty_reserved = good;
        dirty_reserved[7] = 1;
        assert_eq!(
            ReadyNotice::from_bytes(&dirty_reserved),
            Err(Errno::BadMagic)
        );
    }

    /// Encode `unit` into a generously sized stack buffer, returning the
    /// exact-length encoded slice.
    fn encode<'b>(unit: &ServiceUnit<'_>, buf: &'b mut [u8]) -> &'b [u8] {
        let len = unit.encode(buf).expect("encode");
        assert_eq!(len, unit.encoded_len().expect("encoded_len"));
        &buf[..len]
    }

    #[test]
    fn service_manifest_magic_and_bounds_are_frozen() {
        assert_eq!(SERVICE_MANIFEST_MAGIC, u32::from_le_bytes(*b"SUM1"));
        assert_eq!(SERVICE_MANIFEST_MAX_DEPENDENCIES, 64);
        assert_eq!(SERVICE_MANIFEST_MAX_CONDITIONS, 16);
        assert_eq!(SERVICE_MANIFEST_MAX_NAME_LEN, 128);
        assert_eq!(SERVICE_MANIFEST_MAX_LIMITS, LimitKind::COUNT);
    }

    #[test]
    fn service_manifest_round_trips_a_full_on_demand_record() {
        let unit = ServiceUnit {
            account: 42,
            readiness: ReadinessKind::Notify,
            activation: ActivationMode::on_demand(Duration64::from_secs(30)),
            restart: RestartPolicy::OnFailure,
            stop_grace: Duration64::from_secs(7),
            connect_capability: Some(CapabilityId::SYSINFO_GLOBAL),
            requires: &[
                ReadyCondition::NetworkUp,
                ReadyCondition::FilesystemsMounted,
            ],
            provides: &[ReadyCondition::BootComplete],
            dependencies: &["netstack", "sysinfod"],
            limits: &[],
            watchdog: Duration64::from_secs(45),
        };
        let mut buf = [0u8; 256];
        let bytes = encode(&unit, &mut buf);

        let manifest = ServiceManifest::from_bytes(bytes).expect("decode");
        assert_eq!(manifest.account(), 42);
        assert_eq!(manifest.readiness(), ReadinessKind::Notify);
        assert_eq!(manifest.restart(), RestartPolicy::OnFailure);
        assert_eq!(
            manifest.activation(),
            ActivationMode::on_demand(Duration64::from_secs(30))
        );
        assert_eq!(
            manifest.connect_capability(),
            Some(CapabilityId::SYSINFO_GLOBAL)
        );
        assert_eq!(manifest.stop_grace(), Duration64::from_secs(7));
        assert!(manifest.requires().eq([
            ReadyCondition::NetworkUp,
            ReadyCondition::FilesystemsMounted
        ]));
        assert!(manifest.provides().eq([ReadyCondition::BootComplete]));
        assert!(manifest.dependencies().eq(["netstack", "sysinfod"]));
        assert_eq!(manifest.watchdog(), Duration64::from_secs(45));
    }

    #[test]
    fn service_manifest_defaults_the_watchdog_to_disabled() {
        // A record that opts out of the watchdog decodes to a zero interval,
        // and the minimal record re-encodes to exactly the fixed prefix.
        let none = unit_with_limits(&[]);
        let mut buf = [0u8; 64];
        let bytes = encode(&none, &mut buf);
        assert_eq!(bytes.len(), super::SERVICE_MANIFEST_PREFIX_LEN);
        let manifest = ServiceManifest::from_bytes(bytes).expect("decode");
        assert_eq!(manifest.watchdog(), Duration64::ZERO);
    }

    #[test]
    fn service_manifest_rejects_a_negative_watchdog() {
        // A negative liveness interval is meaningless; encode refuses it and
        // a hand-forged negative field fails the decode closed.
        let mut unit = unit_with_limits(&[]);
        unit.watchdog = Duration64::from_secs(-1);
        let mut buf = [0u8; 64];
        assert_eq!(unit.encode(&mut buf), Err(Errno::OutOfRange));

        // Encode a valid record, then stamp a negative watchdog second count
        // into its wire bytes and confirm the decoder rejects it.
        let ok = unit_with_limits(&[]);
        let len = ok.encode(&mut buf).expect("encode");
        buf[super::SM_OFF_WATCHDOG..super::SM_OFF_WATCHDOG + 8]
            .copy_from_slice(&(-5i64).to_le_bytes());
        assert_eq!(
            ServiceManifest::from_bytes(&buf[..len]),
            Err(Errno::OutOfRange)
        );
    }

    #[test]
    fn service_manifest_round_trips_a_minimal_permanent_record() {
        let unit = ServiceUnit {
            account: 0,
            readiness: ReadinessKind::Immediate,
            activation: ActivationMode::Permanent,
            restart: RestartPolicy::Never,
            stop_grace: Duration64::ZERO,
            connect_capability: None,
            requires: &[],
            provides: &[],
            dependencies: &[],
            limits: &[],
            watchdog: Duration64::ZERO,
        };
        let mut buf = [0u8; 64];
        let bytes = encode(&unit, &mut buf);
        // A minimal record is exactly the fixed prefix.
        assert_eq!(bytes.len(), super::SERVICE_MANIFEST_PREFIX_LEN);

        let manifest = ServiceManifest::from_bytes(bytes).expect("decode");
        assert_eq!(manifest.account(), 0);
        assert_eq!(manifest.readiness(), ReadinessKind::Immediate);
        assert_eq!(manifest.restart(), RestartPolicy::Never);
        assert_eq!(manifest.activation(), ActivationMode::Permanent);
        assert_eq!(manifest.connect_capability(), None);
        assert_eq!(manifest.stop_grace(), Duration64::ZERO);
        assert_eq!(manifest.requires().count(), 0);
        assert_eq!(manifest.provides().count(), 0);
        assert_eq!(manifest.dependencies().count(), 0);
    }

    #[test]
    fn service_manifest_encode_rejects_a_short_buffer() {
        let unit = ServiceUnit {
            account: 1,
            readiness: ReadinessKind::Immediate,
            activation: ActivationMode::Permanent,
            restart: RestartPolicy::Never,
            stop_grace: Duration64::ZERO,
            connect_capability: None,
            requires: &[],
            provides: &[],
            dependencies: &[],
            limits: &[],
            watchdog: Duration64::ZERO,
        };
        let mut small = [0u8; 8];
        assert_eq!(unit.encode(&mut small), Err(Errno::BufferTooSmall));
    }

    #[test]
    fn service_manifest_encode_rejects_over_bound_input() {
        let base = ServiceUnit {
            account: 1,
            readiness: ReadinessKind::Immediate,
            activation: ActivationMode::Permanent,
            restart: RestartPolicy::Never,
            stop_grace: Duration64::ZERO,
            connect_capability: None,
            requires: &[],
            provides: &[],
            dependencies: &[],
            limits: &[],
            watchdog: Duration64::ZERO,
        };
        let mut buf = [0u8; 512];

        // Too many required conditions.
        let many = [ReadyCondition::NetworkUp; SERVICE_MANIFEST_MAX_CONDITIONS + 1];
        let over_conditions = ServiceUnit {
            requires: &many,
            ..base
        };
        assert_eq!(over_conditions.encode(&mut buf), Err(Errno::OutOfRange));

        // An empty dependency name is refused (a name must be present).
        let empty_dep = ServiceUnit {
            dependencies: &[""],
            ..base
        };
        assert_eq!(empty_dep.encode(&mut buf), Err(Errno::OutOfRange));

        // A dependency name past the structural bound is refused.
        let long = [b'a'; SERVICE_MANIFEST_MAX_NAME_LEN + 1];
        let long_name = core::str::from_utf8(&long).expect("ascii");
        let over_name = ServiceUnit {
            dependencies: &[long_name],
            ..base
        };
        assert_eq!(over_name.encode(&mut buf), Err(Errno::OutOfRange));
    }

    #[test]
    fn service_manifest_decode_fails_closed() {
        let unit = ServiceUnit {
            account: 3,
            readiness: ReadinessKind::Immediate,
            activation: ActivationMode::on_demand(Duration64::from_secs(1)),
            restart: RestartPolicy::Always,
            stop_grace: Duration64::from_secs(2),
            connect_capability: Some(CapabilityId::FS_MOUNT),
            requires: &[ReadyCondition::NetworkUp],
            provides: &[],
            dependencies: &["dep"],
            limits: &[],
            watchdog: Duration64::ZERO,
        };
        let mut buf = [0u8; 256];
        let len = unit.encode(&mut buf).expect("encode");
        let good = &buf[..len];
        assert!(ServiceManifest::from_bytes(good).is_ok());

        // Truncated below the fixed prefix.
        assert_eq!(
            ServiceManifest::from_bytes(&good[..super::SERVICE_MANIFEST_PREFIX_LEN - 1]),
            Err(Errno::BufferTooSmall)
        );

        let tamper = |edit: &mut dyn FnMut(&mut [u8])| {
            let mut copy = [0u8; 256];
            copy[..len].copy_from_slice(good);
            edit(&mut copy[..len]);
            ServiceManifest::from_bytes(&copy[..len]).err()
        };

        // Bad magic.
        assert_eq!(tamper(&mut |b| b[0] ^= 0xFF), Some(Errno::BadMagic));
        // Unsupported version.
        assert_eq!(
            tamper(&mut |b| b[super::SM_OFF_VERSION] = 9),
            Some(Errno::AbiVersionUnsupported)
        );
        // Non-zero reserved0.
        assert_eq!(
            tamper(&mut |b| b[super::SM_OFF_RESERVED0] = 1),
            Some(Errno::BadMagic)
        );
        // An unknown flag bit set.
        assert_eq!(
            tamper(&mut |b| b[super::SM_OFF_FLAGS] |= 1 << 4),
            Some(Errno::BadMagic)
        );
        // A readiness discriminant outside the closed set.
        assert_eq!(
            tamper(&mut |b| b[super::SM_OFF_READINESS] = 7),
            Some(Errno::OutOfRange)
        );
        // A restart discriminant outside the closed set.
        assert_eq!(
            tamper(&mut |b| b[super::SM_OFF_RESTART] = 7),
            Some(Errno::OutOfRange)
        );

        // Trailing bytes after an otherwise-valid record.
        let mut trailing = [0u8; 256];
        trailing[..len].copy_from_slice(good);
        assert_eq!(
            ServiceManifest::from_bytes(&trailing[..=len]),
            Err(Errno::OutOfRange)
        );
    }

    #[test]
    fn service_manifest_rejects_linger_on_a_permanent_record() {
        // A permanent service must carry a zero linger; a non-zero one is a
        // malformed record, not a silently-ignored field.
        let unit = ServiceUnit {
            account: 0,
            readiness: ReadinessKind::Immediate,
            activation: ActivationMode::Permanent,
            restart: RestartPolicy::Never,
            stop_grace: Duration64::ZERO,
            connect_capability: None,
            requires: &[],
            provides: &[],
            dependencies: &[],
            limits: &[],
            watchdog: Duration64::ZERO,
        };
        let mut buf = [0u8; 64];
        let len = unit.encode(&mut buf).expect("encode");
        // Write a non-zero linger while leaving the on-demand flag clear.
        buf[super::SM_OFF_LINGER] = 1;
        assert_eq!(
            ServiceManifest::from_bytes(&buf[..len]),
            Err(Errno::BadMagic)
        );
    }

    #[test]
    fn service_manifest_rejects_connect_cap_without_its_flag() {
        // The connect-capability field must be zero unless its flag is set.
        let unit = ServiceUnit {
            account: 0,
            readiness: ReadinessKind::Immediate,
            activation: ActivationMode::Permanent,
            restart: RestartPolicy::Never,
            stop_grace: Duration64::ZERO,
            connect_capability: None,
            requires: &[],
            provides: &[],
            dependencies: &[],
            limits: &[],
            watchdog: Duration64::ZERO,
        };
        let mut buf = [0u8; 64];
        let len = unit.encode(&mut buf).expect("encode");
        buf[super::SM_OFF_CONNECT_CAP] = 5; // non-zero, flag still clear
        assert_eq!(
            ServiceManifest::from_bytes(&buf[..len]),
            Err(Errno::BadMagic)
        );
    }

    /// Build a permanent, immediate record carrying the given `limits`.
    fn unit_with_limits(limits: &[ServiceLimit]) -> ServiceUnit<'_> {
        ServiceUnit {
            account: 12,
            readiness: ReadinessKind::Immediate,
            activation: ActivationMode::Permanent,
            restart: RestartPolicy::Never,
            stop_grace: Duration64::ZERO,
            connect_capability: None,
            requires: &[],
            provides: &[],
            dependencies: &[],
            limits,
            watchdog: Duration64::ZERO,
        }
    }

    #[test]
    fn service_manifest_round_trips_per_service_limits() {
        let limits = [
            ServiceLimit {
                kind: LimitKind::OpenStreams,
                limit: ResourceLimit::new(64, 128).expect("well-formed"),
            },
            ServiceLimit {
                kind: LimitKind::Processes,
                limit: ResourceLimit::new(8, 8).expect("well-formed"),
            },
            ServiceLimit {
                kind: LimitKind::PinnedMemoryBytes,
                limit: ResourceLimit::UNLIMITED,
            },
        ];
        let unit = unit_with_limits(&limits);
        let mut buf = [0u8; 256];
        let bytes = encode(&unit, &mut buf);

        let manifest = ServiceManifest::from_bytes(bytes).expect("decode");
        assert!(manifest.limits().eq(limits.iter().copied()));
        // The empty case yields no entries and re-encodes to the bare prefix.
        let none = unit_with_limits(&[]);
        let mut nbuf = [0u8; 64];
        let nbytes = encode(&none, &mut nbuf);
        assert_eq!(nbytes.len(), super::SERVICE_MANIFEST_PREFIX_LEN);
        assert_eq!(
            ServiceManifest::from_bytes(nbytes)
                .expect("decode")
                .limits()
                .count(),
            0
        );
    }

    #[test]
    fn service_manifest_rejects_non_canonical_or_malformed_limits() {
        // A duplicate kind is not strictly ascending, so encode refuses it.
        let duplicate = [
            ServiceLimit {
                kind: LimitKind::Processes,
                limit: ResourceLimit::UNLIMITED,
            },
            ServiceLimit {
                kind: LimitKind::Processes,
                limit: ResourceLimit::UNLIMITED,
            },
        ];
        let mut buf = [0u8; 256];
        assert_eq!(
            unit_with_limits(&duplicate).encode(&mut buf),
            Err(Errno::OutOfRange)
        );

        // A descending pair is likewise non-canonical.
        let descending = [
            ServiceLimit {
                kind: LimitKind::Processes,
                limit: ResourceLimit::UNLIMITED,
            },
            ServiceLimit {
                kind: LimitKind::OpenStreams,
                limit: ResourceLimit::UNLIMITED,
            },
        ];
        assert_eq!(
            unit_with_limits(&descending).encode(&mut buf),
            Err(Errno::OutOfRange)
        );

        // More entries than there are resource kinds cannot be canonical.
        let too_many = [ServiceLimit {
            kind: LimitKind::AddressSpaceBytes,
            limit: ResourceLimit::UNLIMITED,
        }; SERVICE_MANIFEST_MAX_LIMITS + 1];
        assert_eq!(
            unit_with_limits(&too_many).encode(&mut buf),
            Err(Errno::OutOfRange)
        );

        // A well-formed single limit encodes; tampering its stored bound to
        // soft > hard, or its kind to an unknown discriminant, fails the
        // decoder closed.
        let ok = [ServiceLimit {
            kind: LimitKind::StackBytes,
            limit: ResourceLimit::new(4096, 8192).expect("well-formed"),
        }];
        let len = unit_with_limits(&ok).encode(&mut buf).expect("encode");
        assert!(ServiceManifest::from_bytes(&buf[..len]).is_ok());
        // The single limit's body sits immediately after the fixed prefix
        // (this record has no requires/provides/dependency body).
        let limit_off = super::SERVICE_MANIFEST_PREFIX_LEN;
        // Corrupt the kind discriminant.
        let mut bad_kind = buf;
        crate::le::put_u32(&mut bad_kind, limit_off, 0xDEAD_BEEF);
        assert_eq!(
            ServiceManifest::from_bytes(&bad_kind[..len]),
            Err(Errno::OutOfRange)
        );
        // Corrupt the bound so soft > hard (soft is the 8 bytes after kind).
        let mut bad_bound = buf;
        crate::le::put_u64(&mut bad_bound, limit_off + 4, u64::MAX);
        crate::le::put_u64(&mut bad_bound, limit_off + 12, 0);
        assert_eq!(
            ServiceManifest::from_bytes(&bad_bound[..len]),
            Err(Errno::OutOfRange)
        );
    }
}
