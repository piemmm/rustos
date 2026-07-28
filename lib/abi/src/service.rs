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

use crate::le::{put_u32, read_u32};
use crate::{Duration64, Errno};

/// Magic number identifying an `abi-v1` service readiness notice (`"SVC1"`
/// little-endian).
pub const SERVICE_NOTICE_MAGIC: u32 = u32::from_le_bytes(*b"SVC1");

/// The `service-v1` readiness-protocol version.
pub const SERVICE_VERSION_V1: u16 = 1;

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

#[cfg(test)]
mod tests {
    use super::{
        LifecycleSignal, ReadinessKind, ReadyCondition, ReadyNotice, ServiceState,
        SERVICE_NOTICE_MAGIC, SERVICE_VERSION_V1,
    };
    use crate::Errno;

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
}
