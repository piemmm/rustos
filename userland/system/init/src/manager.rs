//! The [`Init`] state machine: register services, start them in dependency
//! order, and reap exited children.
//!
//! This is the one place a service is ordered, capability-gated, audited,
//! and launched. The pipeline **fails closed**: a
//! structurally broken service graph starts nothing, and a service whose
//! manifest over-requests authority is refused rather than narrowed
//! silently.

use alloc::collections::VecDeque;
use alloc::string::{String, ToString};
use alloc::vec;
use alloc::vec::Vec;
use core::fmt::{self, Write as _};

use tairix_abi::{
    Duration64, LifecycleSignal, ReadinessKind, ReadyCondition, ServiceControlOp,
    ServiceControlRequest, ServiceState, NANOS_PER_SEC,
};
use tairix_caps::CapabilitySet;
use tairix_log::{log, Event, EventId, Field, Level, Sink};

use crate::error::{ActivateError, ControlError, InitError, NotifyError, StartFailure};
use crate::events;
use crate::registry::{validate_service_name, Enrolment};
use crate::scope::AuthorityScope;
use crate::service::{ClientId, Pid, ReapedChild, Reaper, ServiceSpec, Spawner, Stopper};

/// Number of distinct named readiness conditions, sized from the closed
/// [`ReadyCondition`] set so the satisfied-conditions bitmap tracks the
/// vocabulary rather than a hand-picked constant.
const CONDITION_COUNT: usize = ReadyCondition::ALL.len();

/// Maximum number of clients that may be parked waiting for a single
/// service to become ready.
///
/// This is a **security bound**, not a resource capacity: it fails a
/// connect flood closed ([`ActivateError::QueueFull`]) rather than letting
/// an attacker grow the manager's per-service wait queue without limit and
/// exhaust its memory. The bound is per service, so one busy service cannot
/// starve another's queue. A legitimate service with more genuinely
/// concurrent first-connections than this is vanishingly unlikely; if one
/// ever arises it is raised deliberately here, never removed.
const MAX_PENDING_PER_SERVICE: usize = 64;

/// How many times the restart policy will relaunch a single service that
/// keeps dying before it has run stably, before the manager gives up on it.
///
/// This is the **crash-loop guard**, not a resource capacity: a service
/// that crashes the instant it starts would otherwise be relaunched forever
/// (the `spawn`-in-a-loop the charter forbids). Once a relaunched service
/// runs longer than [`RESTART_STABLE_WINDOW`] the counter resets, so this
/// bounds only a *tight* crash loop; a service that fails after a long,
/// healthy uptime is always restarted afresh. The count is per service, so
/// one crash-looping service never exhausts another's budget.
const MAX_RESTART_ATTEMPTS: u32 = 5;

/// The base of the exponential restart backoff, in nanoseconds (100 ms):
/// the delay before the first relaunch. Each subsequent relaunch doubles
/// it, capped at [`RESTART_BACKOFF_CAP`], so the manager never hammers a
/// failing service.
const RESTART_BACKOFF_BASE_NS: u128 = 100_000_000;

/// The ceiling on the exponential restart backoff. A service that keeps
/// failing waits at most this long between relaunches, so the delay never
/// grows without bound while the crash-loop budget counts down.
const RESTART_BACKOFF_CAP: Duration64 = Duration64::from_secs(30);

/// How long a relaunched service must run before the manager considers it
/// to have recovered and resets its [`MAX_RESTART_ATTEMPTS`] budget.
///
/// A service that stays up past this window and only then exits is treated
/// as a fresh, isolated failure rather than part of a crash loop, so a
/// long-lived daemon that crashes once after hours is restarted with a full
/// budget instead of being penalised for restarts in the distant past.
const RESTART_STABLE_WINDOW: Duration64 = Duration64::from_secs(30);

/// The synthetic exit code the reaper attributes to a process the liveness
/// watchdog force-killed for wedging ([`Init::expire_watchdog`]).
///
/// A wedged process is force-terminated, so the exit code a real signal
/// kill reports is not something the manager can rely on (a port might even
/// surface it as zero). This deterministic non-zero code makes the exit
/// unambiguously *abnormal*, so the restart policy's `should_restart` treats
/// a watchdog kill as a failure under `on-failure` exactly as under `always`.
/// It is fed only to the restart-policy decision; the *real* child exit
/// code is what the audit record reports.
const WATCHDOG_KILL_EXIT_CODE: i32 = -1;

/// The restart backoff for the `attempt`-th relaunch: `base * 2^attempt`,
/// saturating and clamped to [`RESTART_BACKOFF_CAP`].
///
/// Computed in nanoseconds as a `u128`. A shift that would overflow the
/// value (not merely the shift width) saturates to the maximum and is then
/// clamped down, so a large `attempt` yields the cap rather than a wrapped
/// value. The result is always in `RESTART_BACKOFF_BASE_NS..=CAP`.
fn restart_backoff(attempt: u32) -> Duration64 {
    let cap_ns = duration_nanos(RESTART_BACKOFF_CAP);
    // `checked_shl` only rejects a shift wider than the type; a shift that
    // discards significant bits still "succeeds", so verify it round-trips
    // and otherwise saturate.
    let scaled = match RESTART_BACKOFF_BASE_NS.checked_shl(attempt) {
        Some(v) if (v >> attempt) == RESTART_BACKOFF_BASE_NS => v,
        _ => u128::MAX,
    };
    let ns = scaled.min(cap_ns);
    // `ns <= cap_ns`, which is well within `u64`, so the conversion is exact.
    Duration64::from_nanos(u64::try_from(ns).unwrap_or(u64::MAX))
}

/// The whole span `d` in nanoseconds as a `u128`, for backoff arithmetic.
/// A negative span (never produced here) clamps to zero.
fn duration_nanos(d: Duration64) -> u128 {
    let secs = u128::try_from(d.secs()).unwrap_or(0);
    secs * u128::from(NANOS_PER_SEC) + u128::from(d.subsec_nanos())
}

/// Construction-time configuration for an [`Init`] instance.
///
/// All seams are borrowed for the manager's lifetime, mirroring the
/// `drvhost` host configuration: one config per PID 1 process, alive for
/// the whole run.
pub struct InitConfig<'a> {
    /// Seam that launches a verified service binary as its service account.
    ///
    /// The kernel is the single capability authority: it derives each
    /// service's grant from the signed bundle manifest intersected with the
    /// account's ceiling at load time. The manager names only the binary and
    /// the account, never a capability set, so no init-side derivation can
    /// drift from the kernel's authoritative one.
    pub spawner: &'a dyn Spawner,
    /// Seam that stops a running service (graceful request, then force).
    pub stopper: &'a dyn Stopper,
    /// Seam that reports exited children.
    pub reaper: &'a dyn Reaper,
    /// Structured audit log sink.
    pub sink: &'a dyn Sink,
    /// The authority scope this manager instance wields — the fixed security
    /// boundary between the single system manager and a per-user manager.
    ///
    /// A per-user manager ([`AuthorityScope::User`]) may manage only services
    /// that run as its own user; the system manager
    /// ([`AuthorityScope::System`]) may manage any account. The scope is
    /// chosen once when the instance is created and never changes.
    pub scope: AuthorityScope,
}

/// A service that was successfully started during [`Init::start_all`].
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct StartedService {
    /// Service name.
    pub name: String,
    /// Process identifier the [`Spawner`] returned.
    pub pid: Pid,
}

/// A client whose parked connection request has been satisfied because its
/// service reached readiness.
///
/// The manager accumulates these as services become ready; the caller
/// drains them with [`Init::take_ready_clients`] to wake each parked client
/// and hand it the connection to its service's endpoint. A client that
/// connected while its service was already ready is returned synchronously
/// from [`Init::connect`] instead and never appears here.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ReadyClient {
    /// The service the client is now connected to.
    pub service: String,
    /// The client to wake and hand the endpoint.
    pub client: ClientId,
}

/// The outcome of a successful [`Init::connect`].
#[derive(Copy, Clone, Debug, Eq, PartialEq)]
pub enum ActivationOutcome {
    /// The service was already ready: the client is connected now and the
    /// caller may hand it the endpoint immediately.
    Connected,
    /// The service is not yet ready (it was just activated, or is still
    /// starting): the client is parked and will be reported through
    /// [`Init::take_ready_clients`] once the service becomes ready. The
    /// client is never busy-polled.
    Queued,
}

/// A service that was not started during [`Init::start_all`].
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct FailedService {
    /// Service name.
    pub name: String,
    /// Why the service was not started.
    pub failure: StartFailure,
}

/// Outcome of a single [`Init::start_all`] over a structurally valid graph.
///
/// Services init brought up are in `started`; services it refused or
/// skipped are in `failed`. The whole bring-up is reported, so a caller can
/// surface which optional services are absent without the boot aborting.
#[derive(Clone, Debug, Default, Eq, PartialEq)]
pub struct StartReport {
    /// Services that started, in dependency order.
    pub started: Vec<StartedService>,
    /// Services that were refused or skipped, with the reason.
    pub failed: Vec<FailedService>,
}

impl StartReport {
    /// `true` if every registered service started.
    #[must_use]
    pub fn all_started(&self) -> bool {
        self.failed.is_empty()
    }
}

/// One registered service and its live lifecycle state.
///
/// The `pid` is `Some` for exactly the window between a successful spawn and
/// the reap of the process, so it doubles as the "is a process live for this
/// service" flag the reaper matches against.
struct Service {
    spec: ServiceSpec,
    state: ServiceState,
    pid: Option<Pid>,
    /// Clients currently connected to this service's endpoint — the sink
    /// whose count drives idle-stop. Tracked by [`ClientId`] (not a bare
    /// counter) so a disconnect removes exactly the client that left and a
    /// duplicate connect is idempotent.
    sink: Vec<ClientId>,
    /// Clients parked waiting for this service to reach readiness, in
    /// arrival order. Drained into [`Service::sink`] (and reported through
    /// [`Init::ready_clients`]) when the service becomes ready. Bounded by
    /// [`MAX_PENDING_PER_SERVICE`].
    waiters: VecDeque<ClientId>,
    /// When set, the absolute monotonic instant at (or after) which an idle
    /// on-demand service is stopped. Armed when the last client disconnects
    /// and cleared by a new connect or when the stop begins — a single
    /// one-shot deadline, never a poll.
    linger_deadline: Option<Duration64>,
    /// When set, the absolute monotonic instant at (or after) which a
    /// gracefully-stopping service is force-terminated if it has not yet
    /// exited.
    grace_deadline: Option<Duration64>,
    /// When set, the absolute monotonic instant at (or after) which a
    /// crashed service whose [`RestartPolicy`](tairix_abi::RestartPolicy)
    /// asks for it is relaunched.
    /// Armed by [`reap`](Init::reap) after an unexpected exit and consumed
    /// by [`expire_restart_backoff`](Init::expire_restart_backoff) — a
    /// single one-shot deadline, never a spawn-in-a-loop. `Some` here also
    /// marks the service as *pending restart*, so a terminally-`Failed`
    /// state with a live restart deadline does not block its dependents.
    restart_deadline: Option<Duration64>,
    /// How many times this service has been relaunched by the restart
    /// policy since it last ran stably. Grows the backoff and is capped by
    /// [`MAX_RESTART_ATTEMPTS`] so a service that dies the instant it starts
    /// is abandoned rather than relaunched forever (the crash-loop guard).
    restart_attempts: u32,
    /// The monotonic instant at which the restart policy last relaunched
    /// this service, or `None` if it has not been restarted since it was
    /// first started (at boot or on demand). Used to reset
    /// [`restart_attempts`](Service::restart_attempts) once a relaunched
    /// service has run longer than [`RESTART_STABLE_WINDOW`], so a genuine
    /// crash after a long, healthy uptime does not count against the
    /// crash-loop budget.
    relaunched_at: Option<Duration64>,
    /// When set, the absolute monotonic instant at (or after) which the
    /// service is judged to have *wedged* because it has not renewed its
    /// liveness heartbeat ([`Init::heartbeat`](Init::heartbeat)) since. Armed
    /// by [`arm_watchdogs`](Init::arm_watchdogs) once a service with a
    /// non-zero [`ServiceSpec::watchdog`](crate::ServiceSpec::watchdog)
    /// interval is running, pushed forward on every heartbeat, and consumed
    /// by [`expire_watchdog`](Init::expire_watchdog) — a single one-shot
    /// deadline, never a poll. `None` means the service opts out of the
    /// liveness watchdog or is not currently running.
    watchdog_deadline: Option<Duration64>,
    /// Set when [`expire_watchdog`](Init::expire_watchdog) has force-killed a
    /// wedged process, so [`reap`](Init::reap) classifies the resulting exit
    /// as an unexpected *failure* (driving the restart policy) rather than a
    /// clean stop, regardless of the exit code the killed process reports.
    /// Cleared once that exit is reaped or the service is next started.
    killed_by_watchdog: bool,
}

/// PID 1 service manager.
///
/// Bring-up is an event-driven admission engine rather than a single
/// spawn-everything pass: a service is admitted only once every dependency
/// it names is [`ServiceState::is_ready`] and every named readiness
/// condition it requires is satisfied. An [`ReadinessKind::Immediate`]
/// service is ready the instant its spawn succeeds; a
/// [`ReadinessKind::Notify`] service stays `starting` until it announces
/// readiness through [`notify`](Self::notify), so a dependent that needs the
/// service *functional* is never released against one that is merely
/// spawned.
pub struct Init<'a> {
    cfg: InitConfig<'a>,
    services: Vec<Service>,
    /// Dependency-respecting start order, computed and validated by
    /// [`start_all`](Self::start_all) and reused by the readiness-driven
    /// admission passes.
    order: Vec<usize>,
    /// Which named readiness conditions are currently satisfied, indexed by
    /// [`ReadyCondition::as_u16`].
    satisfied: [bool; CONDITION_COUNT],
    /// Parked clients that became connected because their service reached
    /// readiness, awaiting the caller's [`take_ready_clients`](Self::take_ready_clients)
    /// drain. The manager never wakes a client itself; it records who is now
    /// ready and lets the transport layer deliver the endpoint.
    ready_clients: Vec<ReadyClient>,
}

impl<'a> Init<'a> {
    /// Create a manager with no registered services.
    #[must_use]
    pub fn new(cfg: InitConfig<'a>) -> Self {
        Self {
            cfg,
            services: Vec::new(),
            order: Vec::new(),
            satisfied: [false; CONDITION_COUNT],
            ready_clients: Vec::new(),
        }
    }

    /// Number of registered services.
    #[must_use]
    pub fn registered_count(&self) -> usize {
        self.services.len()
    }

    /// The [`AuthorityScope`] this manager instance wields.
    #[must_use]
    pub fn scope(&self) -> AuthorityScope {
        self.cfg.scope
    }

    /// Number of services with a live process — spawned and not yet reaped.
    #[must_use]
    pub fn running_count(&self) -> usize {
        self.services.iter().filter(|s| s.pid.is_some()).count()
    }

    /// The [`Pid`] of a service with a live process, or `None` if it has no
    /// live process (not started, or already reaped).
    #[must_use]
    pub fn running_pid(&self, name: &str) -> Option<Pid> {
        self.index_of(name).and_then(|idx| self.services[idx].pid)
    }

    /// The current [`ServiceState`] of a registered service, or `None` if
    /// no service by that name is registered.
    #[must_use]
    pub fn state_of(&self, name: &str) -> Option<ServiceState> {
        self.index_of(name).map(|idx| self.services[idx].state)
    }

    /// Whether a named readiness condition is currently satisfied.
    #[must_use]
    pub fn condition_satisfied(&self, condition: ReadyCondition) -> bool {
        self.satisfied[condition.as_u16() as usize]
    }

    /// Register a service.
    ///
    /// The service account the spec names must be permitted by this
    /// manager's [`AuthorityScope`]: the system
    /// manager may register any account, but a per-user manager may register
    /// only services that run as its own user. This authority boundary is
    /// enforced before any state is touched (fail closed).
    ///
    /// # Errors
    ///
    /// * [`InitError::ScopeViolation`] if the spec's service account is
    ///   outside this manager's scope (a per-user manager naming a system
    ///   account or another user's uid). Audited as
    ///   [`events::SERVICE_SCOPE_REJECTED`].
    /// * [`InitError::DuplicateService`] if a service with the same name is
    ///   already registered.
    pub fn register(&mut self, spec: ServiceSpec) -> Result<(), InitError> {
        // The authority boundary is checked before any state is touched: a
        // per-user manager may manage only services that run as its own user,
        // so a spec naming a system service account or another user's uid is
        // refused (fail closed) before it can be recorded. The system manager
        // permits any account. This is an identity check on the account the
        // spec already names, never a capability derivation — the kernel
        // remains the single capability authority.
        if !self.cfg.scope.permits_account(spec.account()) {
            self.audit(
                events::SERVICE_SCOPE_REJECTED,
                Level::Warn,
                spec.name(),
                "service account outside manager scope",
            );
            return Err(InitError::ScopeViolation);
        }
        if self.index_of(spec.name()).is_some() {
            return Err(InitError::DuplicateService);
        }
        self.services.push(Service {
            spec,
            state: ServiceState::Inactive,
            pid: None,
            sink: Vec::new(),
            waiters: VecDeque::new(),
            linger_deadline: None,
            grace_deadline: None,
            restart_deadline: None,
            restart_attempts: 0,
            relaunched_at: None,
            watchdog_deadline: None,
            killed_by_watchdog: false,
        });
        Ok(())
    }

