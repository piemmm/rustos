//! `sysinfod`'s own state: the registry of cache ledgers a userland process
//! reports for caches only it can see.
//!
//! The kernel's `MemStats::ramzip_reclaimable_residue()` gates a real
//! reclaim decision on the ledgers it measures itself, and a self-reported
//! figure must never be able to steer it. Keeping the reported rows here,
//! in `sysinfod`, rather than in the kernel, makes that impossible by
//! construction: the registry cannot feed a kernel decision because the
//! kernel never reads it, the blast radius of a hostile reporter is bounded
//! to this restartable service, and restarting `sysinfod` clears every lie.

use alloc::vec::Vec;

use tairix_abi::sysinfo::{
    CacheLedgerOrigin, CacheLedgerRecord, CacheOwnerKind, MAX_CACHE_REPORT_ENTRIES,
};
use tairix_abi::{Errno, ProcId};

use crate::source::Caller;

/// RAM, in bytes, one admitted reporter slot is derived from.
///
/// A reporter's own worst-case footprint is bounded by
/// [`MAX_CACHE_REPORT_ENTRIES`] rows; sizing the *count* of reporters from
/// the machine's RAM rather than a hand-picked constant means a large
/// machine gets a registry that scales with it and a small machine never
/// reserves more than it can afford.
pub(crate) const RAM_BYTES_PER_REPORTER: u64 = 16 << 20;

/// Fewest reporters the registry admits, however little RAM the machine
/// reports.
pub(crate) const MIN_REPORTERS: usize = 16;

/// Most reporters the registry admits, however much RAM the machine
/// reports. Together with [`MAX_CACHE_REPORT_ENTRIES`] this bounds the
/// registry's worst-case footprint to
/// `MAX_REPORTERS * MAX_CACHE_REPORT_ENTRIES` rows.
const MAX_REPORTERS: usize = 1024;

/// One reporting process's rows, keyed by its unforgeable process-instance
/// identity rather than its reusable numeric pid.
struct Reporter {
    proc_id: ProcId,
    pid: u64,
    rows: Vec<CacheLedgerRecord>,
}

/// The registry of cache ledgers userland processes report for their own
/// caches.
///
/// This is `sysinfod`'s own state, not a kernel table: every entry is keyed
/// by the reporting process's unforgeable [`ProcId`], so a numeric pid the
/// kernel later hands to an unrelated process can never inherit another
/// process's rows. A process's entry is replaced wholesale on every
/// [`report`](Self::report) call rather than merged, so a process's
/// footprint in the registry never grows however often it reports, and an
/// empty report withdraws the entry entirely — what a process does when it
/// tears its caches down.
///
/// Admission is capacity-bounded ([`Self::new`]) and never displaces a
/// live reporter's truthful rows to make room for an unknown one: a caller
/// filling a `sysinfod`-wide `report` pipeline is expected to call
/// [`retain_live`](Self::retain_live) immediately beforehand (as
/// `crate::service::serve` does), so a dead reporter's slot is already
/// free by the time a new one is admitted, and a registry that is still
/// full after that refuses the new reporter with [`Errno::NoSpace`].
pub struct CacheLedgerRegistry {
    reporters: Vec<Reporter>,
    capacity: usize,
}

impl CacheLedgerRegistry {
    /// Build an empty registry sized for a machine with `total_ram_bytes`
    /// of usable RAM.
    ///
    /// The registry admits one reporter per `RAM_BYTES_PER_REPORTER` of
    /// RAM, clamped to `MIN_REPORTERS`..=`MAX_REPORTERS`. The count is
    /// derived rather than hand-picked: a fixed ceiling would cap a large
    /// machine's reporter population at an arbitrary number and would
    /// still waste memory reserving unused slots on a small one, whereas
    /// deriving it from the discovered hardware lets the registry scale
    /// with the machine it runs on.
    #[must_use]
    pub fn new(total_ram_bytes: u64) -> Self {
        let derived = total_ram_bytes / RAM_BYTES_PER_REPORTER;
        let capacity = usize::try_from(derived)
            .unwrap_or(MAX_REPORTERS)
            .clamp(MIN_REPORTERS, MAX_REPORTERS);
        Self {
            reporters: Vec::new(),
            capacity,
        }
    }

