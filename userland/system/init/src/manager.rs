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

use rustos_abi::{
    decode_capability_ids, CapabilityId, Errno, ManifestHeader, MANIFEST_MAX_CAPABILITIES,
};
use rustos_caps::CapabilitySet;
use rustos_log::{log, Event, EventId, Field, Level, Sink};

use crate::error::{InitError, StartFailure};
use crate::events;
use crate::service::{Pid, ReapedChild, Reaper, ServiceSpec, Spawner};

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

/// A currently-running service tracked for reaping.
struct RunningService {
    name: String,
    pid: Pid,
}

/// PID 1 service manager (Stage 6).
pub struct Init<'a> {
    cfg: InitConfig<'a>,
    services: Vec<ServiceSpec>,
    running: Vec<RunningService>,
}

impl<'a> Init<'a> {
    /// Create a manager with no registered services.
    #[must_use]
    pub fn new(cfg: InitConfig<'a>) -> Self {
        Self {
            cfg,
            services: Vec::new(),
            running: Vec::new(),
        }
    }

    /// Number of registered services.
    #[must_use]
    pub fn registered_count(&self) -> usize {
        self.services.len()
    }

    /// Number of services currently tracked as running.
    #[must_use]
    pub fn running_count(&self) -> usize {
        self.running.len()
    }

    /// The [`Pid`] of a running service, or `None` if it is not running.
    #[must_use]
    pub fn running_pid(&self, name: &str) -> Option<Pid> {
        self.running.iter().find(|r| r.name == name).map(|r| r.pid)
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
        self.services.push(spec);
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
        let order = match self.topological_order() {
            Ok(order) => order,
            Err(err) => {
                self.audit_graph_rejected(err);
                return Err(err);
            }
        };

        let mut failed = vec![false; self.services.len()];
        let mut report = StartReport::default();
        for &idx in &order {
            if self.dependency_failed(idx, &failed) {
                failed[idx] = true;
                let name = self.services[idx].name().to_string();
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
                continue;
            }
            match self.try_start(idx) {
                Ok(started) => report.started.push(started),
                Err(failure) => {
                    failed[idx] = true;
                    report.failed.push(failure);
                }
            }
        }
        Ok(report)
    }

    /// Reap every child that has exited, returning the number reaped.
    ///
    /// A reaped process that matches a running service is logged as a
    /// service exit and removed from the running set; any other reaped
    /// process is an inherited orphan and is logged as such (PID 1 reaps the whole system's zombies).
    pub fn reap(&mut self) -> usize {
        let mut reaped = 0;
        while let Some(child) = self.cfg.reaper.collect() {
            reaped += 1;
            if let Some(pos) = self.running.iter().position(|r| r.pid == child.pid) {
                let service = self.running.remove(pos);
                self.audit_exit(&service.name, child);
            } else {
                self.audit_orphan(child);
            }
        }
        reaped
    }

    fn index_of(&self, name: &str) -> Option<usize> {
        self.services.iter().position(|s| s.name() == name)
    }

    /// Compute a dependency-respecting start order, or report a structural
    /// defect. Ready services are emitted in registration order so the
    /// result is deterministic.
    fn topological_order(&self) -> Result<Vec<usize>, InitError> {
        let n = self.services.len();
        let mut indegree = vec![0usize; n];
        let mut dependents: Vec<Vec<usize>> = vec![Vec::new(); n];
        for (i, spec) in self.services.iter().enumerate() {
            for dep in spec.dependencies() {
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

    fn dependency_failed(&self, idx: usize, failed: &[bool]) -> bool {
        self.services[idx]
            .dependencies()
            .iter()
            .any(|dep| self.index_of(dep).is_some_and(|d| failed[d]))
    }

    fn try_start(&mut self, idx: usize) -> Result<StartedService, FailedService> {
        let name = self.services[idx].name().to_string();

        let requested = match self.requested_capabilities(idx) {
            Ok(set) => set,
            Err(failure) => {
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

        let pid = match self.cfg.spawner.spawn(&self.services[idx], &granted) {
            Ok(pid) => pid,
            Err(err) => {
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

        self.audit_started(&name, pid, &granted);
        self.running.push(RunningService {
            name: name.clone(),
            pid,
        });
        Ok(StartedService { name, pid, granted })
    }

    /// Decode a service manifest into the capability set it requests.
    fn requested_capabilities(&self, idx: usize) -> Result<CapabilitySet, StartFailure> {
        let manifest = self.services[idx].manifest();
        let header = ManifestHeader::from_bytes(manifest).map_err(StartFailure::ManifestInvalid)?;
        if header.abi_version != self.cfg.accepted_abi_version {
            return Err(StartFailure::ManifestInvalid(Errno::AbiVersionUnsupported));
        }
        let count = usize::from(header.capability_count);
        let body = manifest
            .get(ManifestHeader::WIRE_LEN..)
            .ok_or(StartFailure::ManifestInvalid(Errno::BufferTooSmall))?;
        let mut scratch = [CapabilityId::FS_MOUNT; MANIFEST_MAX_CAPABILITIES as usize];
        let decoded = decode_capability_ids(body, count, &mut scratch)
            .map_err(StartFailure::ManifestInvalid)?;
        let mut set = CapabilitySet::empty();
        for cap in &scratch[..decoded] {
            set.insert(*cap);
        }
        Ok(set)
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
                    value: name,
                },
                Field {
                    key: "reason",
                    value: reason,
                },
            ],
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
                    value: name,
                },
                Field {
                    key: "pid",
                    value: pid_buf.format(i128::from(pid.as_u64())),
                },
                Field {
                    key: "granted_caps",
                    value: cap_buf.format(i128::from(granted.len())),
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
                    value: name,
                },
                Field {
                    key: "pid",
                    value: pid_buf.format(i128::from(child.pid.as_u64())),
                },
                Field {
                    key: "exit_code",
                    value: code_buf.format(i128::from(child.exit_code)),
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
                    value: pid_buf.format(i128::from(child.pid.as_u64())),
                },
                Field {
                    key: "exit_code",
                    value: code_buf.format(i128::from(child.exit_code)),
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
                value: reason,
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
    use super::{event_message, DecBuf, Init, InitConfig, InitError, ServiceSpec, StartFailure};
    use crate::events;
    use crate::service::{Pid, ReapedChild, Reaper, Spawner};
    use alloc::collections::VecDeque;
    use alloc::string::{String, ToString};
    use alloc::vec::Vec;
    use core::cell::{Cell, RefCell};
    use rustos_abi::{
        CapabilityId, Errno, ManifestHeader, ABI_VERSION_CURRENT, MANIFEST_MAGIC,
        SYSCALL_TABLE_HASH_LEN,
    };
    use rustos_caps::CapabilitySet;
    use rustos_log::{Event, EventId, Level, Sink};

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
}