    /// Register the discovered service bundles that are **enrolled**,
    /// skipping (and auditing) those that are present on disk but not.
    ///
    /// This is the discovery → registration → activation split
    /// (`plans/NEW-SERVICEMANAGER.md` §3.1): `discovered` is what a scan of
    /// `/System/Services` turned up (each already parsed into a
    /// [`ServiceSpec`] by the loader seam), and `enrolment` is the
    /// fail-closed enrolment record read from the registration store. A
    /// bundle is registered for bring-up **only** if
    /// [`Enrolment::is_enabled`] returns `true` for its name; a discovered
    /// bundle that is not enrolled is never registered — presence on disk
    /// grants no eligibility (no ambient authority). Each skip emits
    /// [`events::SERVICE_NOT_ENROLLED`].
    ///
    /// Registration only records the service; the kernel still derives the
    /// capability grant from the signed bundle and the service account's
    /// ceiling at start ([`Init::start_all`]), so enrolling a service can
    /// never widen the authority it ultimately runs with.
    ///
    /// # Errors
    ///
    /// * [`InitError::ScopeViolation`] if an enrolled bundle's service
    ///   account is outside this manager's scope (a per-user enrolment
    ///   naming a system account or another user's uid) — failing closed
    ///   before any service is brought up, so a mis-scoped enrolment can
    ///   never boot a surprising, over-privileged service.
    /// * [`InitError::DuplicateService`] if two enrolled bundles share a
    ///   name (a packaging defect), failing closed before any is brought up.
    pub fn register_enrolled(
        &mut self,
        discovered: Vec<ServiceSpec>,
        enrolment: &Enrolment,
    ) -> Result<(), InitError> {
        for spec in discovered {
            if enrolment.is_enabled(spec.name()) {
                self.register(spec)?;
            } else {
                self.audit(
                    events::SERVICE_NOT_ENROLLED,
                    Level::Info,
                    spec.name(),
                    "not enrolled",
                );
            }
        }
        Ok(())
    }

    /// Start every registered service in dependency order.
    ///
    /// Services are brought up so that each one starts only after all of
    /// its dependencies. Each service is launched as its own service account
    /// and the kernel grants it `bundle-manifest ∩ account-ceiling` from the
    /// signed bundle; a service whose dependency failed is skipped, and a
    /// service whose spawn is refused is recorded failed — neither aborts the
    /// independent services.
    ///
    /// # Errors
    ///
    /// Returns an [`InitError`] — and starts **nothing** — if the registered
    /// graph is structurally invalid: a dependency names an unregistered
    /// service ([`InitError::DependencyMissing`]) or the graph contains a
    /// cycle ([`InitError::DependencyCycle`]).
    pub fn start_all(&mut self) -> Result<StartReport, InitError> {
        self.order = match self.topological_order() {
            Ok(order) => order,
            Err(err) => {
                self.audit_graph_rejected(err);
                return Err(err);
            }
        };
        Ok(self.pump())
    }

    /// Record that a named readiness condition is satisfied and admit any
    /// services that were waiting only on it.
    ///
    /// Idempotent: satisfying an already-satisfied condition changes
    /// nothing and audits nothing. This is the seam for a condition a
    /// providing service does not itself announce — for example the kernel
    /// signalling [`ReadyCondition::FilesystemsMounted`]. The returned
    /// [`StartReport`] lists the services this newly admitted.
    pub fn satisfy_condition(&mut self, condition: ReadyCondition) -> StartReport {
        self.satisfy_condition_inner(condition);
        self.pump()
    }

    /// Apply a readiness notification a service sent about itself.
    ///
    /// In production the manager maps the kernel-attested sender of a
    /// [`ReadyNotice`](tairix_abi::ReadyNotice) to the service it started
    /// and calls this with that name; the notice never carries the name
    /// itself. A [`LifecycleSignal::Ready`] releases the service's
    /// dependents and satisfies the conditions it provides; a
    /// [`LifecycleSignal::Failed`] marks it failed and skips the dependents
    /// blocked on it. The returned [`StartReport`] lists whatever the
    /// resulting admission pass started or failed.
    ///
    /// # Errors
    ///
    /// * [`NotifyError::UnknownService`] if `name` is not registered.
    /// * [`NotifyError::NotStarting`] if the service is not in
    ///   [`ServiceState::Starting`] — a notice cannot resolve a readiness
    ///   edge that does not exist. The notice is dropped and audited; it is
    ///   never trusted to move the service anyway (fail closed).
    pub fn notify(
        &mut self,
        name: &str,
        signal: LifecycleSignal,
    ) -> Result<StartReport, NotifyError> {
        let Some(idx) = self.index_of(name) else {
            self.audit(
                events::NOTIFY_REJECTED,
                Level::Warn,
                name,
                "unknown service",
            );
            return Err(NotifyError::UnknownService);
        };
        if self.services[idx].state != ServiceState::Starting {
            self.audit(events::NOTIFY_REJECTED, Level::Warn, name, "not starting");
            return Err(NotifyError::NotStarting);
        }
        match signal {
            LifecycleSignal::Ready => self.mark_ready(idx),
            LifecycleSignal::Failed => {
                self.services[idx].state = ServiceState::Failed;
                self.services[idx].pid = None;
                let owned = name.to_string();
                self.audit(
                    events::SERVICE_START_FAILED,
                    Level::Warn,
                    &owned,
                    "reported failure",
                );
            }
        }
        Ok(self.pump())
    }

    /// Connect a client to a service's reserved endpoint, activating the
    /// service on demand if it is not already up.
    ///
    /// This is the one capability-brokered on-demand activation entry point
    /// (`plans/NEW-SERVICEMANAGER.md` §3.4). The manager checks the client
    /// holds the capability the endpoint requires **before** it touches any
    /// state (fail closed), then:
    ///
    /// * if the service is already ready, adds the client to the sink and
    ///   returns [`ActivationOutcome::Connected`] so the caller hands back
    ///   the endpoint immediately;
    /// * if the service is down (and every readiness condition it requires
    ///   is satisfied), starts it as its own service account, parks the
    ///   client, and returns [`ActivationOutcome::Queued`];
    /// * if the service is already starting, parks the client behind the
    ///   others and returns [`ActivationOutcome::Queued`].
    ///
    /// A new connection always cancels a pending idle-linger stop: fresh
    /// interest keeps the service alive. Parked clients are woken through
    /// [`take_ready_clients`](Self::take_ready_clients) when the service
    /// becomes ready — never by polling (§2.23).
    ///
    /// # Errors
    ///
    /// * [`ActivateError::UnknownService`] — no service by that name is
    ///   registered (presence on disk never grants activation).
    /// * [`ActivateError::Denied`] — the client does not hold the endpoint's
    ///   required capability. Refused before the service is touched.
    /// * [`ActivateError::Unavailable`] — a required readiness condition is
    ///   unsatisfied (for example a GUI service on a headless system), or the
    ///   service is mid-teardown or terminally failed.
    /// * [`ActivateError::QueueFull`] — the service's bounded pending-connection
    ///   queue is full.
    /// * [`ActivateError::NotActivatable`] — the service could not be
    ///   launched (the kernel's load gate refused the spawn).
    ///
    /// Every refusal is audited with its reason and the client is granted
    /// nothing.
    pub fn connect(
        &mut self,
        name: &str,
        client_capabilities: &CapabilitySet,
        client: ClientId,
    ) -> Result<ActivationOutcome, ActivateError> {
        let Some(idx) = self.index_of(name) else {
            self.audit(
                events::ACTIVATION_DENIED,
                Level::Warn,
                name,
                "unknown service",
            );
            return Err(ActivateError::UnknownService);
        };

        // Capability check before any state change (fail closed).
        if let Some(required) = self.services[idx].spec.connect_capability() {
            if !client_capabilities.contains(required) {
                self.audit(
                    events::ACTIVATION_DENIED,
                    Level::Warn,
                    name,
                    "capability denied",
                );
                return Err(ActivateError::Denied);
            }
        }

        // Fresh interest: cancel any pending idle-stop.
        self.services[idx].linger_deadline = None;

        match self.services[idx].state {
            ServiceState::Ready | ServiceState::Running => {
                self.attach_client(idx, client);
                Ok(ActivationOutcome::Connected)
            }
            ServiceState::Starting => self.park_client(idx, client),
            ServiceState::Inactive | ServiceState::Stopped => self.activate(idx, client),
            ServiceState::Stopping | ServiceState::Failed => {
                self.audit(
                    events::ACTIVATION_DENIED,
                    Level::Warn,
                    name,
                    "service unavailable",
                );
                Err(ActivateError::Unavailable)
            }
        }
    }

    /// Disconnect a client from a service's endpoint.
    ///
    /// Removes the client from the service's sink (or its pending-connection
    /// queue if it was still waiting). If this was the last interest in a
    /// live on-demand service, the manager arms a single one-shot
    /// idle-linger deadline; after it elapses with no reconnection the
    /// service is idle-stopped ([`expire_linger`](Self::expire_linger)). The
    /// caller reads the armed deadline with
    /// [`linger_deadline`](Self::linger_deadline) to program its one-shot
    /// timer — the manager never polls.
    ///
    /// `now` is the current monotonic instant, from which the linger
    /// deadline is computed. Disconnecting a client that holds no connection
    /// is a harmless no-op.
    ///
    /// # Errors
    ///
    /// [`ActivateError::UnknownService`] if `name` is not registered.
    pub fn disconnect(
        &mut self,
        name: &str,
        client: ClientId,
        now: Duration64,
    ) -> Result<(), ActivateError> {
        let Some(idx) = self.index_of(name) else {
            self.audit(
                events::ACTIVATION_DENIED,
                Level::Warn,
                name,
                "unknown service",
            );
            return Err(ActivateError::UnknownService);
        };

        let in_sink = if let Some(pos) = self.services[idx].sink.iter().position(|c| *c == client) {
            self.services[idx].sink.remove(pos);
            true
        } else {
            false
        };
        let in_waiters =
            if let Some(pos) = self.services[idx].waiters.iter().position(|c| *c == client) {
                self.services[idx].waiters.remove(pos);
                true
            } else {
                false
            };

        let now_idle = in_sink || in_waiters;
        let no_interest =
            self.services[idx].sink.is_empty() && self.services[idx].waiters.is_empty();
        let alive = matches!(
            self.services[idx].state,
            ServiceState::Starting | ServiceState::Ready | ServiceState::Running
        );
        if now_idle && no_interest && alive && self.services[idx].spec.activation().is_on_demand() {
            let linger = self.services[idx]
                .spec
                .activation()
                .linger()
                .unwrap_or(Duration64::ZERO);
            self.services[idx].linger_deadline = Some(add_duration(now, linger));
            let owned = name.to_string();
            self.audit(events::SERVICE_LINGER_ARMED, Level::Info, &owned, "idle");
        }
        Ok(())
    }

    /// Idle-stop a service whose linger deadline has elapsed.
    ///
    /// The caller invokes this when the one-shot linger timer it armed from
    /// [`linger_deadline`](Self::linger_deadline) fires. The manager stops
    /// the service **only** if it is still idle (no connected or pending
    /// clients) and the deadline has genuinely passed at `now`; a
    /// reconnection since the timer was armed leaves the service running and
    /// this a no-op (fail safe). A stop is graceful: the service is asked to
    /// exit and given its grace period before a forced terminate
    /// ([`expire_grace`](Self::expire_grace)).
    ///
    /// Returns `true` if a stop was initiated, `false` if the service is no
    /// longer eligible to be idle-stopped.
    pub fn expire_linger(&mut self, name: &str, now: Duration64) -> bool {
        let Some(idx) = self.index_of(name) else {
            return false;
        };
        let Some(deadline) = self.services[idx].linger_deadline else {
            return false;
        };
        let idle = self.services[idx].sink.is_empty() && self.services[idx].waiters.is_empty();
        let alive = matches!(
            self.services[idx].state,
            ServiceState::Starting | ServiceState::Ready | ServiceState::Running
        );
        if now < deadline || !idle || !alive {
            return false;
        }
        self.begin_stop(idx, now, "idle");
        true
    }

    /// Force a gracefully-stopping service down because its grace period has
    /// elapsed without it exiting.
    ///
    /// The caller invokes this when the one-shot grace timer it armed from
    /// [`grace_deadline`](Self::grace_deadline) fires. The manager forces
    /// the process down **only** if the service is still
    /// [`ServiceState::Stopping`], still has a live process, and the grace
    /// deadline has genuinely passed; otherwise it is a no-op (the service
    /// exited on its own and the [`Reaper`] will observe it).
    ///
    /// Returns `true` if the process was forced down.
    pub fn expire_grace(&mut self, name: &str, now: Duration64) -> bool {
        let Some(idx) = self.index_of(name) else {
            return false;
        };
        if self.services[idx].state != ServiceState::Stopping {
            return false;
        }
        let Some(deadline) = self.services[idx].grace_deadline else {
            return false;
        };
        if now < deadline {
            return false;
        }
        let Some(pid) = self.services[idx].pid else {
            return false;
        };
        let _ = self.cfg.stopper.force_terminate(pid);
        self.services[idx].grace_deadline = None;
        let owned = name.to_string();
        self.audit(
            events::SERVICE_FORCE_TERMINATED,
            Level::Warn,
            &owned,
            "grace elapsed",
        );
        true
    }

    /// Relaunch a service whose restart-backoff deadline has elapsed.
    ///
    /// The caller invokes this when the one-shot timer it armed from
    /// [`restart_deadline`](Self::restart_deadline) fires. The manager
    /// relaunches the service **only** if it still has a pending restart
    /// deadline that has genuinely passed at `now`, it is in a terminal
    /// state (its process really did exit), and it is not on-demand (an
    /// on-demand service comes back on its next connect, never by timer).
    /// Otherwise it is a no-op (fail safe).
    ///
    /// A relaunch records `now` as the service's last relaunch instant (so
    /// the crash-loop budget can later reset once it has run stably), clears
    /// the deadline, returns the service to [`ServiceState::Inactive`], and
    /// drives the admission engine — which brings it back up if its
    /// dependencies are still ready, exactly as at boot. The returned
    /// [`StartReport`] lists whatever that pass started or failed.
    ///
    /// The relaunch is woken by the timer event, never by polling, and the
    /// crash-loop budget already bounded whether a deadline was armed at
    /// all, so this can never become a spawn-in-a-loop.
    pub fn expire_restart_backoff(&mut self, name: &str, now: Duration64) -> StartReport {
        let Some(idx) = self.index_of(name) else {
            return StartReport::default();
        };
        let Some(deadline) = self.services[idx].restart_deadline else {
            return StartReport::default();
        };
        if now < deadline
            || !self.services[idx].state.is_terminal()
            || self.services[idx].spec.activation().is_on_demand()
        {
            return StartReport::default();
        }
        self.services[idx].restart_deadline = None;
        self.services[idx].relaunched_at = Some(now);
        self.services[idx].state = ServiceState::Inactive;
        self.pump()
    }

    /// Arm the liveness watchdog for every running service that has opted in
    /// but is not yet being watched.
    ///
    /// A service with a non-zero
    /// [`ServiceSpec::watchdog`](crate::ServiceSpec::watchdog) interval is
    /// placed under the watchdog once it is running (`Ready`/`Running`) with
    /// a live process: its [`watchdog_deadline`](Self::watchdog_deadline) is
    /// armed to `now + interval`. Arming is idempotent — a service already
    /// being watched (its deadline is `Some`, possibly pushed forward by a
    /// [`heartbeat`](Self::heartbeat)) is left untouched — so the caller may
    /// invoke this after every admission/reap pass without disturbing a live
    /// countdown. Each newly-armed service is audited once
    /// ([`events::SERVICE_WATCHDOG_ARMED`]).
    ///
    /// The caller programs its one-shot timer from the soonest armed
    /// [`watchdog_deadline`](Self::watchdog_deadline) and calls
    /// [`expire_watchdog`](Self::expire_watchdog) when it fires — a single
    /// one-shot wakeup, never a poll.
    pub fn arm_watchdogs(&mut self, now: Duration64) {
        for idx in 0..self.services.len() {
            let interval = self.services[idx].spec.watchdog();
            if interval == Duration64::ZERO {
                continue;
            }
            let watched = self.services[idx].pid.is_some()
                && matches!(
                    self.services[idx].state,
                    ServiceState::Ready | ServiceState::Running
                );
            if !watched || self.services[idx].watchdog_deadline.is_some() {
                continue;
            }
            self.services[idx].watchdog_deadline = Some(add_duration(now, interval));
            let name = self.services[idx].spec.name().to_string();
            self.audit(
                events::SERVICE_WATCHDOG_ARMED,
                Level::Info,
                &name,
                "watchdog armed",
            );
        }
    }

    /// Renew a running service's liveness heartbeat.
    ///
    /// The supervised service (a driver or daemon) calls this through the
    /// manager's control transport to say "I am still making progress". Its
    /// [`watchdog_deadline`](Self::watchdog_deadline) is pushed forward to
    /// `now + interval`, so the watchdog fires only if the service later
    /// goes silent for a whole interval. A heartbeat is a high-frequency,
    /// steady-state signal and is deliberately **not** audited — one record
    /// per heartbeat would flood the log with no diagnostic value.
    ///
    /// Returns `true` if the heartbeat was accepted (the service is
    /// registered, opts into the watchdog, and is running with a live
    /// process). A heartbeat from an unknown, watchdog-less, or not-running
    /// service is a harmless no-op returning `false` (fail safe) — never an
    /// error that could be used to probe which services exist.
    pub fn heartbeat(&mut self, name: &str, now: Duration64) -> bool {
        let Some(idx) = self.index_of(name) else {
            return false;
        };
        let interval = self.services[idx].spec.watchdog();
        if interval == Duration64::ZERO {
            return false;
        }
        let watched = self.services[idx].pid.is_some()
            && matches!(
                self.services[idx].state,
                ServiceState::Ready | ServiceState::Running
            );
        if !watched {
            return false;
        }
        self.services[idx].watchdog_deadline = Some(add_duration(now, interval));
        true
    }