    /// Replace `caller`'s entry in the registry with `rows`.
    ///
    /// `rows` must carry [`CacheLedgerRecord::origin`] left at
    /// [`CacheLedgerOrigin::Unset`] and [`CacheLedgerRecord::reporter_pid`]
    /// left at zero: those two fields are `sysinfod`'s to fill, and a
    /// caller that sets either is trying to present its own figures as
    /// kernel-measured, or to attribute them to another process. On
    /// success every row is stamped with
    /// [`CacheLedgerOrigin::SelfReported`] and `caller`'s numeric pid
    /// before it replaces (never appends to) the caller's previous entry.
    /// An empty `rows` withdraws the entry entirely.
    ///
    /// Every row must also name an owner the reporting process can
    /// truthfully be. A process describing its own caches is not a kernel
    /// subsystem, so a row claiming
    /// [`CacheOwnerKind::KernelSubsystem`] is refused rather than
    /// rendered beside the rows the kernel really measured — no correct
    /// reporter can produce one. The other four kinds stay legal for a
    /// reporter: a userland filesystem driver's cache is genuinely owned
    /// by the volume it caches, a per-task cache by its task, and a
    /// desktop cache by its seat.
    ///
    /// No audit record is emitted here. `CACHE_REPORT` is deliberately
    /// ungated in the query spec — any process may call it, including one
    /// with no other capability — so auditing this path would hand every
    /// process on the machine a way to write the security log; the
    /// dispatcher already logs nothing for it for the same reason.
    ///
    /// # Errors
    ///
    /// * [`Errno::BadMagic`] if any row's `origin` or `reporter_pid` is
    ///   already set, or its `owner_kind` is
    ///   [`CacheOwnerKind::KernelSubsystem`].
    /// * [`Errno::LengthOutOfRange`] if `rows.len()` exceeds
    ///   [`MAX_CACHE_REPORT_ENTRIES`].
    /// * [`Errno::PermissionDenied`] if `caller`'s attested process
    ///   instance is [`ProcId::KERNEL`] — a kernel-domain principal is
    ///   never a real user process and can never self-report a cache.
    /// * [`Errno::NoSpace`] if the registry is full and `caller` is not
    ///   already a reporter. Call [`Self::retain_live`] first to make room
    ///   for a genuinely new reporter; a live reporter's own rows are
    ///   never evicted to admit one.
    pub fn report(&mut self, caller: &Caller, rows: Vec<CacheLedgerRecord>) -> Result<(), Errno> {
        if rows.iter().any(|row| {
            row.origin != CacheLedgerOrigin::Unset
                || row.reporter_pid != 0
                || row.owner_kind == CacheOwnerKind::KernelSubsystem
        }) {
            return Err(Errno::BadMagic);
        }
        if rows.len() > MAX_CACHE_REPORT_ENTRIES {
            return Err(Errno::LengthOutOfRange);
        }
        let proc_id = caller.origin().proc_id();
        if proc_id.is_kernel() {
            return Err(Errno::PermissionDenied);
        }

        let existing = self.reporters.iter().position(|r| r.proc_id == proc_id);

        if rows.is_empty() {
            if let Some(index) = existing {
                self.reporters.remove(index);
            }
            return Ok(());
        }

        let pid = caller.origin().pid();
        let stamped: Vec<CacheLedgerRecord> = rows
            .into_iter()
            .map(|mut row| {
                row.origin = CacheLedgerOrigin::SelfReported;
                row.reporter_pid = pid;
                row
            })
            .collect();

        if let Some(index) = existing {
            let reporter = &mut self.reporters[index];
            reporter.pid = pid;
            reporter.rows = stamped;
            return Ok(());
        }

        if self.reporters.len() >= self.capacity {
            return Err(Errno::NoSpace);
        }
        self.reporters.push(Reporter {
            proc_id,
            pid,
            rows: stamped,
        });
        Ok(())
    }

    /// Drop every reporter whose process instance is not in `live`.
    ///
    /// Keyed on the unforgeable [`ProcId`], not the reusable numeric pid,
    /// so a pid the kernel recycles to an unrelated process can never
    /// inherit a dead reporter's rows: the recycled process reports under
    /// its own, different `ProcId` and is admitted as a fresh entry.
    pub fn retain_live(&mut self, live: &[ProcId]) {
        self.reporters.retain(|r| live.contains(&r.proc_id));
    }

    /// Every retained row, in a stable order: by reporter (its `ProcId`),
    /// then by label within a reporter.
    ///
    /// The order is a function of the registry's current contents alone,
    /// never of insertion or call history, so a client paging the
    /// combined list across several calls never skips or repeats a row as
    /// long as the registry itself is unchanged between them.
    #[must_use]
    pub fn rows(&self) -> Vec<CacheLedgerRecord> {
        let mut reporters: Vec<&Reporter> = self.reporters.iter().collect();
        reporters.sort_by_key(|r| r.proc_id);
        let mut out = Vec::new();
        for reporter in reporters {
            let mut rows: Vec<&CacheLedgerRecord> = reporter.rows.iter().collect();
            rows.sort_by(|a, b| a.label().cmp(b.label()));
            out.extend(rows.into_iter().copied());
        }
        out
    }
}

