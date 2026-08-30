//! `sysinfod`'s own state: what processes have told it about **themselves**.
//!
//! Two kinds of figure can only be seen from inside the process that holds
//! them — a userland cache's ledger, and a compositor's frame accounting —
//! and neither is measurable from outside the desktop or the service that
//! owns it. Both therefore arrive as submissions and are retained here.
//!
//! The kernel's `MemStats::ramzip_reclaimable_residue()` gates a real
//! reclaim decision on the ledgers it measures itself, and a self-reported
//! figure must never be able to steer it. Keeping the reported values here,
//! in `sysinfod`, rather than in the kernel, makes that impossible by
//! construction: nothing here can feed a kernel decision because the kernel
//! never reads it, the blast radius of a hostile reporter is bounded to this
//! restartable service, and restarting `sysinfod` clears every lie.
//!
//! Both submissions are ungated by design — a process describing itself
//! grants nothing and reads nothing — so the table that holds them is what
//! bounds an untrusted population of reporters. That bound is shared rather
//! than written twice: one table type keyed on the unforgeable [`ProcId`],
//! one capacity derived from the machine's RAM, one liveness sweep.

use alloc::vec::Vec;

use tairix_abi::sysinfo::{
    CacheLedgerOrigin, CacheLedgerRecord, CacheOwnerKind, DesktopFrameRecord, DesktopFrameTotals,
    MAX_CACHE_REPORT_ENTRIES,
};
use tairix_abi::{Errno, ProcId};

use crate::source::Caller;

/// RAM, in bytes, one admitted reporter slot is derived from.
///
/// A cache reporter's own worst-case footprint is bounded by
/// [`MAX_CACHE_REPORT_ENTRIES`] rows and a frame reporter's by one
/// [`DesktopFrameTotals`], so sizing the *count* of reporters from the
/// machine's RAM rather than a hand-picked constant means a large machine
/// gets a registry that scales with it and a small machine never reserves
/// more than it can afford.
pub(crate) const RAM_BYTES_PER_REPORTER: u64 = 16 << 20;

/// Fewest reporters a table admits, however little RAM the machine reports.
pub(crate) const MIN_REPORTERS: usize = 16;

/// Most reporters a table admits, however much RAM the machine reports.
/// Together with the per-reporter payload bounds this caps each table's
/// worst-case footprint.
const MAX_REPORTERS: usize = 1024;

/// Reporter slots a machine with `total_ram_bytes` of usable RAM admits per
/// table: one per [`RAM_BYTES_PER_REPORTER`], clamped to
/// `MIN_REPORTERS..=MAX_REPORTERS`.
///
/// Derived rather than hand-picked: a fixed ceiling would cap a large
/// machine's reporter population at an arbitrary number and would still
/// waste memory reserving unused slots on a small one.
fn derive_capacity(total_ram_bytes: u64) -> usize {
    usize::try_from(total_ram_bytes / RAM_BYTES_PER_REPORTER)
        .unwrap_or(MAX_REPORTERS)
        .clamp(MIN_REPORTERS, MAX_REPORTERS)
}

/// The key a submission is retained under, refusing a principal that can
/// never self-report.
///
/// A kernel-domain caller carries the reserved [`ProcId::KERNEL`]: it is
/// never a real user process, so it holds neither a userland cache nor a
/// compositor.
fn reporter_id(caller: &Caller) -> Result<ProcId, Errno> {
    let proc_id = caller.origin().proc_id();
    if proc_id.is_kernel() {
        return Err(Errno::PermissionDenied);
    }
    Ok(proc_id)
}

/// One reporting process's submitted value.
struct Entry<T> {
    proc_id: ProcId,
    pid: u64,
    value: T,
}