    /// The armed liveness-watchdog deadline of a service, or `None` if it is
    /// not currently being watched. The caller programs its one-shot timer
    /// from this and calls [`expire_watchdog`](Self::expire_watchdog) when it
    /// fires — a single one-shot wakeup, never a poll.
    #[must_use]
    pub fn watchdog_deadline(&self, name: &str) -> Option<Duration64> {
        self.index_of(name)
            .and_then(|idx| self.services[idx].watchdog_deadline)
    }

    /// Handle a service that has missed its liveness-watchdog deadline: its
    /// process has wedged.
    ///
    /// The caller invokes this when the one-shot timer it armed from
    /// [`watchdog_deadline`](Self::watchdog_deadline) fires. The manager
    /// force-terminates the wedged process **only** if it is still running
    /// (`Ready`/`Running`) with a live process and its watchdog deadline has
    /// genuinely passed at `now`; a heartbeat since the timer was armed, or a
    /// stop or exit already in flight, leaves it untouched (fail safe). A
    /// healthy service keeps renewing its deadline, so it is never reached
    /// here.
    ///
    /// The kill is classified as an unexpected *failure*, not a clean stop:
    /// the watchdog deadline is cleared and `killed_by_watchdog` is set so
    /// [`reap`](Self::reap) drives the [`RestartPolicy`](tairix_abi::RestartPolicy)
    /// exactly as for any other abnormal exit (a wedged `on-failure` or
    /// `always` service is relaunched with the bounded crash-loop budget and
    /// backoff; a `never` service is killed and left down, loudly). The
    /// manager does not itself set a terminal state — it forces the process
    /// down and lets [`reap`](Self::reap) observe the exit, mirroring the
    /// graceful force-terminate path ([`expire_grace`](Self::expire_grace)).
    ///
    /// Returns `true` if the process was force-terminated for wedging.
    pub fn expire_watchdog(&mut self, name: &str, now: Duration64) -> bool {
        let Some(idx) = self.index_of(name) else {
            return false;
        };
        let Some(deadline) = self.services[idx].watchdog_deadline else {
            return false;
        };
        let alive = matches!(
            self.services[idx].state,
            ServiceState::Ready | ServiceState::Running
        );
        if now < deadline || !alive {
            return false;
        }
        let Some(pid) = self.services[idx].pid else {
            return false;
        };
        // Best-effort force-down; if it cannot be delivered the process is
        // already exiting and the reaper will observe it. The exit is
        // classified as a watchdog failure whichever way it goes.
        let _ = self.cfg.stopper.force_terminate(pid);
        self.services[idx].watchdog_deadline = None;
        self.services[idx].killed_by_watchdog = true;
        let name = self.services[idx].spec.name().to_string();
        self.audit(
            events::SERVICE_WATCHDOG_TIMEOUT,
            Level::Warn,
            &name,
            "liveness watchdog elapsed",
        );
        true
    }

    /// Drain the clients whose parked connections have been satisfied since
    /// the last call.
    ///
    /// A client parked by [`connect`](Self::connect) is reported here once
    /// its service reaches readiness (through the boot admission engine, a
    /// readiness notice, or a satisfied condition). The caller wakes each
    /// one and hands it the connection to its service's endpoint. Draining
    /// clears the buffer, so each satisfied client is reported exactly once.
    #[must_use]
    pub fn take_ready_clients(&mut self) -> Vec<ReadyClient> {
        core::mem::take(&mut self.ready_clients)
    }

    /// The armed idle-linger deadline of a service, or `None` if it has no
    /// pending idle-stop. The caller programs its one-shot timer from this.
    #[must_use]
    pub fn linger_deadline(&self, name: &str) -> Option<Duration64> {
        self.index_of(name)
            .and_then(|idx| self.services[idx].linger_deadline)
    }

    /// The armed graceful-stop grace deadline of a service, or `None` if it
    /// is not stopping. The caller programs its one-shot timer from this.
    #[must_use]
    pub fn grace_deadline(&self, name: &str) -> Option<Duration64> {
        self.index_of(name)
            .and_then(|idx| self.services[idx].grace_deadline)
    }

    /// The armed restart-backoff deadline of a service, or `None` if it has
    /// no pending restart. The caller programs its one-shot timer from this
    /// and calls [`expire_restart_backoff`](Self::expire_restart_backoff)
    /// when it fires — a single one-shot wakeup, never a poll.
    #[must_use]
    pub fn restart_deadline(&self, name: &str) -> Option<Duration64> {
        self.index_of(name)
            .and_then(|idx| self.services[idx].restart_deadline)
    }

    /// Number of clients currently connected to a service's endpoint (its
    /// sink), or `0` if no such service is registered.
    #[must_use]
    pub fn connected_count(&self, name: &str) -> usize {
        self.index_of(name)
            .map_or(0, |idx| self.services[idx].sink.len())
    }

    /// Number of clients currently parked waiting for a service to become
    /// ready, or `0` if no such service is registered.
    #[must_use]
    pub fn pending_count(&self, name: &str) -> usize {
        self.index_of(name)
            .map_or(0, |idx| self.services[idx].waiters.len())
    }

    /// Add a client to a service's connected sink, without duplicating one
    /// that is already connected.
    fn attach_client(&mut self, idx: usize, client: ClientId) {
        if !self.services[idx].sink.contains(&client) {
            self.services[idx].sink.push(client);
        }
    }

    /// Park a client behind a not-yet-ready service, failing closed if the
    /// bounded pending-connection queue is full.
    fn park_client(
        &mut self,
        idx: usize,
        client: ClientId,
    ) -> Result<ActivationOutcome, ActivateError> {
        if self.services[idx].waiters.contains(&client) {
            return Ok(ActivationOutcome::Queued);
        }
        if self.services[idx].waiters.len() >= MAX_PENDING_PER_SERVICE {
            let name = self.services[idx].spec.name().to_string();
            self.audit(events::ACTIVATION_DENIED, Level::Warn, &name, "queue full");
            return Err(ActivateError::QueueFull);
        }
        self.services[idx].waiters.push_back(client);
        let name = self.services[idx].spec.name().to_string();
        self.audit(
            events::ACTIVATION_QUEUED,
            Level::Info,
            &name,
            "awaiting readiness",
        );
        Ok(ActivationOutcome::Queued)
    }

    /// Start a down service on demand and connect the requesting client:
    /// spawn it (fail closed if its conditions are unmet or the spawn is
    /// refused), then either connect the client immediately (an immediate
    /// service is ready at once) or park it until the service notifies ready.
    fn activate(
        &mut self,
        idx: usize,
        client: ClientId,
    ) -> Result<ActivationOutcome, ActivateError> {
        if !self.admissible(idx) {
            let name = self.services[idx].spec.name().to_string();
            self.audit(
                events::ACTIVATION_DENIED,
                Level::Warn,
                &name,
                "conditions unmet",
            );
            return Err(ActivateError::Unavailable);
        }
        match self.try_start(idx) {
            Ok(_) => {
                let name = self.services[idx].spec.name().to_string();
                self.audit(
                    events::SERVICE_ACTIVATED,
                    Level::Info,
                    &name,
                    "on-demand connect",
                );
                if self.services[idx].spec.readiness() == ReadinessKind::Immediate {
                    self.mark_ready(idx);
                    // Promote this service Ready -> Running and release any
                    // order-based dependents this activation satisfied.
                    self.pump();
                    self.attach_client(idx, client);
                    Ok(ActivationOutcome::Connected)
                } else {
                    self.park_client(idx, client)
                }
            }
            // `try_start` already set the service `Failed` and audited the
            // spawn failure; the client is granted nothing.
            Err(_) => Err(ActivateError::NotActivatable),
        }
    }

    /// Begin a graceful stop of service `idx`: ask it to exit, move it to
    /// [`ServiceState::Stopping`], and arm the grace deadline after which it
    /// is force-terminated. Clears any pending idle-linger and cancels any
    /// pending restart (a stop the manager asked for is never fought with a
    /// relaunch).
    fn begin_stop(&mut self, idx: usize, now: Duration64, reason: &str) {
        self.services[idx].linger_deadline = None;
        self.services[idx].restart_deadline = None;
        // A stop the manager asked for must never be second-guessed by the
        // liveness watchdog: disarm it so a service that is deliberately
        // being torn down is not force-killed and relaunched as if wedged.
        self.services[idx].watchdog_deadline = None;
        self.services[idx].killed_by_watchdog = false;
        if let Some(pid) = self.services[idx].pid {
            // Best-effort graceful request; if it cannot be delivered the
            // process is already exiting and the reaper will observe it.
            let _ = self.cfg.stopper.request_stop(pid);
        }
        self.services[idx].state = ServiceState::Stopping;
        let grace = self.services[idx].spec.stop_grace();
        self.services[idx].grace_deadline = Some(add_duration(now, grace));
        let name = self.services[idx].spec.name().to_string();
        self.audit(events::SERVICE_STOPPING, Level::Info, &name, reason);
    }

    /// Whether service `idx` has a live-or-starting process the manager
    /// should gracefully stop — as opposed to one already inactive, stopped,
    /// or terminally failed.
    fn is_alive(&self, idx: usize) -> bool {
        matches!(
            self.services[idx].state,
            ServiceState::Starting | ServiceState::Ready | ServiceState::Running
        )
    }

    /// Indices in **reverse-dependency order**: every service appears before
    /// the services it depends on, so tearing down in this order never stops
    /// a dependency while a dependent still needs it.
    ///
    /// It is the reverse of the topological start order. The graph was
    /// validated when it was registered ([`start_all`](Self::start_all)), so
    /// the topological sort succeeds; the registration-order fallback keeps
    /// the function total if it is ever called on an unvalidated graph.
    fn reverse_stop_order(&self) -> Vec<usize> {
        let mut order = self
            .topological_order()
            .unwrap_or_else(|_| (0..self.services.len()).collect());
        order.reverse();
        order
    }

    /// Stop `name` and every service that transitively depends on it, in
    /// reverse-dependency order (dependents first).
    ///
    /// A dependent is never stopped after the service it depends on: the
    /// manager tears the closure down dependents-first so nothing is left
    /// running against a stopped prerequisite. Each stop is graceful — the
    /// service is asked to exit and force-terminated only if it overruns its
    /// grace period ([`expire_grace`](Self::expire_grace)) — and any pending
    /// restart in the closure is cancelled (a stop is honoured, never fought
    /// with a relaunch). Services in the closure that are already down are
    /// skipped.
    ///
    /// This is the engine mechanism; the capability-checked control surface
    /// that gates *who* may stop a service is layered above it. `now` is the
    /// current monotonic instant, from which each grace deadline is computed.
    ///
    /// # Errors
    ///
    /// [`ActivateError::UnknownService`] if `name` is not registered (fail
    /// closed — an unknown name never triggers a wider teardown).
    pub fn stop(&mut self, name: &str, now: Duration64) -> Result<(), ActivateError> {
        let Some(target) = self.index_of(name) else {
            self.audit(
                events::ACTIVATION_DENIED,
                Level::Warn,
                name,
                "unknown service",
            );
            return Err(ActivateError::UnknownService);
        };
        let closure = self.dependent_closure(target);
        for idx in self.reverse_stop_order() {
            if !closure[idx] {
                continue;
            }
            // Cancel a pending restart even for a service that is already
            // down: a deliberate stop supersedes a queued relaunch.
            self.services[idx].restart_deadline = None;
            if self.is_alive(idx) {
                self.begin_stop(idx, now, "stop");
            }
        }
        Ok(())
    }

    /// Stop **every** service in reverse-dependency order — the
    /// system-shutdown teardown.
    ///
    /// Tears the whole registered set down dependents-first (the reverse of
    /// the boot start order) so no service is stopped while another still
    /// depends on it. Every stop is graceful with its own grace deadline,
    /// and every pending restart is cancelled first so a service the manager
    /// is shutting down is never relaunched underneath it. `now` is the
    /// current monotonic instant, from which the grace deadlines are
    /// computed.
    ///
    /// In the full system-shutdown sequence a per-user manager stops its
    /// user's services this way and exits before the system manager calls
    /// this for the system services; the caller then reaps the graceful
    /// exits and force-terminates any that overrun
    /// ([`expire_grace`](Self::expire_grace)).
    pub fn shutdown(&mut self, now: Duration64) {
        for idx in self.reverse_stop_order() {
            self.services[idx].restart_deadline = None;
            if self.is_alive(idx) {
                self.begin_stop(idx, now, "shutdown");
            }
        }
    }

    /// Apply a decoded [`ServiceControlRequest`] — the engine side of the
    /// capability-gated service-control surface
    /// (`plans/NEW-SERVICEMANAGER.md` §3.8; the `systemctl` analogue).
    ///
    /// Authorization is the *endpoint's*: the kernel gates reaching this
    /// manager's control endpoint on the send capability the manager binds
    /// it with, so this dispatch does not re-check a caller capability (the
    /// receiver need not re-check what the kernel enforced at dispatch). It
    /// validates the request against the strict service-name policy, applies
    /// the operation, and fails closed — auditing every refusal and changing
    /// nothing on one.
    ///
    /// * [`ServiceControlOp::Start`] brings a specific registered service up
    ///   now ([`start_service`](Self::start_service)).
    /// * [`ServiceControlOp::Stop`] tears it and its dependents down in
    ///   reverse-dependency order ([`stop`](Self::stop)).
    ///
    /// Returns the service's resulting [`ServiceState`]. `now` is the current
    /// monotonic instant, from which a graceful-stop grace deadline is
    /// computed.
    ///
    /// # Errors
    ///
    /// The typed [`ControlError`] for a refused request: an unknown or
    /// policy-invalid name ([`ControlError::UnknownService`]), a service that
    /// cannot be started in its current state or with a required condition
    /// unmet ([`ControlError::Unavailable`]), or a spawn the kernel's load
    /// gate refused ([`ControlError::NotStartable`]).
    pub fn control(
        &mut self,
        request: ServiceControlRequest<'_>,
        now: Duration64,
    ) -> Result<ServiceState, ControlError> {
        match request.op {
            ServiceControlOp::Start => self.start_service(request.name),
            ServiceControlOp::Stop => {
                if validate_service_name(request.name).is_err()
                    || self.index_of(request.name).is_none()
                {
                    self.audit(
                        events::SERVICE_CONTROL_DENIED,
                        Level::Warn,
                        request.name,
                        "unknown service",
                    );
                    return Err(ControlError::UnknownService);
                }
                // The name is registered, so the graceful teardown cannot
                // fail closed on an unknown service; map any residual error
                // to the same fail-closed outcome regardless.
                self.stop(request.name, now)
                    .map_err(|_| ControlError::UnknownService)?;
                let state = self.state_of(request.name).unwrap_or(ServiceState::Stopped);
                let owned = request.name.to_string();
                self.audit(
                    events::SERVICE_CONTROL_STOPPED,
                    Level::Info,
                    &owned,
                    "control stop",
                );
                Ok(state)
            }
        }
    }

    /// Bring a specific registered service up now, on a control request — the
    /// engine side of the control surface's `start`.
    ///
    /// Idempotent with respect to a service that is already coming up or up:
    /// a [`ServiceState::Starting`], [`ServiceState::Ready`], or
    /// [`ServiceState::Running`] service returns its current state unchanged.
    /// A down service ([`ServiceState::Inactive`], [`ServiceState::Stopped`],
    /// or a terminally [`ServiceState::Failed`] one) is admitted exactly like
    /// a boot admission — spawned as its own service account, marked ready if
    /// it is [`ReadinessKind::Immediate`], and its order-dependents released —
    /// but only when every readiness condition it requires is satisfied, so
    /// the headless `display-present` case fails closed rather than starting a
    /// GUI-only service. A pending restart backoff is cancelled first: an
    /// explicit start supersedes a queued relaunch, it never races it.
    ///
    /// # Errors
    ///
    /// * [`ControlError::UnknownService`] — the name is unregistered or fails
    ///   the strict service-name policy.
    /// * [`ControlError::Unavailable`] — a required readiness condition is
    ///   unmet, or the service is mid-teardown ([`ServiceState::Stopping`]).
    /// * [`ControlError::NotStartable`] — the kernel's load gate refused the
    ///   spawn.
    pub fn start_service(&mut self, name: &str) -> Result<ServiceState, ControlError> {
        if validate_service_name(name).is_err() {
            self.audit(
                events::SERVICE_CONTROL_DENIED,
                Level::Warn,
                name,
                "invalid service name",
            );
            return Err(ControlError::UnknownService);
        }
        let Some(idx) = self.index_of(name) else {
            self.audit(
                events::SERVICE_CONTROL_DENIED,
                Level::Warn,
                name,
                "unknown service",
            );
            return Err(ControlError::UnknownService);
        };
        match self.services[idx].state {
            ServiceState::Starting | ServiceState::Ready | ServiceState::Running => {
                Ok(self.services[idx].state)
            }
            ServiceState::Stopping => {
                self.audit(
                    events::SERVICE_CONTROL_DENIED,
                    Level::Warn,
                    name,
                    "service is stopping",
                );
                Err(ControlError::Unavailable)
            }
            ServiceState::Inactive | ServiceState::Stopped | ServiceState::Failed => {
                // An explicit start supersedes any queued restart backoff and
                // resets the service to a clean admission candidate.
                self.services[idx].restart_deadline = None;
                self.services[idx].state = ServiceState::Inactive;
                if !self.admissible(idx) {
                    self.audit(
                        events::SERVICE_CONTROL_DENIED,
                        Level::Warn,
                        name,
                        "readiness conditions unmet",
                    );
                    return Err(ControlError::Unavailable);
                }
                match self.try_start(idx) {
                    Ok(_) => {
                        if self.services[idx].spec.readiness() == ReadinessKind::Immediate {
                            self.mark_ready(idx);
                            // Promote Ready -> Running and release any
                            // order-dependents this start satisfied.
                            self.pump();
                        }
                        let owned = name.to_string();
                        self.audit(
                            events::SERVICE_CONTROL_STARTED,
                            Level::Info,
                            &owned,
                            "control start",
                        );
                        Ok(self.services[idx].state)
                    }
                    // `try_start` already set the service `Failed` and audited
                    // the spawn failure; the request changes nothing more.
                    Err(_) => Err(ControlError::NotStartable),
                }
            }
        }
    }

