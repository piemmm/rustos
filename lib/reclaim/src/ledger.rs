//! The diagnostics view of one cache: who holds it, what class it is, and
//! a shared handle to the figures it keeps (`plans/SMARTRAM.md` SMART1).
//!
//! The reclaim model is deliberately two-sided: the kernel's block,
//! filesystem, launch, and transform caches and a desktop process's glyph
//! atlases, decoded icon artwork, and rasterised chrome all classify the
//! same way and shrink on the same bands. Only the kernel's side can be
//! measured from outside a process, so the userland side has to *say* what
//! it holds. Both sides describe a cache with the same three facts plus its
//! ledger, and that description is [`CacheLedger`].
//!
//! It lives here, beside the model that defines a class and an owner,
//! because the kernel's statistics registry and the userland runtime's
//! reporter both build one and neither may depend on the other. The
//! conversion to the wire record lives here too, so a kernel row and a
//! reported row can never be spelled differently.

use alloc::sync::Arc;

use tairix_abi::sysinfo::{CacheLedgerRecord, CacheOwnerKind};
use tairix_abi::Errno;

use crate::model::{CacheAccounting, ReclaimClass, ReclaimOwner};

impl ReclaimOwner {
    /// The owner's wire form: its kind, and the numeric payload the kinds
    /// that carry one put in `owner_id`.
    ///
    /// The string payload of the two named kinds is deliberately dropped:
    /// the cache's own label already names the holder more precisely than
    /// its subsystem or process name does, and one label beats two.
    #[must_use]
    pub const fn wire(self) -> (CacheOwnerKind, u64) {
        match self {
            Self::KernelSubsystem(_) => (CacheOwnerKind::KernelSubsystem, 0),
            Self::FilesystemVolume { volume } => (CacheOwnerKind::FilesystemVolume, volume),
            Self::Task { task } => (CacheOwnerKind::Task, task),
            Self::DesktopSession { seat } => (CacheOwnerKind::DesktopSession, seat),
            Self::UserlandProcess(_) => (CacheOwnerKind::UserlandProcess, 0),
        }
    }
}

/// One cache's identity plus a shared, read-only handle to its ledger.
///
/// Cloning is cheap and shares the ledger: a registry holds a clone while
/// the owning cache keeps mutating the counters, exactly as
/// [`CacheAccounting`] is designed for.
#[derive(Clone)]
pub struct CacheLedger {
    label: &'static str,
    owner: ReclaimOwner,
    class: ReclaimClass,
    accounting: Arc<CacheAccounting>,
}

impl core::fmt::Debug for CacheLedger {
    /// The identity and the resident total, never the entries: what a
    /// cache retains is user data, and a ledger's job is to describe the
    /// cache rather than reveal its contents.
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        f.debug_struct("CacheLedger")
            .field("label", &self.label)
            .field("owner", &self.owner)
            .field("class", &self.class)
            .field("resident_bytes", &self.accounting.class_bytes(self.class))
            .finish()
    }
}

impl CacheLedger {
    /// Describe a cache by its label, owner, class, and shared ledger.
    #[must_use]
    pub const fn new(
        label: &'static str,
        owner: ReclaimOwner,
        class: ReclaimClass,
        accounting: Arc<CacheAccounting>,
    ) -> Self {
        Self {
            label,
            owner,
            class,
            accounting,
        }
    }