/// One self-reported value per live process, keyed by the reporter's
/// unforgeable process-instance identity rather than its reusable numeric
/// pid.
///
/// A process's entry is replaced wholesale on every [`put`](Self::put) call
/// rather than merged, so a process's footprint never grows however often it
/// reports. Admission is capacity-bounded and never displaces a live
/// reporter's truthful value to make room for an unknown one: a full table
/// sweeps its dead reporters and, if that frees nothing, refuses the new one
/// with [`Errno::NoSpace`].
///
/// The sweep is what makes room, so it is paid only when room is what is
/// missing. Establishing the live set means enumerating the machine's whole
/// process table, and the submissions that dominate this table by orders of
/// magnitude — a desktop restating its frame accounting, a runtime restating
/// its cache ledger — are from reporters already admitted, who need no room
/// at all. Sweeping on each of those would scale a re-submission with the
/// machine's process count.
struct ReportTable<T> {
    entries: Vec<Entry<T>>,
    capacity: usize,
}

impl<T> ReportTable<T> {
    /// An empty table admitting `capacity` reporters.
    const fn new(capacity: usize) -> Self {
        Self {
            entries: Vec::new(),
            capacity,
        }
    }

    /// Replace `caller`'s entry with `value`, sweeping dead reporters to
    /// admit a new one only if the table is full.
    ///
    /// `live` is consulted at most once, and only on that path.
    ///
    /// # Errors
    ///
    /// * [`Errno::PermissionDenied`] as [`reporter_id`] raises it.
    /// * Whatever `live` raises, when the table is full enough to need it.
    /// * [`Errno::NoSpace`] if the table is still full of live reporters.
    fn put(
        &mut self,
        caller: &Caller,
        value: T,
        live: impl FnOnce() -> Result<Vec<ProcId>, Errno>,
    ) -> Result<(), Errno> {
        let proc_id = reporter_id(caller)?;
        let pid = caller.origin().pid();
        if let Some(entry) = self.entries.iter_mut().find(|e| e.proc_id == proc_id) {
            entry.pid = pid;
            entry.value = value;
            return Ok(());
        }
        if self.entries.len() >= self.capacity {
            self.retain_live(&live()?);
            if self.entries.len() >= self.capacity {
                return Err(Errno::NoSpace);
            }
        }
        self.entries.push(Entry {
            proc_id,
            pid,
            value,
        });
        Ok(())
    }

    /// Drop `caller`'s entry, if it has one. What a process does when it has
    /// nothing left to report.
    ///
    /// # Errors
    ///
    /// [`Errno::PermissionDenied`] as [`reporter_id`] raises it: withdrawing
    /// an entry such a principal could never hold is meaningless, and
    /// answering `Ok` would tell it otherwise.
    fn withdraw(&mut self, caller: &Caller) -> Result<(), Errno> {
        let proc_id = reporter_id(caller)?;
        self.entries.retain(|e| e.proc_id != proc_id);
        Ok(())
    }

    /// Drop every entry whose process instance is not in `live`.
    ///
    /// Keyed on the unforgeable [`ProcId`], not the reusable numeric pid, so
    /// a pid the kernel recycles to an unrelated process can never inherit a
    /// dead reporter's value: the recycled process reports under its own,
    /// different `ProcId` and is admitted as a fresh entry.
    fn retain_live(&mut self, live: &[ProcId]) {
        self.entries.retain(|e| live.contains(&e.proc_id));
    }

    /// Every entry, ordered by reporter.
    ///
    /// The order is a function of the table's current contents alone, never
    /// of insertion or call history, so a client paging a list across several
    /// calls never skips or repeats a row as long as the table itself is
    /// unchanged between them.
    fn sorted(&self) -> Vec<&Entry<T>> {
        let mut entries: Vec<&Entry<T>> = self.entries.iter().collect();
        entries.sort_by_key(|e| e.proc_id);
        entries
    }
}

/// Everything processes have reported about themselves.
pub struct SelfReports {
    caches: ReportTable<Vec<CacheLedgerRecord>>,
    frames: ReportTable<DesktopFrameTotals>,
}

impl SelfReports {
    /// Build empty tables sized for a machine with `total_ram_bytes` of
    /// usable RAM.
    #[must_use]
    pub fn new(total_ram_bytes: u64) -> Self {
        let capacity = derive_capacity(total_ram_bytes);
        Self {
            caches: ReportTable::new(capacity),
            frames: ReportTable::new(capacity),
        }
    }