    /// The set of services made up of `target` and everything that
    /// transitively depends on it, as a per-service boolean mask.
    ///
    /// A breadth-first walk over the reverse-dependency edges (a service is
    /// a dependent of `d` when it names `d` in its dependencies), so stopping
    /// `target` can tear down exactly the services that would be left running
    /// against a stopped prerequisite, and no others.
    fn dependent_closure(&self, target: usize) -> Vec<bool> {
        let n = self.services.len();
        let mut in_set = vec![false; n];
        in_set[target] = true;
        let mut queue: VecDeque<usize> = VecDeque::new();
        queue.push_back(target);
        while let Some(d) = queue.pop_front() {
            let dep_name = self.services[d].spec.name();
            for (i, service) in self.services.iter().enumerate() {
                if in_set[i] {
                    continue;
                }
                if service
                    .spec
                    .dependencies()
                    .iter()
                    .any(|dep| dep == dep_name)
                {
                    in_set[i] = true;
                    queue.push_back(i);
                }
            }
        }
        in_set
    }

    /// Reap every child that has exited, returning the number reaped.
    ///
    /// A reaped process that matches a started service is logged as a
    /// service exit and its lifecycle moved to a terminal state. A service
    /// the manager had asked to stop ([`ServiceState::Stopping`]) reaches
    /// [`ServiceState::Stopped`] whatever its exit code — it was told to go,
    /// so a non-zero code is not a failure. Otherwise a clean exit is
    /// [`ServiceState::Stopped`] and a non-zero exit
    /// [`ServiceState::Failed`]. Any other reaped process is an inherited
    /// orphan and is logged as such (PID 1 reaps the whole system's zombies).
    ///
    /// A service whose [`RestartPolicy`](tairix_abi::RestartPolicy) asks to
    /// come back — and whose exit the manager did **not** itself initiate
    /// (an idle-stop or shutdown is honoured, never fought) — is scheduled
    /// for relaunch after a bounded exponential backoff computed from `now`
    /// rather than restarted on the spot: the caller arms a single one-shot
    /// timer from [`restart_deadline`](Self::restart_deadline) and calls
    /// [`expire_restart_backoff`](Self::expire_restart_backoff) when it
    /// fires (no spawn-in-a-loop, no busy-poll). A tight crash loop is
    /// bounded by `MAX_RESTART_ATTEMPTS`; a service that had run past the
    /// stable window since its last relaunch is treated as a fresh failure
    /// and restarted with a full budget.
    ///
    /// `now` is the current monotonic instant, from which a restart backoff
    /// deadline is computed. The process is gone, so its activation state is
    /// cleared: its sink and pending waiters are dropped and any armed
    /// linger or grace deadline is disarmed. Clients whose connections died
    /// with the process are the transport layer's to notice; the manager
    /// holds no stale references.
    pub fn reap(&mut self, now: Duration64) -> usize {
        let mut reaped = 0;
        while let Some(child) = self.cfg.reaper.collect() {
            reaped += 1;
            if let Some(pos) = self.services.iter().position(|s| s.pid == Some(child.pid)) {
                let name = self.services[pos].spec.name().to_string();
                let was_stopping = self.services[pos].state == ServiceState::Stopping;
                let watchdog_kill = self.services[pos].killed_by_watchdog;
                self.services[pos].pid = None;
                // A watchdog kill is an unexpected *failure*, never a clean
                // stop: the process was wedged, so it fails regardless of the
                // exit code the forced termination happens to report.
                self.services[pos].state =
                    if !watchdog_kill && (was_stopping || child.exit_code == 0) {
                        ServiceState::Stopped
                    } else {
                        ServiceState::Failed
                    };
                self.services[pos].sink.clear();
                self.services[pos].waiters.clear();
                self.services[pos].linger_deadline = None;
                self.services[pos].grace_deadline = None;
                self.services[pos].restart_deadline = None;
                self.services[pos].watchdog_deadline = None;
                self.services[pos].killed_by_watchdog = false;
                self.audit_exit(&name, child);
                // A manager-initiated stop is final: the manager asked it to
                // go, so it is never fought with a restart. Only an
                // *unexpected* exit of a still-wanted service is a restart
                // candidate. A watchdog kill is such an exit, and is fed to
                // the restart policy as an abnormal exit so `OnFailure` and
                // `Always` both relaunch a wedged process (`Never` still
                // leaves it down, loudly).
                if !was_stopping {
                    let exit_code = if watchdog_kill {
                        WATCHDOG_KILL_EXIT_CODE
                    } else {
                        child.exit_code
                    };
                    self.schedule_restart(pos, exit_code, now);
                }
            } else {
                self.audit_orphan(child);
            }
        }
        reaped
    }

    /// Consider a just-exited service for a policy-driven restart, arming a
    /// bounded-backoff relaunch deadline when one is due.
    ///
    /// The exit was not manager-initiated (the caller checked). If the
    /// service's [`RestartPolicy`](tairix_abi::RestartPolicy) wants a
    /// restart for this exit code and the crash-loop budget is not spent,
    /// the manager arms [`restart_deadline`](Service::restart_deadline) and
    /// audits it; if the budget is spent it audits the give-up and leaves
    /// the service down (fail closed — never an unbounded relaunch loop).
    fn schedule_restart(&mut self, idx: usize, exit_code: i32, now: Duration64) {
        if !self.services[idx].spec.restart().should_restart(exit_code) {
            return;
        }
        // Reset the crash-loop budget if the service had run stably since
        // its last relaunch (or was never restarted — it ran since boot).
        let ran_stably = self.services[idx]
            .relaunched_at
            .is_none_or(|t| duration_since(now, t) >= RESTART_STABLE_WINDOW);
        if ran_stably {
            self.services[idx].restart_attempts = 0;
        }
        let name = self.services[idx].spec.name().to_string();
        if self.services[idx].restart_attempts >= MAX_RESTART_ATTEMPTS {
            self.audit(
                events::SERVICE_RESTART_EXHAUSTED,
                Level::Warn,
                &name,
                "crash-loop budget spent",
            );
            return;
        }
        let backoff = restart_backoff(self.services[idx].restart_attempts);
        self.services[idx].restart_deadline = Some(add_duration(now, backoff));
        self.services[idx].restart_attempts += 1;
        self.audit(
            events::SERVICE_RESTART_SCHEDULED,
            Level::Info,
            &name,
            "restart scheduled",
        );
    }

    /// Drive the admission engine to a fixpoint: repeatedly admit every
    /// service whose dependencies are all ready and whose required
    /// conditions are all satisfied, and skip every service a failed
    /// dependency blocks, until no service changes state. An
    /// [`ReadinessKind::Immediate`] service is marked ready the moment it
    /// spawns, so a chain of immediate services comes up in a single pass;
    /// a [`ReadinessKind::Notify`] service pauses the chain until it
    /// announces readiness. Finally, promote every service still resting in
    /// the transient [`ServiceState::Ready`] to [`ServiceState::Running`]
    /// now that its dependents have been released.
    ///
    /// On-demand services (`plans/NEW-SERVICEMANAGER.md` §3.4) are **not**
    /// started here: they rest `Inactive` at boot and come up only when a
    /// client connects to their endpoint ([`connect`](Self::connect)).
    fn pump(&mut self) -> StartReport {
        let mut report = StartReport::default();
        loop {
            let mut changed = false;
            for i in 0..self.order.len() {
                let idx = self.order[i];
                if self.services[idx].state != ServiceState::Inactive {
                    continue;
                }
                // On-demand services are never eagerly started at boot: they
                // are activated when a client first connects to their
                // endpoint (`connect`). Leave them resting `Inactive` here.
                if self.services[idx].spec.activation().is_on_demand() {
                    continue;
                }
                if self.dependency_failed(idx) {
                    let name = self.services[idx].spec.name().to_string();
                    self.services[idx].state = ServiceState::Failed;
                    self.audit(
                        events::SERVICE_SKIPPED,
                        Level::Warn,
                        &name,
                        "dependency failed",
                    );
                    report.failed.push(FailedService {
                        name,
                        failure: StartFailure::DependencyFailed,
                    });
                    changed = true;
                    continue;
                }
                if !self.admissible(idx) {
                    continue;
                }
                match self.try_start(idx) {
                    Ok(started) => {
                        if self.services[idx].spec.readiness() == ReadinessKind::Immediate {
                            self.mark_ready(idx);
                        }
                        report.started.push(started);
                    }
                    Err(failure) => report.failed.push(failure),
                }
                changed = true;
            }
            if !changed {
                break;
            }
        }
        for service in &mut self.services {
            if service.state == ServiceState::Ready {
                service.state = ServiceState::Running;
            }
        }
        report
    }

    /// Whether service `idx` may start now: every dependency it names has
    /// reached readiness and every named condition it requires is satisfied.
    fn admissible(&self, idx: usize) -> bool {
        let service = &self.services[idx];
        let deps_ready = service.spec.dependencies().iter().all(|dep| {
            self.index_of(dep)
                .is_some_and(|d| self.services[d].state.is_ready())
        });
        let conditions_met = service
            .spec
            .requires()
            .iter()
            .all(|cond| self.satisfied[cond.as_u16() as usize]);
        deps_ready && conditions_met
    }

    /// Whether any dependency of service `idx` has reached the terminal
    /// [`ServiceState::Failed`] state *for good*, so `idx` can never be
    /// admitted and is skipped.
    ///
    /// A dependency that is `Failed` but has a live restart-backoff deadline
    /// is not counted: it is coming back, so its dependent waits for the
    /// relaunch rather than being permanently skipped.
    fn dependency_failed(&self, idx: usize) -> bool {
        self.services[idx].spec.dependencies().iter().any(|dep| {
            self.index_of(dep).is_some_and(|d| {
                self.services[d].state == ServiceState::Failed
                    && self.services[d].restart_deadline.is_none()
            })
        })
    }

    /// Transition service `idx` across its readiness edge: mark it
    /// [`ServiceState::Ready`] (the [`pump`](Self::pump) promotes it to
    /// `Running` once its dependents are released), audit the readiness, and
    /// satisfy every named condition it provides.
    fn mark_ready(&mut self, idx: usize) {
        self.services[idx].state = ServiceState::Ready;
        let name = self.services[idx].spec.name().to_string();
        self.audit_ready(&name);
        let provides: Vec<ReadyCondition> = self.services[idx].spec.provides().to_vec();
        for condition in provides {
            self.satisfy_condition_inner(condition);
        }
        // Release every client parked waiting for this service to come up:
        // move it into the connected sink and record it so the caller can
        // wake it with the endpoint. The client is woken by this readiness
        // event, never by polling.
        while let Some(client) = self.services[idx].waiters.pop_front() {
            if !self.services[idx].sink.contains(&client) {
                self.services[idx].sink.push(client);
            }
            self.ready_clients.push(ReadyClient {
                service: name.clone(),
                client,
            });
        }
    }

    /// Mark a condition satisfied and audit the transition, unless it was
    /// already satisfied (idempotent, no duplicate audit).
    fn satisfy_condition_inner(&mut self, condition: ReadyCondition) {
        let slot = condition.as_u16() as usize;
        if !self.satisfied[slot] {
            self.satisfied[slot] = true;
            self.audit_condition(condition);
        }
    }

    fn index_of(&self, name: &str) -> Option<usize> {
        self.services.iter().position(|s| s.spec.name() == name)
    }

    /// Compute a dependency-respecting start order, or report a structural
    /// defect. Ready services are emitted in registration order so the
    /// result is deterministic.
    fn topological_order(&self) -> Result<Vec<usize>, InitError> {
        let n = self.services.len();
        let mut indegree = vec![0usize; n];
        let mut dependents: Vec<Vec<usize>> = vec![Vec::new(); n];
        for (i, service) in self.services.iter().enumerate() {
            for dep in service.spec.dependencies() {
                let d = self.index_of(dep).ok_or(InitError::DependencyMissing)?;
                dependents[d].push(i);
                indegree[i] += 1;
            }
        }

        let mut queue: VecDeque<usize> = VecDeque::new();
        for (i, &deg) in indegree.iter().enumerate() {
            if deg == 0 {
                queue.push_back(i);
            }
        }

        let mut order = Vec::with_capacity(n);
        while let Some(i) = queue.pop_front() {
            order.push(i);
            for &dependent in &dependents[i] {
                indegree[dependent] -= 1;
                if indegree[dependent] == 0 {
                    queue.push_back(dependent);
                }
            }
        }

        if order.len() == n {
            Ok(order)
        } else {
            Err(InitError::DependencyCycle)
        }
    }

    /// Spawn service `idx`: launch it as its own service account and
    /// transition it to [`ServiceState::Starting`] with its live [`Pid`]. The
    /// kernel derives the service's capability grant from the signed bundle
    /// (`manifest ∩ account-ceiling`) at load time — the manager never
    /// computes one. On a spawn failure the service is transitioned to
    /// [`ServiceState::Failed`] and the reason recorded, so a failed service
    /// is never left resting in `Inactive` where `pump` would retry it.
    fn try_start(&mut self, idx: usize) -> Result<StartedService, FailedService> {
        let name = self.services[idx].spec.name().to_string();

        let pid = match self.cfg.spawner.spawn(&self.services[idx].spec) {
            Ok(pid) => pid,
            Err(err) => {
                self.services[idx].state = ServiceState::Failed;
                self.audit(
                    events::SERVICE_START_FAILED,
                    Level::Warn,
                    &name,
                    "spawn failed",
                );
                return Err(FailedService {
                    name,
                    failure: StartFailure::SpawnFailed(err),
                });
            }
        };

        self.services[idx].state = ServiceState::Starting;
        self.services[idx].pid = Some(pid);
        self.audit_started(&name, pid);
        Ok(StartedService { name, pid })
    }

    fn emit(&self, level: Level, id: EventId, fields: &[Field<'_>]) {
        log(
            self.cfg.sink,
            &Event {
                level,
                id,
                message: event_message(id),
                fields,
            },
        );
    }

    fn audit(&self, id: EventId, level: Level, name: &str, reason: &str) {
        self.emit(
            level,
            id,
            &[
                Field {
                    key: "service",
                    value: tairix_log::FieldValue::Str(name),
                },
                Field {
                    key: "reason",
                    value: tairix_log::FieldValue::Str(reason),
                },
            ],
        );
    }

    fn audit_ready(&self, name: &str) {
        self.emit(
            Level::Info,
            events::SERVICE_READY,
            &[Field {
                key: "service",
                value: tairix_log::FieldValue::Str(name),
            }],
        );
    }

    fn audit_condition(&self, condition: ReadyCondition) {
        self.emit(
            Level::Info,
            events::CONDITION_SATISFIED,
            &[Field {
                key: "condition",
                value: tairix_log::FieldValue::Str(condition.as_str()),
            }],
        );
    }

    fn audit_started(&self, name: &str, pid: Pid) {
        let mut pid_buf = DecBuf::new();
        self.emit(
            Level::Info,
            events::SERVICE_STARTED,
            &[
                Field {
                    key: "service",
                    value: tairix_log::FieldValue::Str(name),
                },
                Field {
                    key: "pid",
                    value: tairix_log::FieldValue::Str(pid_buf.format(i128::from(pid.as_u64()))),
                },
            ],
        );
    }

    fn audit_exit(&self, name: &str, child: ReapedChild) {
        let mut pid_buf = DecBuf::new();
        let mut code_buf = DecBuf::new();
        self.emit(
            Level::Info,
            events::SERVICE_EXITED,
            &[
                Field {
                    key: "service",
                    value: tairix_log::FieldValue::Str(name),
                },
                Field {
                    key: "pid",
                    value: tairix_log::FieldValue::Str(
                        pid_buf.format(i128::from(child.pid.as_u64())),
                    ),
                },
                Field {
                    key: "exit_code",
                    value: tairix_log::FieldValue::Str(
                        code_buf.format(i128::from(child.exit_code)),
                    ),
                },
            ],
        );
    }

    fn audit_orphan(&self, child: ReapedChild) {
        let mut pid_buf = DecBuf::new();
        let mut code_buf = DecBuf::new();
        self.emit(
            Level::Info,
            events::ORPHAN_REAPED,
            &[
                Field {
                    key: "pid",
                    value: tairix_log::FieldValue::Str(
                        pid_buf.format(i128::from(child.pid.as_u64())),
                    ),
                },
                Field {
                    key: "exit_code",
                    value: tairix_log::FieldValue::Str(
                        code_buf.format(i128::from(child.exit_code)),
                    ),
                },
            ],
        );
    }

    fn audit_graph_rejected(&self, err: InitError) {
        let reason = match err {
            InitError::DuplicateService => "duplicate service",
            InitError::DependencyMissing => "dependency missing",
            InitError::DependencyCycle => "dependency cycle",
            InitError::ScopeViolation => "scope violation",
        };
        self.emit(
            Level::Error,
            events::GRAPH_REJECTED,
            &[Field {
                key: "reason",
                value: tairix_log::FieldValue::Str(reason),
            }],
        );
    }
}

fn event_message(id: EventId) -> &'static str {
    match id {
        events::SERVICE_STARTED => "service started",
        events::SERVICE_START_FAILED => "service failed to start",
        events::SERVICE_SKIPPED => "service skipped: dependency failed",
        events::SERVICE_EXITED => "service exited",
        events::ORPHAN_REAPED => "orphan reaped",
        events::GRAPH_REJECTED => "service graph rejected",
        events::SERVICE_READY => "service ready",
        events::CONDITION_SATISFIED => "readiness condition satisfied",
        events::NOTIFY_REJECTED => "readiness notice rejected",
        events::SERVICE_NOT_ENROLLED => "service not enrolled: skipped",
        events::SERVICE_ACTIVATED => "service activated on demand",
        events::ACTIVATION_QUEUED => "activation queued: awaiting readiness",
        events::ACTIVATION_DENIED => "activation denied",
        events::SERVICE_LINGER_ARMED => "idle linger armed",
        events::SERVICE_STOPPING => "service stopping",
        events::SERVICE_FORCE_TERMINATED => "service force-terminated",
        events::SERVICE_RESTART_SCHEDULED => "service restart scheduled",
        events::SERVICE_RESTART_EXHAUSTED => "service restart budget spent",
        events::SERVICE_SCOPE_REJECTED => "service account outside manager scope",
        events::SERVICE_WATCHDOG_ARMED => "liveness watchdog armed",
        events::SERVICE_WATCHDOG_TIMEOUT => "liveness watchdog elapsed: process wedged",
        _ => "init event",
    }
}