    /// The cache's stable label.
    #[must_use]
    pub const fn label(&self) -> &'static str {
        self.label
    }

    /// Who is charged for the cache's memory.
    #[must_use]
    pub const fn owner(&self) -> ReclaimOwner {
        self.owner
    }

    /// The reclaim class of every entry in the cache.
    #[must_use]
    pub const fn class(&self) -> ReclaimClass {
        self.class
    }

    /// The shared ledger, for a registry that samples it.
    #[must_use]
    pub fn accounting(&self) -> &Arc<CacheAccounting> {
        &self.accounting
    }

    /// Sample the ledger into the wire record the System Information API
    /// carries.
    ///
    /// The sample is lock-free and per-field, so a record may straddle an
    /// in-flight mutation; each figure is individually untorn, which is the
    /// sampling semantics every live gauge has. The record's origin is left
    /// unset — whoever publishes it stamps that, so a process cannot
    /// present its own figures as measured ones.
    ///
    /// # Errors
    ///
    /// [`Errno::LengthOutOfRange`] or [`Errno::OutOfRange`] if the label is
    /// empty, longer than the wire record admits, or not printable ASCII.
    /// A cache with an unrenderable label is a defect in the crate that
    /// built it, and it is refused here rather than shown as a broken row.
    pub fn to_record(&self) -> Result<CacheLedgerRecord, Errno> {
        let (owner_kind, owner_id) = self.owner.wire();
        let class = u8::try_from(self.class.index()).map_err(|_| Errno::OutOfRange)?;
        let mut record =
            CacheLedgerRecord::new(self.label.as_bytes(), owner_kind, owner_id, class)?;
        let stats = self.accounting.class_stats(self.class);
        record.payload_bytes = stats.payload_bytes;
        record.metadata_bytes = stats.metadata_bytes;
        record.entries = stats.entries;
        record.refusals = stats.refusals;
        record.pressure_shrinks = stats.pressure_shrinks;
        record.teardowns = stats.teardowns;
        record.failures = stats.failures;
        record.hits = stats.hits;
        record.misses = stats.misses;
        Ok(record)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use tairix_abi::sysinfo::CacheLedgerOrigin;

    fn ledger(label: &'static str, owner: ReclaimOwner, class: ReclaimClass) -> CacheLedger {
        CacheLedger::new(label, owner, class, Arc::new(CacheAccounting::new()))
    }

    #[test]
    fn every_owner_kind_has_a_wire_form() {
        assert_eq!(
            ReclaimOwner::KernelSubsystem("mem").wire(),
            (CacheOwnerKind::KernelSubsystem, 0)
        );
        assert_eq!(
            ReclaimOwner::FilesystemVolume { volume: 7 }.wire(),
            (CacheOwnerKind::FilesystemVolume, 7)
        );
        assert_eq!(
            ReclaimOwner::Task { task: 12 }.wire(),
            (CacheOwnerKind::Task, 12)
        );
        assert_eq!(
            ReclaimOwner::DesktopSession { seat: 1 }.wire(),
            (CacheOwnerKind::DesktopSession, 1)
        );
        assert_eq!(
            ReclaimOwner::UserlandProcess("fontd").wire(),
            (CacheOwnerKind::UserlandProcess, 0)
        );
    }

    #[test]
    fn a_record_carries_the_identity_and_the_live_figures() {
        let entry = ledger(
            "fontd.glyph-raster",
            ReclaimOwner::UserlandProcess("fontd"),
            ReclaimClass::DisposableUi,
        );
        entry
            .accounting()
            .charge(ReclaimClass::DisposableUi, 4096, 256)
            .expect("a fresh ledger accepts a charge");
        entry.accounting().record_hit(ReclaimClass::DisposableUi);
        entry.accounting().record_miss(ReclaimClass::DisposableUi);

        let record = entry.to_record().expect("a printable label encodes");
        assert_eq!(record.label(), "fontd.glyph-raster");
        assert_eq!(record.owner_kind, CacheOwnerKind::UserlandProcess);
        assert_eq!(record.owner_id, 0);
        assert_eq!(
            usize::from(record.class),
            ReclaimClass::DisposableUi.index()
        );
        assert_eq!(record.payload_bytes, 4096);
        assert_eq!(record.metadata_bytes, 256);
        assert_eq!(record.entries, 1);
        assert_eq!(record.hits, 1);
        assert_eq!(record.misses, 1);
    }

    #[test]
    fn a_sampled_record_never_claims_to_be_measured() {
        // The publisher stamps the origin from an attested identity; a
        // sample taken here must not pre-empt that, or a process could
        // present its own figures as the kernel's.
        let record = ledger(
            "wm.cursor",
            ReclaimOwner::DesktopSession { seat: 1 },
            ReclaimClass::DisposableUi,
        )
        .to_record()
        .expect("a printable label encodes");
        assert_eq!(record.origin, CacheLedgerOrigin::Unset);
        assert_eq!(record.reporter_pid, 0);
    }

    #[test]
    fn an_unrenderable_label_is_refused_rather_than_shown_broken() {
        let record = ledger(
            "wm\u{1b}[2Jcursor",
            ReclaimOwner::DesktopSession { seat: 1 },
            ReclaimClass::DisposableUi,
        )
        .to_record();
        assert_eq!(record, Err(Errno::OutOfRange));
    }
}