#[cfg(test)]
mod tests {
    use super::{CacheLedgerRegistry, MAX_REPORTERS, MIN_REPORTERS, RAM_BYTES_PER_REPORTER};
    use crate::testing::{kernel_caller, user_caller};
    use alloc::vec;
    use tairix_abi::sysinfo::{CacheLedgerOrigin, CacheLedgerRecord, CacheOwnerKind};
    use tairix_abi::Errno;

    fn row(label: &str) -> CacheLedgerRecord {
        owned_row(label, CacheOwnerKind::UserlandProcess)
    }

    fn owned_row(label: &str, owner_kind: CacheOwnerKind) -> CacheLedgerRecord {
        CacheLedgerRecord::new(label.as_bytes(), owner_kind, 0, 0).expect("valid label")
    }

    #[test]
    fn new_derives_capacity_from_ram_and_clamps() {
        assert_eq!(CacheLedgerRegistry::new(0).capacity, MIN_REPORTERS);
        assert_eq!(
            CacheLedgerRegistry::new(RAM_BYTES_PER_REPORTER * 100).capacity,
            100
        );
        assert_eq!(CacheLedgerRegistry::new(u64::MAX).capacity, MAX_REPORTERS);
    }

    #[test]
    fn report_stamps_origin_and_reporter_pid() {
        let mut registry = CacheLedgerRegistry::new(1 << 30);
        let caller = user_caller(&[], 1, 42);
        registry
            .report(&caller, vec![row("glyphs")])
            .expect("admits a fresh reporter");
        let rows = registry.rows();
        assert_eq!(rows.len(), 1);
        assert_eq!(rows[0].origin, CacheLedgerOrigin::SelfReported);
        assert_eq!(rows[0].reporter_pid, 42);
    }

    #[test]
    fn report_refuses_preset_origin() {
        let mut registry = CacheLedgerRegistry::new(1 << 30);
        let caller = user_caller(&[], 1, 42);
        let mut malicious = row("glyphs");
        malicious.origin = CacheLedgerOrigin::Kernel;
        assert_eq!(
            registry.report(&caller, vec![malicious]),
            Err(Errno::BadMagic)
        );
        assert!(registry.rows().is_empty());
    }

    #[test]
    fn report_refuses_preset_reporter_pid() {
        let mut registry = CacheLedgerRegistry::new(1 << 30);
        let caller = user_caller(&[], 1, 42);
        let mut malicious = row("glyphs");
        malicious.reporter_pid = 7;
        assert_eq!(
            registry.report(&caller, vec![malicious]),
            Err(Errno::BadMagic)
        );
        assert!(registry.rows().is_empty());
    }

    #[test]
    fn report_refuses_a_kernel_subsystem_owner_kind() {
        let mut registry = CacheLedgerRegistry::new(1 << 30);
        let caller = user_caller(&[], 1, 42);
        registry
            .report(&caller, vec![row("glyphs")])
            .expect("a truthful report is admitted");

        assert_eq!(
            registry.report(
                &caller,
                vec![
                    row("artwork"),
                    owned_row("pretender", CacheOwnerKind::KernelSubsystem),
                ]
            ),
            Err(Errno::BadMagic)
        );

        // Refused before the entry is touched, and refused whole: the
        // truthful row submitted beside the pretender is not admitted
        // either, and what the caller reported earlier still stands.
        let rows = registry.rows();
        assert_eq!(rows.len(), 1);
        assert_eq!(rows[0].label(), "glyphs");
    }

    #[test]
    fn report_admits_every_owner_kind_a_process_can_truthfully_be() {
        let mut registry = CacheLedgerRegistry::new(1 << 30);
        let caller = user_caller(&[], 1, 42);
        registry
            .report(
                &caller,
                vec![
                    owned_row("volume", CacheOwnerKind::FilesystemVolume),
                    owned_row("task", CacheOwnerKind::Task),
                    owned_row("seat", CacheOwnerKind::DesktopSession),
                    owned_row("process", CacheOwnerKind::UserlandProcess),
                ],
            )
            .expect("a userland cache can be owned by a volume, task, seat, or process");

        let rows = registry.rows();
        let owners: alloc::vec::Vec<(&str, CacheOwnerKind)> =
            rows.iter().map(|r| (r.label(), r.owner_kind)).collect();
        assert_eq!(
            owners,
            [
                ("process", CacheOwnerKind::UserlandProcess),
                ("seat", CacheOwnerKind::DesktopSession),
                ("task", CacheOwnerKind::Task),
                ("volume", CacheOwnerKind::FilesystemVolume),
            ]
        );
    }