/// Add two spans, saturating the seconds at [`i64::MAX`] and keeping the
/// nanosecond field canonical.
///
/// Used to turn "now plus the linger/grace span" into the absolute
/// monotonic deadline the manager arms its one-shot timers against. Both
/// operands' sub-second fields are already in `0..NANOS_PER_SEC`, so their
/// sum needs at most one carry into the seconds.
fn add_duration(a: Duration64, b: Duration64) -> Duration64 {
    let mut secs = a.secs().saturating_add(b.secs());
    let mut nanos = a.subsec_nanos() + b.subsec_nanos();
    if nanos >= NANOS_PER_SEC {
        nanos -= NANOS_PER_SEC;
        secs = secs.saturating_add(1);
    }
    // `nanos` is now below `NANOS_PER_SEC`, so this never fails; the
    // saturated-seconds fallback keeps the function total without a panic.
    Duration64::new(secs, nanos).unwrap_or_else(|_| Duration64::from_secs(secs))
}

/// The non-negative span from the earlier instant `earlier` to the current
/// instant `now`, both monotonic. A `now` before `earlier` (never expected
/// from a monotonic clock) clamps to [`Duration64::ZERO`] rather than
/// producing a negative span.
fn duration_since(now: Duration64, earlier: Duration64) -> Duration64 {
    if now <= earlier {
        return Duration64::ZERO;
    }
    let mut secs = now.secs().saturating_sub(earlier.secs());
    let now_nanos = i64::from(now.subsec_nanos());
    let earlier_nanos = i64::from(earlier.subsec_nanos());
    let mut nanos = now_nanos - earlier_nanos;
    if nanos < 0 {
        nanos += i64::from(NANOS_PER_SEC);
        secs = secs.saturating_sub(1);
    }
    // `nanos` is now in `0..NANOS_PER_SEC` and `secs >= 0` because
    // `now > earlier`; the fallback keeps the function total.
    let nanos = u32::try_from(nanos).unwrap_or(0);
    Duration64::new(secs, nanos).unwrap_or_else(|_| Duration64::from_secs(secs))
}

/// Fixed-capacity decimal formatter for an `i128`, used to render numeric
/// audit fields without an allocator. 40 bytes hold the widest `i128`
/// (39 digits plus a sign).
struct DecBuf {
    bytes: [u8; Self::CAP],
    len: usize,
}

impl DecBuf {
    const CAP: usize = 40;

    fn new() -> Self {
        Self {
            bytes: [0; Self::CAP],
            len: 0,
        }
    }

    fn format(&mut self, value: i128) -> &str {
        self.len = 0;
        let _ = write!(DecWriter(self), "{value}");
        core::str::from_utf8(&self.bytes[..self.len]).unwrap_or("?")
    }
}

struct DecWriter<'a>(&'a mut DecBuf);

