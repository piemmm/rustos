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

use tairix_abi::{LifecycleSignal, ReadinessKind, ReadyCondition, ServiceState};
use tairix_caps::CapabilitySet;
use tairix_log::{log, Event, EventId, Field, Level, Sink};

use crate::error::{InitError, NotifyError, StartFailure};
use crate::events;
use crate::registry::Enrolment;
use crate::service::{
    decode_manifest_capabilities, Pid, ReapedChild, Reaper, ServiceSpec, Spawner,
};

/// Number of distinct named readiness conditions, sized from the closed
/// [`ReadyCondition`] set so the satisfied-conditions bitmap tracks the
/// vocabulary rather than a hand-picked constant.
const CONDITION_COUNT: usize = ReadyCondition::ALL.len();

/// Construction-time configuration for an [`Init`] instance.
///
/// All seams are borrowed for the manager's lifetime, mirroring the
/// `drvhost` host configuration: one config per PID 1 process, alive for
/// the whole run.
pub struct InitConfig<'a> {
    /// The capability set init itself was granted. Every service's grant is
    /// the intersection of its manifest request with this authority; a
    /// service may never exceed it.
    pub authority: CapabilitySet,
    /// ABI version the manager accepts in service manifests. A manifest
    /// targeting a different version is refused
    /// ([`StartFailure::ManifestInvalid`]).
    pub accepted_abi_version: u32,
    /// Seam that launches a verified service binary.
    pub spawner: &'a dyn Spawner,
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
    /// Capability set granted to the service (the manifest request
    /// intersected with the system authority).
    pub granted: CapabilitySet,
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
    /// Registration only records the service; the capability grant is still
    /// intersected with the system authority at start
    /// ([`Init::start_all`]), so enrolling a service can never widen the
    /// authority it ultimately runs with.
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
    /// its dependencies. Each service's capability grant is the
    /// intersection of its manifest request with the system authority; a
    /// service that over-requests is refused, and a service whose
    /// dependency failed is skipped — neither aborts the independent
    /// services.
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

    /// Reap every child that has exited, returning the number reaped.
    ///
    /// A reaped process that matches a started service is logged as a
    /// service exit and its lifecycle moved to a terminal state (a clean
    /// exit is [`ServiceState::Stopped`], a non-zero exit
    /// [`ServiceState::Failed`]); any other reaped process is an inherited
    /// orphan and is logged as such (PID 1 reaps the whole system's zombies).
    pub fn reap(&mut self) -> usize {
        let mut reaped = 0;
        while let Some(child) = self.cfg.reaper.collect() {
            reaped += 1;
            if let Some(pos) = self.services.iter().position(|s| s.pid == Some(child.pid)) {
                let name = self.services[pos].spec.name().to_string();
                self.services[pos].pid = None;
                self.services[pos].state = if child.exit_code == 0 {
                    ServiceState::Stopped
                } else {
                    ServiceState::Failed
                };
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
    fn pump(&mut self) -> StartReport {
        let mut report = StartReport::default();
        loop {
            let mut changed = false;
            for i in 0..self.order.len() {
                let idx = self.order[i];
                if self.services[idx].state != ServiceState::Inactive {
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

    /// Spawn service `idx`: decode and cap-check its manifest, launch it with
    /// its capability ceiling, and transition it to [`ServiceState::Starting`]
    /// with its live [`Pid`]. On any failure the service is transitioned to
    /// [`ServiceState::Failed`] and the reason recorded, so a failed service
    /// is never left resting in `Inactive` where `pump` would retry it.
    fn try_start(&mut self, idx: usize) -> Result<StartedService, FailedService> {
        let name = self.services[idx].spec.name().to_string();

        let requested = match self.requested_capabilities(idx) {
            Ok(set) => set,
            Err(failure) => {
                self.services[idx].state = ServiceState::Failed;
                self.audit(
                    events::SERVICE_START_FAILED,
                    Level::Warn,
                    &name,
                    "manifest invalid",
                );
                return Err(FailedService { name, failure });
            }
        };

        if !requested.is_subset_of(&self.cfg.authority) {
            self.services[idx].state = ServiceState::Failed;
            self.audit(
                events::SERVICE_DENIED,
                Level::Warn,
                &name,
                "capability escalation",
            );
            return Err(FailedService {
                name,
                failure: StartFailure::CapabilityEscalation,
            });
        }

        // granted == requested here (it is a subset of the authority); the
        // intersection is computed explicitly to make the grant rule
        // visible rather than implied.
        let granted = requested.intersection(&self.cfg.authority);

        let pid = match self.cfg.spawner.spawn(&self.services[idx].spec, &granted) {
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
        self.audit_started(&name, pid, &granted);
        Ok(StartedService { name, pid, granted })
    }

    /// Decode a service manifest into the capability set it requests.
    fn requested_capabilities(&self, idx: usize) -> Result<CapabilitySet, StartFailure> {
        decode_manifest_capabilities(
            self.services[idx].spec.manifest(),
            self.cfg.accepted_abi_version,
        )
        .map_err(StartFailure::ManifestInvalid)
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

    fn audit_started(&self, name: &str, pid: Pid, granted: &CapabilitySet) {
        let mut pid_buf = DecBuf::new();
        let mut cap_buf = DecBuf::new();
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
                Field {
                    key: "granted_caps",
                    value: tairix_log::FieldValue::Str(cap_buf.format(i128::from(granted.len()))),
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
        events::SERVICE_DENIED => "service denied: capability escalation",
        events::SERVICE_SKIPPED => "service skipped: dependency failed",
        events::SERVICE_EXITED => "service exited",
        events::ORPHAN_REAPED => "orphan reaped",
        events::GRAPH_REJECTED => "service graph rejected",
        events::SERVICE_READY => "service ready",
        events::CONDITION_SATISFIED => "readiness condition satisfied",
        events::NOTIFY_REJECTED => "readiness notice rejected",
        events::SERVICE_NOT_ENROLLED => "service not enrolled: skipped",
        _ => "init event",
    }
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
        event_message, DecBuf, Init, InitConfig, InitError, NotifyError, ServiceSpec, StartFailure,
    };
    use crate::events;
    use crate::service::{Pid, ReapedChild, Reaper, Spawner};
    use alloc::collections::VecDeque;
    use alloc::string::{String, ToString};
    use alloc::vec::Vec;
    use core::cell::{Cell, RefCell};
    use tairix_abi::{
        CapabilityId, Errno, LifecycleSignal, ManifestHeader, ReadinessKind, ReadyCondition,
        ServiceState, ABI_VERSION_CURRENT, MANIFEST_MAGIC, SYSCALL_TABLE_HASH_LEN,
    };
    use tairix_caps::CapabilitySet;
    use tairix_log::{Event, EventId, Level, Sink};

    /// Build a syntactically valid manifest requesting `requested`.
    fn manifest(requested: &[CapabilityId]) -> Vec<u8> {
        let header = ManifestHeader {
            magic: MANIFEST_MAGIC,
            abi_version: ABI_VERSION_CURRENT,
            flags: 0,
            capability_count: u16::try_from(requested.len()).unwrap(),
            reserved0: 0,
            syscall_table_hash: [0u8; SYSCALL_TABLE_HASH_LEN],
            signer_pubkey: [0u8; 32],
            signature: [0u8; 64],
        };
        let mut out = header.to_le_bytes().to_vec();
        for cap in requested {
            out.extend_from_slice(&cap.as_u16().to_le_bytes());
        }
        out
    }

    fn cap_set(list: &[CapabilityId]) -> CapabilitySet {
        let mut set = CapabilitySet::empty();
        for cap in list {
            set.insert(*cap);
        }
        set
    }

    fn spec(name: &str, requested: &[CapabilityId], deps: &[&str]) -> ServiceSpec {
        let deps: Vec<String> = deps.iter().map(|d| (*d).to_string()).collect();
        ServiceSpec::new(
            name,
            alloc::format!("/System/Services/{name}"),
            manifest(requested),
            deps,
        )
    }

    /// A `notify`-readiness service: it stays `starting` until it announces
    /// readiness, so a dependent is genuinely gated on the notice.
    fn notify_spec(name: &str, deps: &[&str]) -> ServiceSpec {
        spec(name, &[], deps).with_readiness(ReadinessKind::Notify)
    }

    /// Spawner that records each launch and can be told to fail a named
    /// service.
    struct MockSpawner {
        next: Cell<u64>,
        fail: Option<&'static str>,
        launched: RefCell<Vec<(String, CapabilitySet)>>,
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
        fn spawn(&self, spec: &ServiceSpec, granted: &CapabilitySet) -> Result<Pid, Errno> {
            if self.fail == Some(spec.name()) {
                return Err(Errno::NotFound);
            }
            let raw = self.next.get();
            self.next.set(raw + 1);
            self.launched
                .borrow_mut()
                .push((spec.name().to_string(), *granted));
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
        authority: CapabilitySet,
        spawner: &'a MockSpawner,
        reaper: &'a dyn Reaper,
        sink: &'a RecordingSink,
    ) -> InitConfig<'a> {
        InitConfig {
            authority,
            accepted_abi_version: ABI_VERSION_CURRENT,
            spawner,
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
        let authority = cap_set(&[CapabilityId::FS_MOUNT]);
        let mut init = Init::new(cfg(authority, &spawner, &reaper, &sink));
        // Register out of order; dependencies: c->b->a.
        init.register(spec("c", &[], &["b"])).unwrap();
        init.register(spec("a", &[], &[])).unwrap();
        init.register(spec("b", &[], &["a"])).unwrap();

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
        let mut init = Init::new(cfg(CapabilitySet::empty(), &spawner, &reaper, &sink));
        init.register(spec("a", &[], &["ghost"])).unwrap();

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
        let mut init = Init::new(cfg(CapabilitySet::empty(), &spawner, &reaper, &sink));
        init.register(spec("a", &[], &["b"])).unwrap();
        init.register(spec("b", &[], &["a"])).unwrap();

        assert_eq!(init.start_all(), Err(InitError::DependencyCycle));
        assert_eq!(init.running_count(), 0);
        assert_eq!(sink.count(events::GRAPH_REJECTED), 1);
    }

    #[test]
    fn duplicate_registration_is_rejected() {
        let spawner = MockSpawner::new();
        let reaper = IdleReaper;
        let sink = RecordingSink::new();
        let mut init = Init::new(cfg(CapabilitySet::empty(), &spawner, &reaper, &sink));
        init.register(spec("a", &[], &[])).unwrap();
        assert_eq!(
            init.register(spec("a", &[], &[])),
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
        let authority = cap_set(&[CapabilityId::FS_MOUNT]);
        let mut init = Init::new(cfg(authority, &spawner, &reaper, &sink));

        // Three bundles are discovered on disk; only two are enrolled.
        let discovered = alloc::vec![
            spec("netstack", &[], &[]),
            spec("sysinfod", &[], &[]),
            spec("rogue", &[], &[]),
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
        let mut init = Init::new(cfg(CapabilitySet::empty(), &spawner, &reaper, &sink));

        // A missing or corrupt store resolves to the empty enrolment, so no
        // discovered bundle is eligible — nothing auto-starts, fail closed.
        let discovered = alloc::vec![spec("netstack", &[], &[]), spec("sysinfod", &[], &[])];
        init.register_enrolled(discovered, &Enrolment::empty())
            .unwrap();

        assert_eq!(init.registered_count(), 0);
        assert_eq!(sink.count(events::SERVICE_NOT_ENROLLED), 2);
        assert!(spawner.launched.borrow().is_empty());
    }

    #[test]
    fn grant_is_manifest_request_intersected_with_authority() {
        let spawner = MockSpawner::new();
        let reaper = IdleReaper;
        let sink = RecordingSink::new();
        let authority = cap_set(&[CapabilityId::FS_MOUNT, CapabilityId::NET_RAW]);
        let mut init = Init::new(cfg(authority, &spawner, &reaper, &sink));
        init.register(spec("svc", &[CapabilityId::NET_RAW], &[]))
            .unwrap();

        let report = init.start_all().unwrap();
        assert!(report.all_started());
        let granted = &report.started[0].granted;
        assert!(granted.contains(CapabilityId::NET_RAW));
        assert!(!granted.contains(CapabilityId::FS_MOUNT));
        assert_eq!(granted.len(), 1);
        // The spawner saw exactly the granted ceiling.
        assert_eq!(spawner.launched.borrow()[0].1, *granted);
    }

    #[test]
    fn capability_escalation_is_refused() {
        let spawner = MockSpawner::new();
        let reaper = IdleReaper;
        let sink = RecordingSink::new();
        // Authority lacks NET_RAW, which the manifest requests.
        let authority = cap_set(&[CapabilityId::FS_MOUNT]);
        let mut init = Init::new(cfg(authority, &spawner, &reaper, &sink));
        init.register(spec("svc", &[CapabilityId::NET_RAW], &[]))
            .unwrap();

        let report = init.start_all().unwrap();
        assert!(report.started.is_empty());
        assert_eq!(report.failed.len(), 1);
        assert_eq!(report.failed[0].failure, StartFailure::CapabilityEscalation);
        assert_eq!(init.running_count(), 0);
        assert_eq!(sink.count(events::SERVICE_DENIED), 1);
        assert!(spawner.launched.borrow().is_empty());
    }

    #[test]
    fn spawn_failure_skips_transitive_dependents() {
        let spawner = MockSpawner::failing("b");
        let reaper = IdleReaper;
        let sink = RecordingSink::new();
        let mut init = Init::new(cfg(CapabilitySet::empty(), &spawner, &reaper, &sink));
        init.register(spec("a", &[], &[])).unwrap();
        init.register(spec("b", &[], &[])).unwrap();
        init.register(spec("c", &[], &["b"])).unwrap();
        init.register(spec("d", &[], &["c"])).unwrap();

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
    fn invalid_manifest_is_reported() {
        let spawner = MockSpawner::new();
        let reaper = IdleReaper;
        let sink = RecordingSink::new();
        let mut init = Init::new(cfg(CapabilitySet::empty(), &spawner, &reaper, &sink));
        let mut bad = manifest(&[]);
        bad[0] ^= 0xFF; // corrupt the magic
        init.register(ServiceSpec::new(
            "svc",
            "/System/Services/svc",
            bad,
            Vec::new(),
        ))
        .unwrap();

        let report = init.start_all().unwrap();
        assert_eq!(report.failed.len(), 1);
        assert_eq!(
            report.failed[0].failure,
            StartFailure::ManifestInvalid(Errno::BadMagic)
        );
        assert_eq!(sink.count(events::SERVICE_START_FAILED), 1);
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
        let mut init = Init::new(cfg(CapabilitySet::empty(), &spawner, &reaper, &sink));
        init.register(spec("svc", &[], &[])).unwrap();
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
        let mut init = Init::new(cfg(CapabilitySet::empty(), &spawner, &reaper, &sink));
        // `a` is a notify service; `b` (immediate) depends on it.
        init.register(notify_spec("a", &[])).unwrap();
        init.register(spec("b", &[], &["a"])).unwrap();

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
        let mut init = Init::new(cfg(CapabilitySet::empty(), &spawner, &reaper, &sink));
        init.register(notify_spec("a", &[])).unwrap();
        init.register(spec("b", &[], &["a"])).unwrap();

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
        let mut init = Init::new(cfg(CapabilitySet::empty(), &spawner, &reaper, &sink));
        init.register(spec("client", &[], &[]).requiring([ReadyCondition::NetworkUp]))
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
        let mut init = Init::new(cfg(CapabilitySet::empty(), &spawner, &reaper, &sink));
        // `netstack` provides network-up on readiness; `client` requires it.
        // Neither names the other — the condition decouples them.
        init.register(notify_spec("netstack", &[]).providing([ReadyCondition::NetworkUp]))
            .unwrap();
        init.register(spec("client", &[], &[]).requiring([ReadyCondition::NetworkUp]))
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
        let mut init = Init::new(cfg(CapabilitySet::empty(), &spawner, &reaper, &sink));
        init.register(notify_spec("a", &[])).unwrap();
        init.register(spec("b", &[], &["a"])).unwrap();

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
        let mut init = Init::new(cfg(CapabilitySet::empty(), &spawner, &reaper, &sink));
        init.register(spec("a", &[], &[])).unwrap();
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
}