    #[test]
    fn report_refuses_oversize_submission() {
        let mut registry = CacheLedgerRegistry::new(1 << 30);
        let caller = user_caller(&[], 1, 42);
        let rows = vec![row("a"); super::MAX_CACHE_REPORT_ENTRIES + 1];
        assert_eq!(registry.report(&caller, rows), Err(Errno::LengthOutOfRange));
        assert!(registry.rows().is_empty());
    }

    #[test]
    fn report_refuses_kernel_domain_caller() {
        let mut registry = CacheLedgerRegistry::new(1 << 30);
        assert_eq!(
            registry.report(&kernel_caller(), vec![row("glyphs")]),
            Err(Errno::PermissionDenied)
        );
        assert!(registry.rows().is_empty());
    }

    #[test]
    fn second_report_replaces_rather_than_appends() {
        let mut registry = CacheLedgerRegistry::new(1 << 30);
        let caller = user_caller(&[], 1, 42);
        registry
            .report(&caller, vec![row("glyphs"), row("artwork")])
            .expect("first report admitted");
        registry
            .report(&caller, vec![row("cursors")])
            .expect("second report admitted");
        let rows = registry.rows();
        assert_eq!(rows.len(), 1);
        assert_eq!(rows[0].label(), "cursors");
    }

    #[test]
    fn empty_report_withdraws_the_reporter() {
        let mut registry = CacheLedgerRegistry::new(1 << 30);
        let caller = user_caller(&[], 1, 42);
        registry
            .report(&caller, vec![row("glyphs")])
            .expect("first report admitted");
        registry
            .report(&caller, vec![])
            .expect("empty report withdraws");
        assert!(registry.rows().is_empty());
    }

    #[test]
    fn retain_live_drops_dead_reporters_by_proc_id() {
        let mut registry = CacheLedgerRegistry::new(1 << 30);
        let gone = user_caller(&[], 1, 42);
        let staying = user_caller(&[], 2, 43);
        registry
            .report(&gone, vec![row("glyphs")])
            .expect("admitted");
        registry
            .report(&staying, vec![row("artwork")])
            .expect("admitted");

        registry.retain_live(&[staying.origin().proc_id()]);

        let rows = registry.rows();
        assert_eq!(rows.len(), 1);
        assert_eq!(rows[0].label(), "artwork");
    }

    #[test]
    fn recycled_pid_does_not_inherit_a_dead_reporters_rows() {
        let mut registry = CacheLedgerRegistry::new(1 << 30);
        let original = user_caller(&[], 1, 99);
        registry
            .report(&original, vec![row("glyphs")])
            .expect("admitted");

        // The instance is gone; its pid 99 is recycled to an unrelated
        // process with a fresh ProcId.
        registry.retain_live(&[]);
        let recycled = user_caller(&[], 9, 99);
        registry
            .report(&recycled, vec![row("cursors")])
            .expect("admitted as a new reporter");

        let rows = registry.rows();
        assert_eq!(rows.len(), 1);
        assert_eq!(rows[0].label(), "cursors");
    }

    #[test]
    fn full_registry_refuses_a_new_reporter_without_evicting_live_ones() {
        let floor = u8::try_from(MIN_REPORTERS).expect("the reporter floor fits a fixture tag");
        let mut registry = CacheLedgerRegistry::new(RAM_BYTES_PER_REPORTER * u64::from(floor));
        for tag in 0..floor {
            let caller = user_caller(&[], tag, u64::from(tag));
            registry
                .report(&caller, vec![row("glyphs")])
                .expect("admitted within capacity");
        }
        let overflow = user_caller(&[], 200, 200);
        assert_eq!(
            registry.report(&overflow, vec![row("glyphs")]),
            Err(Errno::NoSpace)
        );
        assert_eq!(registry.rows().len(), MIN_REPORTERS);
    }

    #[test]
    fn rows_are_ordered_by_reporter_then_label() {
        let mut registry = CacheLedgerRegistry::new(1 << 30);
        let first = user_caller(&[], 1, 1);
        let second = user_caller(&[], 2, 2);
        registry
            .report(&second, vec![row("zeta"), row("alpha")])
            .expect("admitted");
        registry
            .report(&first, vec![row("beta")])
            .expect("admitted");

        let rows = registry.rows();
        let labels: alloc::vec::Vec<&str> = rows.iter().map(CacheLedgerRecord::label).collect();
        assert_eq!(labels, ["beta", "alpha", "zeta"]);
    }
}