impl fmt::Write for DecWriter<'_> {
    fn write_str(&mut self, s: &str) -> fmt::Result {
        let bytes = s.as_bytes();
        let end = self.0.len.checked_add(bytes.len()).ok_or(fmt::Error)?;
        if end > DecBuf::CAP {
            return Err(fmt::Error);
        }
        self.0.bytes[self.0.len..end].copy_from_slice(bytes);
        self.0.len = end;
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::{
        add_duration, event_message, ActivateError, ActivationOutcome, AuthorityScope,
        ControlError, DecBuf, Init, InitConfig, InitError, NotifyError, ServiceSpec, StartFailure,
    };
    use crate::events;
    use crate::service::{ClientId, Pid, ReapedChild, Reaper, Spawner, Stopper};
    use alloc::collections::VecDeque;
    use alloc::string::{String, ToString};
    use alloc::vec::Vec;
    use core::cell::{Cell, RefCell};
    use tairix_abi::{
        ActivationMode, CapabilityId, Duration64, Errno, LifecycleSignal, ReadinessKind,
        ReadyCondition, RestartPolicy, ServiceControlOp, ServiceControlRequest, ServiceState,
    };
    use tairix_caps::CapabilitySet;
    use tairix_log::{Event, EventId, Level, Sink};

    /// The service account uid every test service runs as. The concrete value
    /// is irrelevant to the engine (the kernel derives the grant from it and
    /// the signed bundle); it exists only so a [`ServiceSpec`] names an
    /// account like a real one does.
    const TEST_ACCOUNT: u32 = 10;

    fn cap_set(list: &[CapabilityId]) -> CapabilitySet {
        let mut set = CapabilitySet::empty();
        for cap in list {
            set.insert(*cap);
        }
        set
    }

    fn spec(name: &str, deps: &[&str]) -> ServiceSpec {
        let deps: Vec<String> = deps.iter().map(|d| (*d).to_string()).collect();
        ServiceSpec::new(
            name,
            alloc::format!("/System/Services/{name}"),
            TEST_ACCOUNT,
            deps,
        )
    }

    /// A dependency-free service that runs as a chosen service `account`, for
    /// the authority-scope boundary tests.
    fn spec_account(name: &str, account: u32) -> ServiceSpec {
        ServiceSpec::new(
            name,
            alloc::format!("/System/Services/{name}"),
            account,
            Vec::new(),
        )
    }

    /// A `notify`-readiness service: it stays `starting` until it announces
    /// readiness, so a dependent is genuinely gated on the notice.
    fn notify_spec(name: &str, deps: &[&str]) -> ServiceSpec {
        spec(name, deps).with_readiness(ReadinessKind::Notify)
    }

    /// Spawner that records each launch and can be told to fail a named
    /// service.
    struct MockSpawner {
        next: Cell<u64>,
        fail: Option<&'static str>,
        launched: RefCell<Vec<String>>,
    }
    impl MockSpawner {
        fn new() -> Self {
            Self {
                next: Cell::new(100),
                fail: None,
                launched: RefCell::new(Vec::new()),
            }
        }
        fn failing(name: &'static str) -> Self {
            Self {
                fail: Some(name),
                ..Self::new()
            }
        }
    }
    impl Spawner for MockSpawner {
        fn spawn(&self, spec: &ServiceSpec) -> Result<Pid, Errno> {
            if self.fail == Some(spec.name()) {
                return Err(Errno::NotFound);
            }
            let raw = self.next.get();
            self.next.set(raw + 1);
            self.launched.borrow_mut().push(spec.name().to_string());
            Ok(Pid::new(raw))
        }
    }

    /// Reaper that replays a fixed script of exited children.
    struct ScriptedReaper {
        queue: RefCell<VecDeque<ReapedChild>>,
    }
    impl ScriptedReaper {
        fn new(children: &[ReapedChild]) -> Self {
            Self {
                queue: RefCell::new(children.iter().copied().collect()),
            }
        }
        /// Enqueue an exit to be reported by a later [`Reaper::collect`], so
        /// a test can drive an exit at a chosen point rather than only at
        /// construction.
        fn push(&self, child: ReapedChild) {
            self.queue.borrow_mut().push_back(child);
        }
    }
    impl Reaper for ScriptedReaper {
        fn collect(&self) -> Option<ReapedChild> {
            self.queue.borrow_mut().pop_front()
        }
    }

    /// Reaper that never reports an exit.
    struct IdleReaper;
    impl Reaper for IdleReaper {
        fn collect(&self) -> Option<ReapedChild> {
            None
        }
    }

    /// A stopper that ignores every request. Used by the tests that never
    /// exercise a stop, so the default [`cfg`] can borrow a `'static` one
    /// and existing call sites stay unchanged.
    struct NoopStopper;
    impl Stopper for NoopStopper {
        fn request_stop(&self, _pid: Pid) -> Result<(), Errno> {
            Ok(())
        }
        fn force_terminate(&self, _pid: Pid) -> Result<(), Errno> {
            Ok(())
        }
    }
    static NOOP_STOPPER: NoopStopper = NoopStopper;

    /// A stopper that records the pids it was asked to stop and to force.
    struct RecordingStopper {
        requested: RefCell<Vec<Pid>>,
        forced: RefCell<Vec<Pid>>,
    }
    impl RecordingStopper {
        fn new() -> Self {
            Self {
                requested: RefCell::new(Vec::new()),
                forced: RefCell::new(Vec::new()),
            }
        }
    }
    impl Stopper for RecordingStopper {
        fn request_stop(&self, pid: Pid) -> Result<(), Errno> {
            self.requested.borrow_mut().push(pid);
            Ok(())
        }
        fn force_terminate(&self, pid: Pid) -> Result<(), Errno> {
            self.forced.borrow_mut().push(pid);
            Ok(())
        }
    }

    struct RecordingSink {
        events: RefCell<Vec<(Level, EventId)>>,
    }
    impl RecordingSink {
        fn new() -> Self {
            Self {
                events: RefCell::new(Vec::new()),
            }
        }
        fn count(&self, id: EventId) -> usize {
            self.events
                .borrow()
                .iter()
                .filter(|(_, e)| *e == id)
                .count()
        }
    }
    impl Sink for RecordingSink {
        fn write_event(&self, event: &Event<'_>) {
            self.events.borrow_mut().push((event.level, event.id));
        }
    }

    fn cfg<'a>(
        spawner: &'a MockSpawner,
        reaper: &'a dyn Reaper,
        sink: &'a RecordingSink,
    ) -> InitConfig<'a> {
        InitConfig {
            spawner,
            stopper: &NOOP_STOPPER,
            reaper,
            sink,
            scope: AuthorityScope::System,
        }
    }

    /// Like [`cfg`] but for a **per-user** manager confined to `uid`, so the
    /// authority-scope boundary tests can assert what a user's manager may
    /// and may not manage.
    fn cfg_user<'a>(
        spawner: &'a MockSpawner,
        reaper: &'a dyn Reaper,
        sink: &'a RecordingSink,
        uid: u32,
    ) -> InitConfig<'a> {
        InitConfig {
            spawner,
            stopper: &NOOP_STOPPER,
            reaper,
            sink,
            scope: AuthorityScope::User { uid },
        }
    }

    /// Like [`cfg`] but with a caller-supplied [`Stopper`], for the tests
    /// that assert a service was asked to stop or forced down.
    fn cfg_stop<'a>(
        spawner: &'a MockSpawner,
        stopper: &'a dyn Stopper,
        reaper: &'a dyn Reaper,
        sink: &'a RecordingSink,
    ) -> InitConfig<'a> {
        InitConfig {
            spawner,
            stopper,
            reaper,
            sink,
            scope: AuthorityScope::System,
        }
    }

    fn started_names(report: &super::StartReport) -> Vec<&str> {
        report.started.iter().map(|s| s.name.as_str()).collect()
    }

    #[test]
    fn starts_services_in_dependency_order() {
        let spawner = MockSpawner::new();
        let reaper = IdleReaper;
        let sink = RecordingSink::new();
        let mut init = Init::new(cfg(&spawner, &reaper, &sink));
        // Register out of order; dependencies: c->b->a.
        init.register(spec("c", &["b"])).unwrap();
        init.register(spec("a", &[])).unwrap();
        init.register(spec("b", &["a"])).unwrap();

        let report = init.start_all().unwrap();
        assert!(report.all_started());
        assert_eq!(started_names(&report), ["a", "b", "c"]);
        assert_eq!(init.running_count(), 3);
        assert_eq!(sink.count(events::SERVICE_STARTED), 3);
    }

    #[test]
    fn missing_dependency_fails_closed() {
        let spawner = MockSpawner::new();
        let reaper = IdleReaper;
        let sink = RecordingSink::new();
        let mut init = Init::new(cfg(&spawner, &reaper, &sink));
        init.register(spec("a", &["ghost"])).unwrap();

        assert_eq!(init.start_all(), Err(InitError::DependencyMissing));
        assert_eq!(init.running_count(), 0);
        assert_eq!(sink.count(events::GRAPH_REJECTED), 1);
        assert!(spawner.launched.borrow().is_empty());
    }

    #[test]
    fn dependency_cycle_fails_closed() {
        let spawner = MockSpawner::new();
        let reaper = IdleReaper;
        let sink = RecordingSink::new();
        let mut init = Init::new(cfg(&spawner, &reaper, &sink));
        init.register(spec("a", &["b"])).unwrap();
        init.register(spec("b", &["a"])).unwrap();

        assert_eq!(init.start_all(), Err(InitError::DependencyCycle));
        assert_eq!(init.running_count(), 0);
        assert_eq!(sink.count(events::GRAPH_REJECTED), 1);
    }

    #[test]
    fn duplicate_registration_is_rejected() {
        let spawner = MockSpawner::new();
        let reaper = IdleReaper;
        let sink = RecordingSink::new();
        let mut init = Init::new(cfg(&spawner, &reaper, &sink));
        init.register(spec("a", &[])).unwrap();
        assert_eq!(
            init.register(spec("a", &[])),
            Err(InitError::DuplicateService)
        );
        assert_eq!(init.registered_count(), 1);
    }

    #[test]
    fn register_enrolled_registers_only_enrolled_bundles_and_audits_skips() {
        use crate::registry::Enrolment;
        let spawner = MockSpawner::new();
        let reaper = IdleReaper;
        let sink = RecordingSink::new();
        let mut init = Init::new(cfg(&spawner, &reaper, &sink));

        // Three bundles are discovered on disk; only two are enrolled.
        let discovered = alloc::vec![
            spec("netstack", &[]),
            spec("sysinfod", &[]),
            spec("rogue", &[]),
        ];
        let enrolment = Enrolment::parse("netstack\nsysinfod\n").expect("parses");

        init.register_enrolled(discovered, &enrolment).unwrap();

        // The present-but-unenrolled `rogue` bundle is never registered:
        // presence on disk grants no eligibility. The skip is audited.
        assert_eq!(init.registered_count(), 2);
        assert_eq!(sink.count(events::SERVICE_NOT_ENROLLED), 1);

        let report = init.start_all().unwrap();
        let mut started = started_names(&report);
        started.sort_unstable();
        assert_eq!(started, ["netstack", "sysinfod"]);
        assert!(init.running_pid("rogue").is_none());
    }

    #[test]
    fn register_enrolled_with_an_empty_enrolment_registers_nothing() {
        use crate::registry::Enrolment;
        let spawner = MockSpawner::new();
        let reaper = IdleReaper;
        let sink = RecordingSink::new();
        let mut init = Init::new(cfg(&spawner, &reaper, &sink));

        // A missing or corrupt store resolves to the empty enrolment, so no
        // discovered bundle is eligible — nothing auto-starts, fail closed.
        let discovered = alloc::vec![spec("netstack", &[]), spec("sysinfod", &[])];
        init.register_enrolled(discovered, &Enrolment::empty())
            .unwrap();

        assert_eq!(init.registered_count(), 0);
        assert_eq!(sink.count(events::SERVICE_NOT_ENROLLED), 2);
        assert!(spawner.launched.borrow().is_empty());
    }

    // --- Authority-scope boundary (plans/NEW-SERVICEMANAGER.md §3.2) -------

    #[test]
    fn scope_accessor_reports_the_configured_scope() {
        let spawner = MockSpawner::new();
        let reaper = IdleReaper;
        let sink = RecordingSink::new();
        let system = Init::new(cfg(&spawner, &reaper, &sink));
        assert_eq!(system.scope(), AuthorityScope::System);

        let s2 = MockSpawner::new();
        let r2 = IdleReaper;
        let k2 = RecordingSink::new();
        let user = Init::new(cfg_user(&s2, &r2, &k2, 1000));
        assert_eq!(user.scope(), AuthorityScope::User { uid: 1000 });
    }

    #[test]
    fn system_scope_manages_any_account() {
        // The system manager holds system authority and may register a
        // service running under any account — a system service account and a
        // user account alike.
        let spawner = MockSpawner::new();
        let reaper = IdleReaper;
        let sink = RecordingSink::new();
        let mut init = Init::new(cfg(&spawner, &reaper, &sink));
        init.register(spec_account("sysinfod", 8)).unwrap();
        init.register(spec_account("a-user-service", 1000)).unwrap();
        assert_eq!(init.registered_count(), 2);
        assert_eq!(sink.count(events::SERVICE_SCOPE_REJECTED), 0);
    }

    #[test]
    fn user_scope_manages_only_its_own_user() {
        // A per-user manager confined to uid 1000 may register a service that
        // runs as uid 1000.
        let spawner = MockSpawner::new();
        let reaper = IdleReaper;
        let sink = RecordingSink::new();
        let mut init = Init::new(cfg_user(&spawner, &reaper, &sink, 1000));
        init.register(spec_account("my-service", 1000)).unwrap();
        assert_eq!(init.registered_count(), 1);
        assert_eq!(sink.count(events::SERVICE_SCOPE_REJECTED), 0);
    }

    #[test]
    fn user_scope_cannot_bring_up_a_system_service() {
        // A per-user manager must never be able to bring a system-authority
        // service to life: a spec naming a system service account is refused
        // (fail closed) before it is recorded, and the refusal is audited.
        let spawner = MockSpawner::new();
        let reaper = IdleReaper;
        let sink = RecordingSink::new();
        let mut init = Init::new(cfg_user(&spawner, &reaper, &sink, 1000));
        assert_eq!(
            init.register(spec_account("fontd", 15)),
            Err(InitError::ScopeViolation),
        );
        assert_eq!(init.registered_count(), 0);
        assert_eq!(sink.count(events::SERVICE_SCOPE_REJECTED), 1);
    }

    #[test]
    fn user_scope_cannot_touch_another_users_service() {
        // A per-user manager confined to uid 1000 cannot manage a service
        // that runs as a different user (uid 1001).
        let spawner = MockSpawner::new();
        let reaper = IdleReaper;
        let sink = RecordingSink::new();
        let mut init = Init::new(cfg_user(&spawner, &reaper, &sink, 1000));
        assert_eq!(
            init.register(spec_account("their-service", 1001)),
            Err(InitError::ScopeViolation),
        );
        assert_eq!(init.registered_count(), 0);
        assert_eq!(sink.count(events::SERVICE_SCOPE_REJECTED), 1);
    }

    #[test]
    fn register_enrolled_fails_closed_on_out_of_scope_account() {
        // Even a positively-enrolled bundle whose account is outside the
        // per-user scope is refused: enrolment records a decision but can
        // never raise a service above the manager's own authority. The whole
        // bring-up fails closed rather than booting a surprising service.
        use crate::registry::Enrolment;
        let spawner = MockSpawner::new();
        let reaper = IdleReaper;
        let sink = RecordingSink::new();
        let mut init = Init::new(cfg_user(&spawner, &reaper, &sink, 1000));
        let enrolment = Enrolment::parse("privileged\n").expect("parses");
        let discovered = alloc::vec![spec_account("privileged", 0)];
        assert_eq!(
            init.register_enrolled(discovered, &enrolment),
            Err(InitError::ScopeViolation),
        );
        assert_eq!(init.registered_count(), 0);
        assert_eq!(sink.count(events::SERVICE_SCOPE_REJECTED), 1);
        assert!(spawner.launched.borrow().is_empty());
    }

    #[test]
    fn spawn_failure_skips_transitive_dependents() {
        let spawner = MockSpawner::failing("b");
        let reaper = IdleReaper;
        let sink = RecordingSink::new();
        let mut init = Init::new(cfg(&spawner, &reaper, &sink));
        init.register(spec("a", &[])).unwrap();
        init.register(spec("b", &[])).unwrap();
        init.register(spec("c", &["b"])).unwrap();
        init.register(spec("d", &["c"])).unwrap();

        let report = init.start_all().unwrap();
        assert_eq!(started_names(&report), ["a"]);
        // b failed to spawn; c and d are skipped because they depend on it.
        let failed: Vec<(&str, StartFailure)> = report
            .failed
            .iter()
            .map(|f| (f.name.as_str(), f.failure))
            .collect();
        assert_eq!(
            failed,
            [
                ("b", StartFailure::SpawnFailed(Errno::NotFound)),
                ("c", StartFailure::DependencyFailed),
                ("d", StartFailure::DependencyFailed),
            ]
        );
        assert_eq!(init.running_count(), 1);
        assert_eq!(sink.count(events::SERVICE_START_FAILED), 1);
        assert_eq!(sink.count(events::SERVICE_SKIPPED), 2);
    }

    #[test]
    fn reap_distinguishes_service_exit_from_orphan() {
        let spawner = MockSpawner::new();
        let sink = RecordingSink::new();
        // Start one service so we know its pid (the spawner starts at 100).
        let reaper = ScriptedReaper::new(&[
            ReapedChild {
                pid: Pid::new(100),
                exit_code: 0,
            },
            ReapedChild {
                pid: Pid::new(9999),
                exit_code: 7,
            },
        ]);
        let mut init = Init::new(cfg(&spawner, &reaper, &sink));
        init.register(spec("svc", &[])).unwrap();
        init.start_all().unwrap();
        assert_eq!(init.running_pid("svc"), Some(Pid::new(100)));

        let reaped_count = init.reap(Duration64::ZERO);
        assert_eq!(reaped_count, 2);
        assert_eq!(init.running_count(), 0);
        assert_eq!(init.running_pid("svc"), None);
        assert_eq!(sink.count(events::SERVICE_EXITED), 1);
        assert_eq!(sink.count(events::ORPHAN_REAPED), 1);
    }

    #[test]
    fn dec_buf_formats_and_event_message_total() {
        let mut buf = DecBuf::new();
        assert_eq!(buf.format(0), "0");
        assert_eq!(buf.format(-42), "-42");
        assert_eq!(buf.format(i128::from(u64::MAX)), "18446744073709551615");
        // Every emitted id has a dedicated message; unknown ids fall back.
        assert_eq!(event_message(events::SERVICE_STARTED), "service started");
        assert_eq!(event_message(EventId(1)), "init event");
    }

    #[test]
    fn notify_dependency_gates_dependent_until_ready() {
        let spawner = MockSpawner::new();
        let reaper = IdleReaper;
        let sink = RecordingSink::new();
        let mut init = Init::new(cfg(&spawner, &reaper, &sink));
        // `a` is a notify service; `b` (immediate) depends on it.
        init.register(notify_spec("a", &[])).unwrap();
        init.register(spec("b", &["a"])).unwrap();

        // The initial pass spawns `a` (now `starting`) but must NOT start
        // `b`: a spawned-but-not-ready dependency does not release it.
        let report = init.start_all().unwrap();
        assert_eq!(started_names(&report), ["a"]);
        assert_eq!(init.state_of("a"), Some(ServiceState::Starting));
        assert_eq!(init.state_of("b"), Some(ServiceState::Inactive));
        assert!(init.running_pid("a").is_some());
        assert_eq!(init.running_pid("b"), None);
        assert_eq!(sink.count(events::SERVICE_READY), 0);

        // Once `a` announces readiness, `b` is admitted and both are running.
        let report = init.notify("a", LifecycleSignal::Ready).unwrap();
        assert_eq!(started_names(&report), ["b"]);
        assert_eq!(init.state_of("a"), Some(ServiceState::Running));
        assert_eq!(init.state_of("b"), Some(ServiceState::Running));
        assert_eq!(init.running_count(), 2);
        // One readiness edge for `a`, one for the immediate `b`.
        assert_eq!(sink.count(events::SERVICE_READY), 2);
    }

    #[test]
    fn never_ready_notify_dependency_leaves_dependent_inactive() {
        let spawner = MockSpawner::new();
        let reaper = IdleReaper;
        let sink = RecordingSink::new();
        let mut init = Init::new(cfg(&spawner, &reaper, &sink));
        init.register(notify_spec("a", &[])).unwrap();
        init.register(spec("b", &["a"])).unwrap();

        init.start_all().unwrap();
        // With no readiness notice, `b` never comes up — fail closed, no
        // guessing that a spawned dependency is good enough.
        assert_eq!(init.state_of("a"), Some(ServiceState::Starting));
        assert_eq!(init.state_of("b"), Some(ServiceState::Inactive));
        assert_eq!(init.running_pid("b"), None);
    }

    #[test]
    fn required_condition_gates_start() {
        let spawner = MockSpawner::new();
        let reaper = IdleReaper;
        let sink = RecordingSink::new();
        let mut init = Init::new(cfg(&spawner, &reaper, &sink));
        init.register(spec("client", &[]).requiring([ReadyCondition::NetworkUp]))
            .unwrap();

        // The condition is unmet, so `client` stays inactive.
        let report = init.start_all().unwrap();
        assert!(report.started.is_empty());
        assert_eq!(init.state_of("client"), Some(ServiceState::Inactive));
        assert!(!init.condition_satisfied(ReadyCondition::NetworkUp));

        // Satisfying it externally (e.g. a kernel signal) admits `client`.
        let report = init.satisfy_condition(ReadyCondition::NetworkUp);
        assert_eq!(started_names(&report), ["client"]);
        assert_eq!(init.state_of("client"), Some(ServiceState::Running));
        assert!(init.condition_satisfied(ReadyCondition::NetworkUp));
        assert_eq!(sink.count(events::CONDITION_SATISFIED), 1);
    }

    #[test]
    fn provided_condition_is_satisfied_when_provider_becomes_ready() {
        let spawner = MockSpawner::new();
        let reaper = IdleReaper;
        let sink = RecordingSink::new();
        let mut init = Init::new(cfg(&spawner, &reaper, &sink));
        // `netstack` provides network-up on readiness; `client` requires it.
        // Neither names the other — the condition decouples them.
        init.register(notify_spec("netstack", &[]).providing([ReadyCondition::NetworkUp]))
            .unwrap();
        init.register(spec("client", &[]).requiring([ReadyCondition::NetworkUp]))
            .unwrap();

        init.start_all().unwrap();
        assert_eq!(init.state_of("client"), Some(ServiceState::Inactive));
        assert!(!init.condition_satisfied(ReadyCondition::NetworkUp));

        let report = init.notify("netstack", LifecycleSignal::Ready).unwrap();
        assert_eq!(started_names(&report), ["client"]);
        assert!(init.condition_satisfied(ReadyCondition::NetworkUp));
        assert_eq!(init.state_of("client"), Some(ServiceState::Running));
    }

    #[test]
    fn explicit_failure_signal_skips_dependents() {
        let spawner = MockSpawner::new();
        let reaper = IdleReaper;
        let sink = RecordingSink::new();
        let mut init = Init::new(cfg(&spawner, &reaper, &sink));
        init.register(notify_spec("a", &[])).unwrap();
        init.register(spec("b", &["a"])).unwrap();

        init.start_all().unwrap();
        // `a` reports it could not come up; `b` must be skipped, not started.
        let report = init.notify("a", LifecycleSignal::Failed).unwrap();
        assert!(report.started.is_empty());
        assert_eq!(init.state_of("a"), Some(ServiceState::Failed));
        assert_eq!(init.state_of("b"), Some(ServiceState::Failed));
        assert_eq!(init.running_pid("a"), None);
        assert_eq!(
            report
                .failed
                .iter()
                .map(|f| f.name.as_str())
                .collect::<Vec<_>>(),
            ["b"]
        );
        assert_eq!(report.failed[0].failure, StartFailure::DependencyFailed);
        assert_eq!(sink.count(events::SERVICE_SKIPPED), 1);
    }

    #[test]
    fn notify_fails_closed_on_unknown_and_non_starting() {
        let spawner = MockSpawner::new();
        let reaper = IdleReaper;
        let sink = RecordingSink::new();
        let mut init = Init::new(cfg(&spawner, &reaper, &sink));
        init.register(spec("a", &[])).unwrap();
        init.start_all().unwrap();

        // A notice for a service the manager does not know is refused.
        assert_eq!(
            init.notify("ghost", LifecycleSignal::Ready),
            Err(NotifyError::UnknownService)
        );
        // `a` is immediate, so it is already `running`, not `starting`: a
        // readiness notice for it has no pending edge to resolve.
        assert_eq!(init.state_of("a"), Some(ServiceState::Running));
        assert_eq!(
            init.notify("a", LifecycleSignal::Ready),
            Err(NotifyError::NotStarting)
        );
        assert_eq!(sink.count(events::NOTIFY_REJECTED), 2);
    }

    // --- SVC-4: on-demand endpoint activation + idle linger -------------

    /// An immediate-readiness on-demand service with the given idle-linger.
    fn on_demand_spec(name: &str, linger_secs: i64) -> ServiceSpec {
        spec(name, &[]).with_activation(ActivationMode::on_demand(Duration64::from_secs(
            linger_secs,
        )))
    }

    /// A `notify`-readiness on-demand service: it activates on connect but
    /// stays `starting` until it announces readiness.
    fn on_demand_notify_spec(name: &str, linger_secs: i64) -> ServiceSpec {
        on_demand_spec(name, linger_secs).with_readiness(ReadinessKind::Notify)
    }

    #[test]
    fn on_demand_service_is_not_started_at_boot() {
        let spawner = MockSpawner::new();
        let reaper = IdleReaper;
        let sink = RecordingSink::new();
        let mut init = Init::new(cfg(&spawner, &reaper, &sink));
        init.register(on_demand_spec("fontd", 30)).unwrap();

        let report = init.start_all().unwrap();
        assert!(report.started.is_empty());
        assert_eq!(init.state_of("fontd"), Some(ServiceState::Inactive));
        assert!(spawner.launched.borrow().is_empty());
    }

    #[test]
    fn connect_activates_a_down_immediate_service_and_connects_now() {
        let spawner = MockSpawner::new();
        let reaper = IdleReaper;
        let sink = RecordingSink::new();
        let caps = CapabilitySet::empty();
        let mut init = Init::new(cfg(&spawner, &reaper, &sink));
        init.register(on_demand_spec("fontd", 30)).unwrap();
        init.start_all().unwrap();

        let out = init.connect("fontd", &caps, ClientId::new(1)).unwrap();
        assert_eq!(out, ActivationOutcome::Connected);
        assert_eq!(init.state_of("fontd"), Some(ServiceState::Running));
        assert_eq!(init.connected_count("fontd"), 1);
        assert_eq!(sink.count(events::SERVICE_ACTIVATED), 1);
        // Immediate readiness connects synchronously, so nothing is queued.
        assert!(init.take_ready_clients().is_empty());
        assert_eq!(init.pending_count("fontd"), 0);

        // A second client shares the already-running service.
        let out = init.connect("fontd", &caps, ClientId::new(2)).unwrap();
        assert_eq!(out, ActivationOutcome::Connected);
        assert_eq!(init.connected_count("fontd"), 2);
        assert_eq!(sink.count(events::SERVICE_ACTIVATED), 1);
    }

    #[test]
    fn connect_parks_a_notify_service_until_it_is_ready() {
        let spawner = MockSpawner::new();
        let reaper = IdleReaper;
        let sink = RecordingSink::new();
        let caps = CapabilitySet::empty();
        let mut init = Init::new(cfg(&spawner, &reaper, &sink));
        init.register(on_demand_notify_spec("fontd", 30)).unwrap();
        init.start_all().unwrap();

        let out = init.connect("fontd", &caps, ClientId::new(7)).unwrap();
        assert_eq!(out, ActivationOutcome::Queued);
        assert_eq!(init.state_of("fontd"), Some(ServiceState::Starting));
        assert_eq!(init.pending_count("fontd"), 1);
        // Not ready yet: the client is parked, not woken (no busy-poll).
        assert!(init.take_ready_clients().is_empty());
        assert_eq!(sink.count(events::ACTIVATION_QUEUED), 1);

        // The service announces readiness: the parked client is released.
        init.notify("fontd", LifecycleSignal::Ready).unwrap();
        assert_eq!(init.state_of("fontd"), Some(ServiceState::Running));
        let ready = init.take_ready_clients();
        assert_eq!(ready.len(), 1);
        assert_eq!(ready[0].service, "fontd");
        assert_eq!(ready[0].client, ClientId::new(7));
        assert_eq!(init.connected_count("fontd"), 1);
        assert_eq!(init.pending_count("fontd"), 0);
        // Drained exactly once.
        assert!(init.take_ready_clients().is_empty());
    }

    #[test]
    fn idle_service_lingers_then_gracefully_stops_and_is_forced_and_reaped() {
        let spawner = MockSpawner::new();
        let stopper = RecordingStopper::new();
        let reaper = ScriptedReaper::new(&[]);
        let sink = RecordingSink::new();
        let caps = CapabilitySet::empty();
        let mut init = Init::new(cfg_stop(&spawner, &stopper, &reaper, &sink));
        init.register(on_demand_spec("fontd", 30)).unwrap();
        init.start_all().unwrap();

        init.connect("fontd", &caps, ClientId::new(1)).unwrap();
        let pid = init.running_pid("fontd").unwrap();
        assert_eq!(init.linger_deadline("fontd"), None);

        // Last client disconnects at t=100 -> one-shot linger armed at t=130.
        let t0 = Duration64::from_secs(100);
        init.disconnect("fontd", ClientId::new(1), t0).unwrap();
        assert_eq!(init.connected_count("fontd"), 0);
        assert_eq!(
            init.linger_deadline("fontd"),
            Some(Duration64::from_secs(130))
        );
        assert_eq!(sink.count(events::SERVICE_LINGER_ARMED), 1);

        // Before the deadline the service keeps running (no busy-poll, no
        // premature stop).
        assert!(!init.expire_linger("fontd", Duration64::from_secs(129)));
        assert_eq!(init.state_of("fontd"), Some(ServiceState::Running));
        assert!(stopper.requested.borrow().is_empty());

        // At the deadline the service is asked to stop gracefully.
        assert!(init.expire_linger("fontd", Duration64::from_secs(130)));
        assert_eq!(init.state_of("fontd"), Some(ServiceState::Stopping));
        assert_eq!(stopper.requested.borrow().as_slice(), &[pid]);
        assert_eq!(sink.count(events::SERVICE_STOPPING), 1);
        // Grace deadline armed at stop + default grace (5s).
        assert_eq!(
            init.grace_deadline("fontd"),
            Some(Duration64::from_secs(135))
        );

        // The service does not exit within the grace period, so it is forced.
        assert!(!init.expire_grace("fontd", Duration64::from_secs(134)));
        assert!(stopper.forced.borrow().is_empty());
        assert!(init.expire_grace("fontd", Duration64::from_secs(135)));
        assert_eq!(stopper.forced.borrow().as_slice(), &[pid]);
        assert_eq!(sink.count(events::SERVICE_FORCE_TERMINATED), 1);

        // The forced process exits non-zero; because it was stopping, the
        // manager records a stop, not a failure.
        reaper.push(ReapedChild {
            pid,
            exit_code: 137,
        });
        assert_eq!(init.reap(Duration64::from_secs(135)), 1);
        assert_eq!(init.state_of("fontd"), Some(ServiceState::Stopped));
        assert_eq!(init.running_pid("fontd"), None);
        // A manager-initiated stop is never fought with a restart, even
        // though the exit code was non-zero.
        assert_eq!(init.restart_deadline("fontd"), None);
    }

    #[test]
    fn a_new_connect_cancels_a_pending_idle_linger() {
        let spawner = MockSpawner::new();
        let stopper = RecordingStopper::new();
        let reaper = IdleReaper;
        let sink = RecordingSink::new();
        let caps = CapabilitySet::empty();
        let mut init = Init::new(cfg_stop(&spawner, &stopper, &reaper, &sink));
        init.register(on_demand_spec("fontd", 30)).unwrap();
        init.start_all().unwrap();

        init.connect("fontd", &caps, ClientId::new(1)).unwrap();
        init.disconnect("fontd", ClientId::new(1), Duration64::from_secs(100))
            .unwrap();
        assert_eq!(
            init.linger_deadline("fontd"),
            Some(Duration64::from_secs(130))
        );

        // Fresh interest before the deadline cancels the pending stop.
        let out = init.connect("fontd", &caps, ClientId::new(2)).unwrap();
        assert_eq!(out, ActivationOutcome::Connected);
        assert_eq!(init.linger_deadline("fontd"), None);

        // The expired timer now finds the service busy and does nothing.
        assert!(!init.expire_linger("fontd", Duration64::from_secs(130)));
        assert_eq!(init.state_of("fontd"), Some(ServiceState::Running));
        assert_eq!(init.connected_count("fontd"), 1);
        assert!(stopper.requested.borrow().is_empty());
    }

    #[test]
    fn connect_checks_the_endpoint_capability_before_touching_state() {
        let spawner = MockSpawner::new();
        let reaper = IdleReaper;
        let sink = RecordingSink::new();
        let mut init = Init::new(cfg(&spawner, &reaper, &sink));
        init.register(on_demand_spec("secure", 30).with_connect_capability(CapabilityId::FS_MOUNT))
            .unwrap();
        init.start_all().unwrap();

        // A client without the required capability is refused, and the
        // service is not started (the check runs before any state change).
        assert_eq!(
            init.connect("secure", &CapabilitySet::empty(), ClientId::new(1)),
            Err(ActivateError::Denied)
        );
        assert_eq!(init.state_of("secure"), Some(ServiceState::Inactive));
        assert!(spawner.launched.borrow().is_empty());
        assert_eq!(sink.count(events::SERVICE_ACTIVATED), 0);
        assert_eq!(sink.count(events::ACTIVATION_DENIED), 1);

        // A client that holds it connects and activates the service.
        let out = init
            .connect(
                "secure",
                &cap_set(&[CapabilityId::FS_MOUNT]),
                ClientId::new(2),
            )
            .unwrap();
        assert_eq!(out, ActivationOutcome::Connected);
        assert_eq!(init.state_of("secure"), Some(ServiceState::Running));
    }

    #[test]
    fn connect_to_an_unknown_service_is_denied() {
        let spawner = MockSpawner::new();
        let reaper = IdleReaper;
        let sink = RecordingSink::new();
        let caps = CapabilitySet::empty();
        let mut init = Init::new(cfg(&spawner, &reaper, &sink));
        init.start_all().unwrap();

        assert_eq!(
            init.connect("ghost", &caps, ClientId::new(1)),
            Err(ActivateError::UnknownService)
        );
        assert_eq!(sink.count(events::ACTIVATION_DENIED), 1);
    }

    #[test]
    fn a_condition_gated_service_cannot_be_activated_until_the_condition_holds() {
        let spawner = MockSpawner::new();
        let reaper = IdleReaper;
        let sink = RecordingSink::new();
        let caps = CapabilitySet::empty();
        let mut init = Init::new(cfg(&spawner, &reaper, &sink));
        // A GUI helper that requires a display: on a headless system nothing
        // satisfies `display-present`, so a connect fails closed.
        init.register(on_demand_spec("fontd", 30).requiring([ReadyCondition::DisplayPresent]))
            .unwrap();
        init.start_all().unwrap();

        assert_eq!(
            init.connect("fontd", &caps, ClientId::new(1)),
            Err(ActivateError::Unavailable)
        );
        assert_eq!(init.state_of("fontd"), Some(ServiceState::Inactive));
        assert!(spawner.launched.borrow().is_empty());
        assert_eq!(sink.count(events::SERVICE_ACTIVATED), 0);

        // Once the display appears the same connect activates the service.
        init.satisfy_condition(ReadyCondition::DisplayPresent);
        let out = init.connect("fontd", &caps, ClientId::new(1)).unwrap();
        assert_eq!(out, ActivationOutcome::Connected);
        assert_eq!(init.state_of("fontd"), Some(ServiceState::Running));
    }

    #[test]
    fn the_pending_connection_queue_is_bounded_and_fails_closed() {
        let spawner = MockSpawner::new();
        let reaper = IdleReaper;
        let sink = RecordingSink::new();
        let caps = CapabilitySet::empty();
        let mut init = Init::new(cfg(&spawner, &reaper, &sink));
        init.register(on_demand_notify_spec("fontd", 30)).unwrap();
        init.start_all().unwrap();

        // Fill the queue exactly to the bound.
        for i in 0..super::MAX_PENDING_PER_SERVICE {
            assert_eq!(
                init.connect("fontd", &caps, ClientId::new(i as u64))
                    .unwrap(),
                ActivationOutcome::Queued
            );
        }
        assert_eq!(init.pending_count("fontd"), super::MAX_PENDING_PER_SERVICE);

        // One more is refused rather than growing the queue without limit.
        assert_eq!(
            init.connect(
                "fontd",
                &caps,
                ClientId::new(super::MAX_PENDING_PER_SERVICE as u64)
            ),
            Err(ActivateError::QueueFull)
        );
        assert_eq!(init.pending_count("fontd"), super::MAX_PENDING_PER_SERVICE);
        assert_eq!(sink.count(events::ACTIVATION_DENIED), 1);
    }

    #[test]
    fn add_duration_carries_nanos_and_saturates_seconds() {
        let a = Duration64::new(1, 800_000_000).unwrap();
        let b = Duration64::new(2, 500_000_000).unwrap();
        let sum = add_duration(a, b);
        assert_eq!(sum.secs(), 4);
        assert_eq!(sum.subsec_nanos(), 300_000_000);

        let saturated = add_duration(Duration64::from_secs(i64::MAX), Duration64::from_secs(10));
        assert_eq!(saturated.secs(), i64::MAX);
    }

    // --- SVC-7: restart policy + reverse-dependency stop/shutdown -------

    /// A permanent, immediate service with the given restart policy.
    fn restart_spec(name: &str, policy: RestartPolicy) -> ServiceSpec {
        spec(name, &[]).with_restart(policy)
    }

    #[test]
    fn restart_backoff_doubles_from_the_base_and_clamps_to_the_cap() {
        // 100 ms base, doubling: 100, 200, 400, 800 ms, then clamped at the
        // 30 s cap once the doubling would exceed it.
        assert_eq!(
            super::restart_backoff(0),
            Duration64::new(0, 100_000_000).unwrap()
        );
        assert_eq!(
            super::restart_backoff(1),
            Duration64::new(0, 200_000_000).unwrap()
        );
        assert_eq!(
            super::restart_backoff(3),
            Duration64::new(0, 800_000_000).unwrap()
        );
        // A large attempt saturates the shift but is clamped to the cap,
        // never overflows.
        assert_eq!(super::restart_backoff(1_000), super::RESTART_BACKOFF_CAP);
    }

    #[test]
    fn duration_since_is_the_non_negative_gap_and_clamps_a_backwards_clock() {
        let earlier = Duration64::new(10, 250_000_000).unwrap();
        let later = Duration64::new(12, 750_000_000).unwrap();
        assert_eq!(
            super::duration_since(later, earlier),
            Duration64::new(2, 500_000_000).unwrap()
        );
        // A borrow across the second boundary carries correctly.
        let a = Duration64::new(10, 100_000_000).unwrap();
        let b = Duration64::new(11, 900_000_000).unwrap();
        assert_eq!(
            super::duration_since(b, a),
            Duration64::new(1, 800_000_000).unwrap()
        );
        // now <= earlier clamps to zero rather than a negative span.
        assert_eq!(super::duration_since(earlier, later), Duration64::ZERO);
        assert_eq!(super::duration_since(earlier, earlier), Duration64::ZERO);
    }

    #[test]
    fn never_policy_leaves_a_crashed_service_down() {
        let spawner = MockSpawner::new();
        let reaper = ScriptedReaper::new(&[]);
        let sink = RecordingSink::new();
        let mut init = Init::new(cfg(&spawner, &reaper, &sink));
        init.register(restart_spec("svc", RestartPolicy::Never))
            .unwrap();
        init.start_all().unwrap();
        let pid = init.running_pid("svc").unwrap();

        reaper.push(ReapedChild { pid, exit_code: 1 });
        init.reap(Duration64::from_secs(10));
        // Never: no restart is scheduled and the service stays failed.
        assert_eq!(init.state_of("svc"), Some(ServiceState::Failed));
        assert_eq!(init.restart_deadline("svc"), None);
        assert_eq!(init.running_count(), 0);
        assert_eq!(sink.count(events::SERVICE_RESTART_SCHEDULED), 0);
    }

    #[test]
    fn on_failure_restarts_after_an_abnormal_exit_but_not_a_clean_one() {
        // Abnormal exit: scheduled, then relaunched at its backoff deadline.
        let spawner = MockSpawner::new();
        let reaper = ScriptedReaper::new(&[]);
        let sink = RecordingSink::new();
        let mut init = Init::new(cfg(&spawner, &reaper, &sink));
        init.register(restart_spec("svc", RestartPolicy::OnFailure))
            .unwrap();
        init.start_all().unwrap();
        let pid = init.running_pid("svc").unwrap();

        reaper.push(ReapedChild { pid, exit_code: 1 });
        init.reap(Duration64::from_secs(10));
        assert_eq!(init.state_of("svc"), Some(ServiceState::Failed));
        // First relaunch waits the base backoff (100 ms) from the exit.
        assert_eq!(
            init.restart_deadline("svc"),
            Some(Duration64::new(10, 100_000_000).unwrap())
        );
        assert_eq!(sink.count(events::SERVICE_RESTART_SCHEDULED), 1);

        // Before the deadline the relaunch is a no-op (no busy-poll).
        let early = init.expire_restart_backoff("svc", Duration64::from_secs(10));
        assert!(early.started.is_empty());
        assert_eq!(init.state_of("svc"), Some(ServiceState::Failed));

        // At the deadline the service is relaunched with a fresh pid.
        let report = init.expire_restart_backoff("svc", Duration64::new(10, 100_000_000).unwrap());
        assert_eq!(started_names(&report), ["svc"]);
        assert_eq!(init.state_of("svc"), Some(ServiceState::Running));
        assert_ne!(init.running_pid("svc"), Some(pid));
        assert_eq!(init.restart_deadline("svc"), None);

        // A clean exit under on-failure is honoured: no restart.
        let pid2 = init.running_pid("svc").unwrap();
        reaper.push(ReapedChild {
            pid: pid2,
            exit_code: 0,
        });
        init.reap(Duration64::from_secs(40));
        assert_eq!(init.state_of("svc"), Some(ServiceState::Stopped));
        assert_eq!(init.restart_deadline("svc"), None);
    }

    #[test]
    fn always_policy_restarts_even_after_a_clean_exit() {
        let spawner = MockSpawner::new();
        let reaper = ScriptedReaper::new(&[]);
        let sink = RecordingSink::new();
        let mut init = Init::new(cfg(&spawner, &reaper, &sink));
        init.register(restart_spec("svc", RestartPolicy::Always))
            .unwrap();
        init.start_all().unwrap();
        let pid = init.running_pid("svc").unwrap();

        // Clean exit, but `always` brings it back.
        reaper.push(ReapedChild { pid, exit_code: 0 });
        init.reap(Duration64::from_secs(5));
        assert_eq!(init.state_of("svc"), Some(ServiceState::Stopped));
        assert_eq!(
            init.restart_deadline("svc"),
            Some(Duration64::new(5, 100_000_000).unwrap())
        );
        let report = init.expire_restart_backoff("svc", Duration64::new(5, 100_000_000).unwrap());
        assert_eq!(started_names(&report), ["svc"]);
        assert_eq!(init.state_of("svc"), Some(ServiceState::Running));
    }

    #[test]
    fn a_crash_loop_is_bounded_by_the_restart_budget() {
        let spawner = MockSpawner::new();
        let reaper = ScriptedReaper::new(&[]);
        let sink = RecordingSink::new();
        let mut init = Init::new(cfg(&spawner, &reaper, &sink));
        init.register(restart_spec("svc", RestartPolicy::Always))
            .unwrap();
        init.start_all().unwrap();

        // Drive `MAX_RESTART_ATTEMPTS` rapid crash/relaunch cycles: each
        // crash lands well inside the stable window of the previous
        // relaunch, so the budget is never reset.
        let mut now = Duration64::from_secs(1);
        for _ in 0..super::MAX_RESTART_ATTEMPTS {
            let pid = init.running_pid("svc").unwrap();
            reaper.push(ReapedChild { pid, exit_code: 1 });
            init.reap(now);
            let deadline = init.restart_deadline("svc").expect("restart scheduled");
            let report = init.expire_restart_backoff("svc", deadline);
            assert_eq!(started_names(&report), ["svc"]);
            now = add_duration(deadline, Duration64::new(0, 500_000_000).unwrap());
        }

        // One more crash inside the window exhausts the budget: the service
        // is left down rather than relaunched forever (fail closed).
        let pid = init.running_pid("svc").unwrap();
        reaper.push(ReapedChild { pid, exit_code: 1 });
        init.reap(now);
        assert_eq!(init.restart_deadline("svc"), None);
        assert_eq!(init.state_of("svc"), Some(ServiceState::Failed));
        assert_eq!(init.running_count(), 0);
        assert_eq!(
            sink.count(events::SERVICE_RESTART_SCHEDULED),
            super::MAX_RESTART_ATTEMPTS as usize
        );
        assert_eq!(sink.count(events::SERVICE_RESTART_EXHAUSTED), 1);
    }

    #[test]
    fn a_service_that_ran_stably_resets_its_restart_budget() {
        let spawner = MockSpawner::new();
        let reaper = ScriptedReaper::new(&[]);
        let sink = RecordingSink::new();
        let mut init = Init::new(cfg(&spawner, &reaper, &sink));
        init.register(restart_spec("svc", RestartPolicy::Always))
            .unwrap();
        init.start_all().unwrap();

        // First crash: relaunch at base backoff.
        let pid = init.running_pid("svc").unwrap();
        reaper.push(ReapedChild { pid, exit_code: 1 });
        init.reap(Duration64::from_secs(1));
        let deadline = init.restart_deadline("svc").unwrap();
        assert_eq!(deadline, Duration64::new(1, 100_000_000).unwrap());
        init.expire_restart_backoff("svc", deadline);

        // Second crash long after the relaunch (past the stable window):
        // the budget resets, so the backoff is the base again, not doubled.
        let pid = init.running_pid("svc").unwrap();
        let much_later = add_duration(deadline, super::RESTART_STABLE_WINDOW);
        let much_later = add_duration(much_later, Duration64::from_secs(1));
        reaper.push(ReapedChild { pid, exit_code: 1 });
        init.reap(much_later);
        assert_eq!(
            init.restart_deadline("svc"),
            Some(add_duration(
                much_later,
                Duration64::new(0, 100_000_000).unwrap()
            ))
        );
    }

    // --- SVC-8: liveness watchdog + restart (plans/WATCHDOG.md) ---------

    /// A permanent, immediate service that opts into the liveness watchdog
    /// with a 5 s interval and the given restart policy.
    fn watchdog_spec(name: &str, policy: RestartPolicy) -> ServiceSpec {
        spec(name, &[])
            .with_restart(policy)
            .with_watchdog(Duration64::from_secs(5))
    }

    #[test]
    fn a_running_service_arms_and_renews_its_liveness_watchdog() {
        let spawner = MockSpawner::new();
        let reaper = IdleReaper;
        let sink = RecordingSink::new();
        let mut init = Init::new(cfg(&spawner, &reaper, &sink));
        init.register(watchdog_spec("svc", RestartPolicy::Always))
            .unwrap();
        // A service that opts out of the watchdog is never armed.
        init.register(spec("plain", &[])).unwrap();
        init.start_all().unwrap();

        // Nothing is armed until the transport arms the watchdogs.
        assert_eq!(init.watchdog_deadline("svc"), None);
        init.arm_watchdogs(Duration64::from_secs(100));
        assert_eq!(
            init.watchdog_deadline("svc"),
            Some(Duration64::from_secs(105))
        );
        assert_eq!(init.watchdog_deadline("plain"), None);
        assert_eq!(sink.count(events::SERVICE_WATCHDOG_ARMED), 1);

        // Arming is idempotent: a service already watched keeps its live
        // countdown and is not re-armed or re-logged.
        init.arm_watchdogs(Duration64::from_secs(200));
        assert_eq!(
            init.watchdog_deadline("svc"),
            Some(Duration64::from_secs(105))
        );
        assert_eq!(sink.count(events::SERVICE_WATCHDOG_ARMED), 1);

        // A heartbeat pushes the deadline forward; it is not audited.
        assert!(init.heartbeat("svc", Duration64::from_secs(103)));
        assert_eq!(
            init.watchdog_deadline("svc"),
            Some(Duration64::from_secs(108))
        );
        // A watchdog-less or unknown service rejects a heartbeat (fail safe),
        // never arming a countdown or leaking existence.
        assert!(!init.heartbeat("plain", Duration64::from_secs(103)));
        assert!(!init.heartbeat("ghost", Duration64::from_secs(103)));
        assert_eq!(init.watchdog_deadline("plain"), None);
    }

    #[test]
    fn a_wedged_service_is_force_killed_and_restarted() {
        let spawner = MockSpawner::new();
        let stopper = RecordingStopper::new();
        let reaper = ScriptedReaper::new(&[]);
        let sink = RecordingSink::new();
        let mut init = Init::new(cfg_stop(&spawner, &stopper, &reaper, &sink));
        init.register(watchdog_spec("svc", RestartPolicy::OnFailure))
            .unwrap();
        init.start_all().unwrap();
        let pid = init.running_pid("svc").unwrap();
        init.arm_watchdogs(Duration64::from_secs(10));
        let deadline = init.watchdog_deadline("svc").unwrap();
        assert_eq!(deadline, Duration64::from_secs(15));

        // Before the deadline the watchdog is a no-op (event-timed, never a
        // busy-poll): the process is left alone.
        assert!(!init.expire_watchdog("svc", Duration64::from_secs(14)));
        assert!(stopper.forced.borrow().is_empty());

        // A heartbeat before the deadline pushes it forward, so the stale
        // one-shot no longer fires — a healthy, progressing service is never
        // killed.
        assert!(init.heartbeat("svc", Duration64::from_secs(14)));
        assert!(!init.expire_watchdog("svc", deadline));
        assert!(stopper.forced.borrow().is_empty());
        let renewed = init.watchdog_deadline("svc").unwrap();
        assert_eq!(renewed, Duration64::from_secs(19));

        // The renewed deadline elapses with no further heartbeat: the process
        // has wedged, so it is force-terminated and audited, but not yet
        // reaped — the state stays running until the reaper observes the exit.
        assert!(init.expire_watchdog("svc", renewed));
        assert_eq!(stopper.forced.borrow().as_slice(), &[pid]);
        assert_eq!(sink.count(events::SERVICE_WATCHDOG_TIMEOUT), 1);
        assert_eq!(init.watchdog_deadline("svc"), None);
        assert_eq!(init.state_of("svc"), Some(ServiceState::Running));

        // Reaping the forced exit classifies it as an abnormal failure even
        // though the killed process reports a zero exit code, and the
        // `on-failure` policy schedules a relaunch.
        reaper.push(ReapedChild { pid, exit_code: 0 });
        init.reap(Duration64::from_secs(20));
        assert_eq!(init.state_of("svc"), Some(ServiceState::Failed));
        assert_eq!(
            init.restart_deadline("svc"),
            Some(Duration64::new(20, 100_000_000).unwrap())
        );
        assert_eq!(sink.count(events::SERVICE_RESTART_SCHEDULED), 1);
    }

    #[test]
    fn a_wedged_never_policy_service_is_killed_but_left_down() {
        let spawner = MockSpawner::new();
        let stopper = RecordingStopper::new();
        let reaper = ScriptedReaper::new(&[]);
        let sink = RecordingSink::new();
        let mut init = Init::new(cfg_stop(&spawner, &stopper, &reaper, &sink));
        init.register(watchdog_spec("svc", RestartPolicy::Never))
            .unwrap();
        init.start_all().unwrap();
        let pid = init.running_pid("svc").unwrap();
        init.arm_watchdogs(Duration64::from_secs(0));

        assert!(init.expire_watchdog("svc", Duration64::from_secs(5)));
        assert_eq!(stopper.forced.borrow().as_slice(), &[pid]);
        reaper.push(ReapedChild { pid, exit_code: 0 });
        init.reap(Duration64::from_secs(6));
        // A wedge is a failure, so it is not "stopped"; but `never` leaves it
        // down rather than relaunching it (loud, not silent).
        assert_eq!(init.state_of("svc"), Some(ServiceState::Failed));
        assert_eq!(init.restart_deadline("svc"), None);
        assert_eq!(sink.count(events::SERVICE_RESTART_SCHEDULED), 0);
    }

    #[test]
    fn a_deliberate_stop_disarms_the_liveness_watchdog() {
        let spawner = MockSpawner::new();
        let stopper = RecordingStopper::new();
        let reaper = ScriptedReaper::new(&[]);
        let sink = RecordingSink::new();
        let mut init = Init::new(cfg_stop(&spawner, &stopper, &reaper, &sink));
        init.register(watchdog_spec("svc", RestartPolicy::Always))
            .unwrap();
        init.start_all().unwrap();
        init.arm_watchdogs(Duration64::from_secs(1));
        assert!(init.watchdog_deadline("svc").is_some());

        // A stop the manager asked for disarms the watchdog: the graceful
        // teardown must never be second-guessed and relaunched as a wedge.
        init.stop("svc", Duration64::from_secs(2)).unwrap();
        assert_eq!(init.watchdog_deadline("svc"), None);
        assert!(!init.expire_watchdog("svc", Duration64::from_secs(100)));

        // The graceful exit is a clean stop, never a watchdog failure.
        let pid = init.running_pid("svc").unwrap();
        reaper.push(ReapedChild { pid, exit_code: 0 });
        init.reap(Duration64::from_secs(3));
        assert_eq!(init.state_of("svc"), Some(ServiceState::Stopped));
        assert_eq!(init.restart_deadline("svc"), None);
        assert_eq!(sink.count(events::SERVICE_WATCHDOG_TIMEOUT), 0);
    }

    #[test]
    fn shutdown_stops_every_service_in_reverse_dependency_order() {
        let spawner = MockSpawner::new();
        let stopper = RecordingStopper::new();
        let reaper = IdleReaper;
        let sink = RecordingSink::new();
        let mut init = Init::new(cfg_stop(&spawner, &stopper, &reaper, &sink));
        // c depends on b depends on a.
        init.register(spec("a", &[])).unwrap();
        init.register(spec("b", &["a"])).unwrap();
        init.register(spec("c", &["b"])).unwrap();
        init.start_all().unwrap();
        let pa = init.running_pid("a").unwrap();
        let pb = init.running_pid("b").unwrap();
        let pc = init.running_pid("c").unwrap();

        init.shutdown(Duration64::from_secs(100));

        // Dependents are asked to stop before the services they depend on.
        assert_eq!(stopper.requested.borrow().as_slice(), &[pc, pb, pa]);
        for name in ["a", "b", "c"] {
            assert_eq!(init.state_of(name), Some(ServiceState::Stopping));
            assert!(init.grace_deadline(name).is_some());
        }
        assert_eq!(sink.count(events::SERVICE_STOPPING), 3);
    }

    #[test]
    fn stop_tears_down_a_service_and_its_dependents_only() {
        let spawner = MockSpawner::new();
        let stopper = RecordingStopper::new();
        let reaper = IdleReaper;
        let sink = RecordingSink::new();
        let mut init = Init::new(cfg_stop(&spawner, &stopper, &reaper, &sink));
        // c -> b -> a; d is independent.
        init.register(spec("a", &[])).unwrap();
        init.register(spec("b", &["a"])).unwrap();
        init.register(spec("c", &["b"])).unwrap();
        init.register(spec("d", &[])).unwrap();
        init.start_all().unwrap();
        let pa = init.running_pid("a").unwrap();
        let pb = init.running_pid("b").unwrap();
        let pc = init.running_pid("c").unwrap();

        // Stopping `a` must take `b` and `c` (its dependents) down first,
        // dependents-first, and leave the independent `d` running.
        init.stop("a", Duration64::from_secs(100)).unwrap();
        assert_eq!(stopper.requested.borrow().as_slice(), &[pc, pb, pa]);
        for name in ["a", "b", "c"] {
            assert_eq!(init.state_of(name), Some(ServiceState::Stopping));
        }
        assert_eq!(init.state_of("d"), Some(ServiceState::Running));
    }

    #[test]
    fn stop_an_unknown_service_fails_closed() {
        let spawner = MockSpawner::new();
        let reaper = IdleReaper;
        let sink = RecordingSink::new();
        let mut init = Init::new(cfg(&spawner, &reaper, &sink));
        init.start_all().unwrap();
        assert_eq!(
            init.stop("ghost", Duration64::from_secs(1)),
            Err(ActivateError::UnknownService)
        );
        assert_eq!(sink.count(events::ACTIVATION_DENIED), 1);
    }

    #[test]
    fn shutdown_cancels_a_pending_restart() {
        let spawner = MockSpawner::new();
        let stopper = RecordingStopper::new();
        let reaper = ScriptedReaper::new(&[]);
        let sink = RecordingSink::new();
        let mut init = Init::new(cfg_stop(&spawner, &stopper, &reaper, &sink));
        init.register(restart_spec("svc", RestartPolicy::Always))
            .unwrap();
        init.start_all().unwrap();
        let pid = init.running_pid("svc").unwrap();

        // A crash arms a pending restart.
        reaper.push(ReapedChild { pid, exit_code: 1 });
        init.reap(Duration64::from_secs(5));
        assert!(init.restart_deadline("svc").is_some());

        // Shutdown supersedes it: the queued relaunch is cancelled so the
        // manager never brings a service back while shutting it down.
        init.shutdown(Duration64::from_secs(6));
        assert_eq!(init.restart_deadline("svc"), None);
    }

    // --- SVC-8: capability-gated control surface ------------------------

    /// A control `start` request naming `name`.
    fn start_req(name: &str) -> ServiceControlRequest<'_> {
        ServiceControlRequest {
            op: ServiceControlOp::Start,
            name,
        }
    }

    /// A control `stop` request naming `name`.
    fn stop_req(name: &str) -> ServiceControlRequest<'_> {
        ServiceControlRequest {
            op: ServiceControlOp::Stop,
            name,
        }
    }

    #[test]
    fn control_start_brings_up_a_down_service() {
        let spawner = MockSpawner::new();
        let reaper = IdleReaper;
        let sink = RecordingSink::new();
        let mut init = Init::new(cfg(&spawner, &reaper, &sink));
        init.register(spec("svc", &[])).unwrap();
        // Registered but never admitted: it rests inactive until asked.
        assert_eq!(init.state_of("svc"), Some(ServiceState::Inactive));

        let state = init
            .control(start_req("svc"), Duration64::from_secs(1))
            .unwrap();
        assert_eq!(state, ServiceState::Running);
        assert_eq!(init.state_of("svc"), Some(ServiceState::Running));
        assert_eq!(init.running_count(), 1);
        assert_eq!(sink.count(events::SERVICE_CONTROL_STARTED), 1);
    }

    #[test]
    fn control_start_is_idempotent_for_an_up_service() {
        let spawner = MockSpawner::new();
        let reaper = IdleReaper;
        let sink = RecordingSink::new();
        let mut init = Init::new(cfg(&spawner, &reaper, &sink));
        init.register(spec("svc", &[])).unwrap();
        init.control(start_req("svc"), Duration64::from_secs(1))
            .unwrap();
        let pid = init.running_pid("svc").unwrap();

        // A second start finds it already up: it returns the state and does
        // not respawn (no second launch recorded, same live pid).
        let state = init
            .control(start_req("svc"), Duration64::from_secs(2))
            .unwrap();
        assert_eq!(state, ServiceState::Running);
        assert_eq!(init.running_pid("svc"), Some(pid));
        assert_eq!(spawner.launched.borrow().len(), 1);
    }

    #[test]
    fn control_start_of_an_unknown_or_invalid_name_fails_closed() {
        let spawner = MockSpawner::new();
        let reaper = IdleReaper;
        let sink = RecordingSink::new();
        let mut init = Init::new(cfg(&spawner, &reaper, &sink));
        init.register(spec("svc", &[])).unwrap();

        assert_eq!(
            init.control(start_req("ghost"), Duration64::from_secs(1)),
            Err(ControlError::UnknownService)
        );
        // A path-traversal-shaped name is refused by the strict name policy
        // before any lookup, so it can never match a registered service.
        assert_eq!(
            init.control(start_req("../etc"), Duration64::from_secs(1)),
            Err(ControlError::UnknownService)
        );
        assert_eq!(init.running_count(), 0);
        assert_eq!(sink.count(events::SERVICE_CONTROL_DENIED), 2);
    }

    #[test]
    fn control_start_fails_closed_when_a_required_condition_is_unmet() {
        // The headless case: a `display-present`-gated service is refused
        // while the condition is unmet, and comes up once it is satisfied —
        // never guessed into life (§17.3).
        let spawner = MockSpawner::new();
        let reaper = IdleReaper;
        let sink = RecordingSink::new();
        let mut init = Init::new(cfg(&spawner, &reaper, &sink));
        init.register(spec("gui", &[]).requiring([ReadyCondition::DisplayPresent]))
            .unwrap();

        assert_eq!(
            init.control(start_req("gui"), Duration64::from_secs(1)),
            Err(ControlError::Unavailable)
        );
        assert_eq!(init.state_of("gui"), Some(ServiceState::Inactive));
        assert_eq!(init.running_count(), 0);
        assert_eq!(sink.count(events::SERVICE_CONTROL_DENIED), 1);

        // Once the display appears the same request succeeds.
        init.satisfy_condition(ReadyCondition::DisplayPresent);
        let state = init
            .control(start_req("gui"), Duration64::from_secs(2))
            .unwrap();
        assert_eq!(state, ServiceState::Running);
        assert_eq!(sink.count(events::SERVICE_CONTROL_STARTED), 1);
    }

    #[test]
    fn control_start_reports_a_refused_spawn_as_not_startable() {
        let spawner = MockSpawner::failing("svc");
        let reaper = IdleReaper;
        let sink = RecordingSink::new();
        let mut init = Init::new(cfg(&spawner, &reaper, &sink));
        init.register(spec("svc", &[])).unwrap();

        assert_eq!(
            init.control(start_req("svc"), Duration64::from_secs(1)),
            Err(ControlError::NotStartable)
        );
        // `try_start` marked it failed and audited the spawn refusal.
        assert_eq!(init.state_of("svc"), Some(ServiceState::Failed));
        assert_eq!(sink.count(events::SERVICE_START_FAILED), 1);
    }

    #[test]
    fn control_stop_tears_down_a_running_service_and_its_dependents() {
        let spawner = MockSpawner::new();
        let stopper = RecordingStopper::new();
        let reaper = IdleReaper;
        let sink = RecordingSink::new();
        let mut init = Init::new(cfg_stop(&spawner, &stopper, &reaper, &sink));
        // b depends on a; stopping a takes b (its dependent) down first.
        init.register(spec("a", &[])).unwrap();
        init.register(spec("b", &["a"])).unwrap();
        init.start_all().unwrap();
        let pa = init.running_pid("a").unwrap();
        let pb = init.running_pid("b").unwrap();

        let state = init
            .control(stop_req("a"), Duration64::from_secs(100))
            .unwrap();
        assert_eq!(state, ServiceState::Stopping);
        assert_eq!(stopper.requested.borrow().as_slice(), &[pb, pa]);
        assert_eq!(init.state_of("a"), Some(ServiceState::Stopping));
        assert_eq!(init.state_of("b"), Some(ServiceState::Stopping));
        assert_eq!(sink.count(events::SERVICE_CONTROL_STOPPED), 1);
    }

    #[test]
    fn control_stop_of_an_unknown_service_fails_closed() {
        let spawner = MockSpawner::new();
        let stopper = RecordingStopper::new();
        let reaper = IdleReaper;
        let sink = RecordingSink::new();
        let mut init = Init::new(cfg_stop(&spawner, &stopper, &reaper, &sink));
        init.register(spec("svc", &[])).unwrap();
        init.start_all().unwrap();

        assert_eq!(
            init.control(stop_req("ghost"), Duration64::from_secs(1)),
            Err(ControlError::UnknownService)
        );
        // Nothing was asked to stop; the denial is audited on the control
        // channel and the running service is untouched.
        assert!(stopper.requested.borrow().is_empty());
        assert_eq!(init.state_of("svc"), Some(ServiceState::Running));
        assert_eq!(sink.count(events::SERVICE_CONTROL_DENIED), 1);
    }
}