    /// Drop every reporter, of either kind, whose process instance is not in
    /// `live`.
    pub fn retain_live(&mut self, live: &[ProcId]) {
        self.caches.retain_live(live);
        self.frames.retain_live(live);
    }

    /// Replace `caller`'s cache ledgers with `rows`.
    ///
    /// `rows` must carry [`CacheLedgerRecord::origin`] left at
    /// [`CacheLedgerOrigin::Unset`] and [`CacheLedgerRecord::reporter_pid`]
    /// left at zero: those two fields are `sysinfod`'s to fill, and a caller
    /// that sets either is trying to present its own figures as
    /// kernel-measured, or to attribute them to another process. On success
    /// every row is stamped with [`CacheLedgerOrigin::SelfReported`] and
    /// `caller`'s numeric pid before it replaces (never appends to) the
    /// caller's previous entry. An empty `rows` withdraws the entry
    /// entirely.
    ///
    /// Every row must also name an owner the reporting process can
    /// truthfully be. A process describing its own caches is not a kernel
    /// subsystem, so a row claiming [`CacheOwnerKind::KernelSubsystem`] is
    /// refused rather than rendered beside the rows the kernel really
    /// measured — no correct reporter can produce one. The other four kinds
    /// stay legal for a reporter: a userland filesystem driver's cache is
    /// genuinely owned by the volume it caches, a per-task cache by its
    /// task, and a desktop cache by its seat.
    ///
    /// No audit record is emitted here. The submission is deliberately
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
    /// * [`Errno::PermissionDenied`] if `caller`'s attested process instance
    ///   is [`ProcId::KERNEL`] — a kernel-domain principal is never a real
    ///   user process and holds no userland cache.
    /// * Whatever `live` raises, on the sole path that consults it.
    /// * [`Errno::NoSpace`] if the table is full of *live* reporters; a live
    ///   reporter's own rows are never evicted to admit a new one.
    ///
    /// `live` yields the machine's live process instances and is called at
    /// most once, only to make room for a reporter not already admitted.
    pub fn report_cache_ledgers(
        &mut self,
        caller: &Caller,
        rows: Vec<CacheLedgerRecord>,
        live: impl FnOnce() -> Result<Vec<ProcId>, Errno>,
    ) -> Result<(), Errno> {
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
        if rows.is_empty() {
            return self.caches.withdraw(caller);
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
        self.caches.put(caller, stamped, live)
    }

    /// Every retained cache row, in a stable order: by reporter (its
    /// `ProcId`), then by label within a reporter.
    #[must_use]
    pub fn cache_rows(&self) -> Vec<CacheLedgerRecord> {
        let mut out = Vec::new();
        for reporter in self.caches.sorted() {
            let mut rows: Vec<&CacheLedgerRecord> = reporter.value.iter().collect();
            rows.sort_by(|a, b| a.label().cmp(b.label()));
            out.extend(rows.into_iter().copied());
        }
        out
    }

    /// Replace `caller`'s compositor frame accounting with `totals`.
    ///
    /// `totals` has already passed [`DesktopFrameTotals::from_bytes`]'s
    /// fail-closed bounds by the time it arrives, so what is left here is
    /// *who* may report: a kernel-domain principal never composites, and the
    /// publisher is identified by the kernel-attested instance rather than
    /// by anything it said. Nothing in the submission names a process, so
    /// there is no attribution field to refuse — the pid is stamped onto the
    /// served record instead.
    ///
    /// [`DesktopFrameTotals::ZERO`] withdraws the entry, which is how a
    /// session that has torn its compositor down stops being counted; a live
    /// desktop never publishes it, since a composed frame moves at least the
    /// frame counter.
    ///
    /// # Errors
    ///
    /// * [`Errno::PermissionDenied`] if `caller`'s attested process instance
    ///   is [`ProcId::KERNEL`] — a kernel-domain principal never composites.
    /// * Whatever `live` raises, and [`Errno::NoSpace`] if the table is full
    ///   of live publishers, both as
    ///   [`report_cache_ledgers`](Self::report_cache_ledgers) describes.
    pub fn report_frame_totals(
        &mut self,
        caller: &Caller,
        totals: DesktopFrameTotals,
        live: impl FnOnce() -> Result<Vec<ProcId>, Errno>,
    ) -> Result<(), Errno> {
        if totals == DesktopFrameTotals::ZERO {
            return self.frames.withdraw(caller);
        }
        self.frames.put(caller, totals, live)
    }

    /// Every retained frame report, ordered by reporter, each attributed to
    /// the process the service attested it to.
    #[must_use]
    pub fn frame_records(&self) -> Vec<DesktopFrameRecord> {
        self.frames
            .sorted()
            .into_iter()
            .map(|entry| DesktopFrameRecord {
                reporter_pid: entry.pid,
                totals: entry.value,
            })
            .collect()
    }
}

#[cfg(test)]
mod tests {
    use super::{
        derive_capacity, SelfReports, MAX_REPORTERS, MIN_REPORTERS, RAM_BYTES_PER_REPORTER,
    };
    use crate::testing::{kernel_caller, user_caller};
    use alloc::vec;
    use tairix_abi::sysinfo::{CacheLedgerOrigin, CacheLedgerRecord, CacheOwnerKind};
    use tairix_abi::{Errno, ProcId};

