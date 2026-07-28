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
    Duration64, LifecycleSignal, ReadinessKind, ReadyCondition, ServiceState, NANOS_PER_SEC,
};
use tairix_caps::CapabilitySet;
use tairix_log::{log, Event, EventId, Field, Level, Sink};

use crate::error::{ActivateError, InitError, NotifyError, StartFailure};
use crate::events;
use crate::registry::Enrolment;
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
    /// # Errors
    ///
    /// Returns [`InitError::DuplicateService`] if a service with the same
    /// name is already registered.
    pub fn register(&mut self, spec: ServiceSpec) -> Result<(), InitError> {
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
    /// Returns [`InitError::DuplicateService`] if two enrolled bundles share
    /// a name (a packaging defect), failing closed before any is brought up.
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
    /// is force-terminated. Clears any pending idle-linger.
    fn begin_stop(&mut self, idx: usize, now: Duration64, reason: &str) {
        self.services[idx].linger_deadline = None;
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
    /// The process is gone, so its activation state is cleared: its sink and
    /// pending waiters are dropped and any armed linger or grace deadline is
    /// disarmed. Clients whose connections died with the process are the
    /// transport layer's to notice; the manager holds no stale references.
    pub fn reap(&mut self) -> usize {
        let mut reaped = 0;
        while let Some(child) = self.cfg.reaper.collect() {
            reaped += 1;
            if let Some(pos) = self.services.iter().position(|s| s.pid == Some(child.pid)) {
                let name = self.services[pos].spec.name().to_string();
                let was_stopping = self.services[pos].state == ServiceState::Stopping;
                self.services[pos].pid = None;
                self.services[pos].state = if was_stopping || child.exit_code == 0 {
                    ServiceState::Stopped
                } else {
                    ServiceState::Failed
                };
                self.services[pos].sink.clear();
                self.services[pos].waiters.clear();
                self.services[pos].linger_deadline = None;
                self.services[pos].grace_deadline = None;
                self.audit_exit(&name, child);
            } else {
                self.audit_orphan(child);
            }
        }
        reaped
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
    /// [`ServiceState::Failed`] state, so `idx` can never be admitted and is
    /// skipped.
    fn dependency_failed(&self, idx: usize) -> bool {
        self.services[idx].spec.dependencies().iter().any(|dep| {
            self.index_of(dep)
                .is_some_and(|d| self.services[d].state == ServiceState::Failed)
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
        add_duration, event_message, ActivateError, ActivationOutcome, DecBuf, Init, InitConfig,
        InitError, NotifyError, ServiceSpec, StartFailure,
    };
    use crate::events;
    use crate::service::{ClientId, Pid, ReapedChild, Reaper, Spawner, Stopper};
    use alloc::collections::VecDeque;
    use alloc::string::{String, ToString};
    use alloc::vec::Vec;
    use core::cell::{Cell, RefCell};
    use tairix_abi::{
        ActivationMode, CapabilityId, Duration64, Errno, LifecycleSignal, ReadinessKind,
        ReadyCondition, ServiceState,
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

        let reaped_count = init.reap();
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
        assert_eq!(init.reap(), 1);
        assert_eq!(init.state_of("fontd"), Some(ServiceState::Stopped));
        assert_eq!(init.running_pid("fontd"), None);
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
}