    /// A live-set source that must never be consulted.
    ///
    /// Establishing it enumerates the machine's whole process table, so a
    /// submission that needs no room must never ask for it. Every test below
    /// passes this, which is what pins a re-submission's cost off the process
    /// count rather than merely asserting it once.
    fn never_swept() -> Result<alloc::vec::Vec<ProcId>, Errno> {
        panic!("an admitted reporter's submission must not enumerate the process table")
    }

    fn row(label: &str) -> CacheLedgerRecord {
        owned_row(label, CacheOwnerKind::UserlandProcess)
    }

    fn owned_row(label: &str, owner_kind: CacheOwnerKind) -> CacheLedgerRecord {
        CacheLedgerRecord::new(label.as_bytes(), owner_kind, 0, 0).expect("valid label")
    }

    #[test]
    fn capacity_is_derived_from_ram_and_clamped() {
        assert_eq!(derive_capacity(0), MIN_REPORTERS);
        assert_eq!(derive_capacity(RAM_BYTES_PER_REPORTER * 100), 100);
        assert_eq!(derive_capacity(u64::MAX), MAX_REPORTERS);
    }

    #[test]
    fn report_stamps_origin_and_reporter_pid() {
        let mut registry = SelfReports::new(1 << 30);
        let caller = user_caller(&[], 1, 42);
        registry
            .report_cache_ledgers(&caller, vec![row("glyphs")], never_swept)
            .expect("admits a fresh reporter");
        let rows = registry.cache_rows();
        assert_eq!(rows.len(), 1);
        assert_eq!(rows[0].origin, CacheLedgerOrigin::SelfReported);
        assert_eq!(rows[0].reporter_pid, 42);
    }

    #[test]
    fn report_refuses_preset_origin() {
        let mut registry = SelfReports::new(1 << 30);
        let caller = user_caller(&[], 1, 42);
        let mut malicious = row("glyphs");
        malicious.origin = CacheLedgerOrigin::Kernel;
        assert_eq!(
            registry.report_cache_ledgers(&caller, vec![malicious], never_swept),
            Err(Errno::BadMagic)
        );
        assert!(registry.cache_rows().is_empty());
    }

    #[test]
    fn report_refuses_preset_reporter_pid() {
        let mut registry = SelfReports::new(1 << 30);
        let caller = user_caller(&[], 1, 42);
        let mut malicious = row("glyphs");
        malicious.reporter_pid = 7;
        assert_eq!(
            registry.report_cache_ledgers(&caller, vec![malicious], never_swept),
            Err(Errno::BadMagic)
        );
        assert!(registry.cache_rows().is_empty());
    }

    #[test]
    fn report_refuses_a_kernel_subsystem_owner_kind() {
        let mut registry = SelfReports::new(1 << 30);
        let caller = user_caller(&[], 1, 42);
        registry
            .report_cache_ledgers(&caller, vec![row("glyphs")], never_swept)
            .expect("a truthful report is admitted");

        assert_eq!(
            registry.report_cache_ledgers(
                &caller,
                vec![
                    row("artwork"),
                    owned_row("pretender", CacheOwnerKind::KernelSubsystem),
                ],
                never_swept,
            ),
            Err(Errno::BadMagic)
        );

        // Refused before the entry is touched, and refused whole: the
        // truthful row submitted beside the pretender is not admitted
        // either, and what the caller reported earlier still stands.
        let rows = registry.cache_rows();
        assert_eq!(rows.len(), 1);
        assert_eq!(rows[0].label(), "glyphs");
    }

    #[test]
    fn report_admits_every_owner_kind_a_process_can_truthfully_be() {
        let mut registry = SelfReports::new(1 << 30);
        let caller = user_caller(&[], 1, 42);
        registry
            .report_cache_ledgers(
                &caller,
                vec![
                    owned_row("volume", CacheOwnerKind::FilesystemVolume),
                    owned_row("task", CacheOwnerKind::Task),
                    owned_row("seat", CacheOwnerKind::DesktopSession),
                    owned_row("process", CacheOwnerKind::UserlandProcess),
                ],
                never_swept,
            )
            .expect("a userland cache can be owned by a volume, task, seat, or process");

        let rows = registry.cache_rows();
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
        let mut registry = SelfReports::new(1 << 30);
        let caller = user_caller(&[], 1, 42);
        let rows = vec![row("a"); super::MAX_CACHE_REPORT_ENTRIES + 1];
        assert_eq!(
            registry.report_cache_ledgers(&caller, rows, never_swept),
            Err(Errno::LengthOutOfRange)
        );
        assert!(registry.cache_rows().is_empty());
    }

    #[test]
    fn report_refuses_kernel_domain_caller() {
        let mut registry = SelfReports::new(1 << 30);
        assert_eq!(
            registry.report_cache_ledgers(&kernel_caller(), vec![row("glyphs")], never_swept),
            Err(Errno::PermissionDenied)
        );
        assert!(registry.cache_rows().is_empty());
    }

    #[test]
    fn second_report_replaces_rather_than_appends() {
        let mut registry = SelfReports::new(1 << 30);
        let caller = user_caller(&[], 1, 42);
        registry
            .report_cache_ledgers(&caller, vec![row("glyphs"), row("artwork")], never_swept)
            .expect("first report admitted");
        registry
            .report_cache_ledgers(&caller, vec![row("cursors")], never_swept)
            .expect("second report admitted");
        let rows = registry.cache_rows();
        assert_eq!(rows.len(), 1);
        assert_eq!(rows[0].label(), "cursors");
    }

    #[test]
    fn empty_report_withdraws_the_reporter() {
        let mut registry = SelfReports::new(1 << 30);
        let caller = user_caller(&[], 1, 42);
        registry
            .report_cache_ledgers(&caller, vec![row("glyphs")], never_swept)
            .expect("first report admitted");
        registry
            .report_cache_ledgers(&caller, vec![], never_swept)
            .expect("empty report withdraws");
        assert!(registry.cache_rows().is_empty());
    }

    #[test]
    fn retain_live_drops_dead_reporters_by_proc_id() {
        let mut registry = SelfReports::new(1 << 30);
        let gone = user_caller(&[], 1, 42);
        let staying = user_caller(&[], 2, 43);
        registry
            .report_cache_ledgers(&gone, vec![row("glyphs")], never_swept)
            .expect("admitted");
        registry
            .report_cache_ledgers(&staying, vec![row("artwork")], never_swept)
            .expect("admitted");

        registry.retain_live(&[staying.origin().proc_id()]);

        let rows = registry.cache_rows();
        assert_eq!(rows.len(), 1);
        assert_eq!(rows[0].label(), "artwork");
    }

    #[test]
    fn recycled_pid_does_not_inherit_a_dead_reporters_rows() {
        let mut registry = SelfReports::new(1 << 30);
        let original = user_caller(&[], 1, 99);
        registry
            .report_cache_ledgers(&original, vec![row("glyphs")], never_swept)
            .expect("admitted");

        // The instance is gone; its pid 99 is recycled to an unrelated
        // process with a fresh ProcId.
        registry.retain_live(&[]);
        let recycled = user_caller(&[], 9, 99);
        registry
            .report_cache_ledgers(&recycled, vec![row("cursors")], never_swept)
            .expect("admitted as a new reporter");

        let rows = registry.cache_rows();
        assert_eq!(rows.len(), 1);
        assert_eq!(rows[0].label(), "cursors");
    }

    /// Fill a registry to exactly its capacity, answering the callers that
    /// hold it so a test can state which of them is still alive.
    fn filled_to_capacity() -> (SelfReports, alloc::vec::Vec<ProcId>) {
        let floor = u8::try_from(MIN_REPORTERS).expect("the reporter floor fits a fixture tag");
        let mut registry = SelfReports::new(RAM_BYTES_PER_REPORTER * u64::from(floor));
        let mut admitted = alloc::vec::Vec::new();
        for tag in 0..floor {
            let caller = user_caller(&[], tag, u64::from(tag));
            registry
                .report_cache_ledgers(&caller, vec![row("glyphs")], never_swept)
                .expect("admitted within capacity");
            admitted.push(caller.origin().proc_id());
        }
        (registry, admitted)
    }

    #[test]
    fn full_registry_refuses_a_new_reporter_without_evicting_live_ones() {
        let (mut registry, admitted) = filled_to_capacity();
        let overflow = user_caller(&[], 200, 200);
        assert_eq!(
            registry.report_cache_ledgers(&overflow, vec![row("glyphs")], || Ok(admitted.clone())),
            Err(Errno::NoSpace)
        );
        assert_eq!(registry.cache_rows().len(), MIN_REPORTERS);
    }

    /// The sweep's whole purpose, and the only path that pays for it: a full
    /// table whose reporters have since exited admits a new one.
    #[test]
    fn a_full_registry_sweeps_its_dead_reporters_to_admit_a_new_one() {
        let (mut registry, admitted) = filled_to_capacity();
        let survivor = admitted[0];
        let newcomer = user_caller(&[], 200, 200);
        registry
            .report_cache_ledgers(&newcomer, vec![row("cursors")], || Ok(vec![survivor]))
            .expect("the dead reporters' slots are freed for it");

        let rows = registry.cache_rows();
        let labels: alloc::vec::Vec<&str> = rows.iter().map(CacheLedgerRecord::label).collect();
        assert_eq!(labels, ["glyphs", "cursors"], "only the live one survived");
    }

    /// An already-admitted reporter restating its figures is the submission
    /// this table sees most, and it must cost nothing beyond the replacement
    /// — `never_swept` panics if the process table is enumerated.
    #[test]
    fn a_resubmission_into_a_full_registry_never_sweeps() {
        let (mut registry, _) = filled_to_capacity();
        let established = user_caller(&[], 0, 0);
        registry
            .report_cache_ledgers(&established, vec![row("artwork")], never_swept)
            .expect("an established reporter needs no room");
    }

    #[test]
    fn rows_are_ordered_by_reporter_then_label() {
        let mut registry = SelfReports::new(1 << 30);
        let first = user_caller(&[], 1, 1);
        let second = user_caller(&[], 2, 2);
        registry
            .report_cache_ledgers(&second, vec![row("zeta"), row("alpha")], never_swept)
            .expect("admitted");
        registry
            .report_cache_ledgers(&first, vec![row("beta")], never_swept)
            .expect("admitted");

        let rows = registry.cache_rows();
        let labels: alloc::vec::Vec<&str> = rows.iter().map(CacheLedgerRecord::label).collect();
        assert_eq!(labels, ["beta", "alpha", "zeta"]);
    }
}
