//! Offline check and rescue (`docs/src/filesystem/arxfs-spec.md` §12, §15.9).
//!
//! These are the recovery operations the online [`crate::ARXFS::scrub`]
//! (Stage 8) deliberately does *not* attempt. Both lean on the seams the
//! earlier stages built (the block identity + companion mirror, the
//! [`crate::integrity::DataFault`] classes, the chunk/reverse-reference trees,
//! and the free-space / dedupe-index rebuilds), so the verification core is
//! shared rather than written twice.
//!
//! * **`check`** is the *offline structural validator/rebuilder*. It runs on a
//!   mounted handle and is the **superset** of the online scrub's checks plus
//!   structural rebuild: it verifies and repairs metadata copies, data
//!   integrity, and refcounts (reusing scrub's [`crate::ARXFS::verify_everything`]),
//!   **rebuilds** the rebuildable derived state — the free-space bitmap and the
//!   dedupe index — from the authoritative metadata so a corrupt derivation can
//!   never keep a sound volume unmountable, validates directories, and
//!   detects and reclaims **orphaned inodes**. It reports what it could not
//!   safely fix and never panics.
//! * **`rescue`** is the *damaged-volume extractor*. Without requiring a
//!   mountable filesystem it recovers the volume keys from a surviving
//!   superblock discovery header, **scans** the device for self-identifying
//!   transaction roots, picks the highest-generation valid root, maps the
//!   inode/extent metadata to files, and **extracts** the readable file data —
//!   running every recovered block through the Stage 5/6 integrity pipeline so
//!   only blocks that pass are emitted. It is read-only on the damaged volume.
//!
//! Both privileged operations are capability-gated on
//! [`CapabilityId::FS_MOUNT`] (refused fail-closed and logged otherwise) and
//! log their outcome through `lib/log` with stable event IDs in the reserved
//! `arxfs` `12000..13000` range (`crate::scrub`).

use tairix_abi::driver::block::Block;
use tairix_abi::{CapabilityId, CapabilityQuery, DriverError};
use tairix_log::{log, Event, EventId, Level, Sink};

use crate::btree::TreeWalk;
use crate::header::ReservedOwner;
use crate::scratch::{ElementWidth, ScratchArray};
use crate::scrub::{ScrubReport, ARXFS_RANGE_END, ARXFS_RANGE_START};
use crate::superblock;
use crate::transaction::TxnRoot;
use crate::{
    as_usize, extent_spec, inode_spec, DirScan, Extent, Inode, InodeKind, ARXFS, MAX_BLOCK_SIZE,
    ROOT_INO,
};

/// An offline `check` finished and the structure validated clean.
pub const CHECK_CLEAN: EventId = EventId(12_010);
/// An offline `check` finished after detecting (and where safe, repairing or
/// reclaiming) structural findings.
pub const CHECK_REPAIRED: EventId = EventId(12_011);
/// An offline `check` finished with findings it could not safely repair.
pub const CHECK_UNRECOVERABLE: EventId = EventId(12_012);
/// An offline `check` was refused because the caller lacks `CAP_FS_MOUNT`.
pub const CHECK_DENIED: EventId = EventId(12_013);
/// A `rescue` pass completed, having discovered a root and extracted files.
pub const RESCUE_COMPLETE: EventId = EventId(12_020);
/// A `rescue` pass found no valid root to extract from.
pub const RESCUE_NO_ROOT: EventId = EventId(12_021);
/// A `rescue` was refused because the caller lacks `CAP_FS_MOUNT`.
pub const RESCUE_DENIED: EventId = EventId(12_022);

/// Every check/rescue event identifier falls inside the reserved `arxfs`
/// range so the stable IDs external audit-log consumers rely on never collide
/// with another subsystem's range.
const _: () = {
    assert!(CHECK_CLEAN.0 >= ARXFS_RANGE_START && CHECK_CLEAN.0 < ARXFS_RANGE_END);
    assert!(CHECK_REPAIRED.0 >= ARXFS_RANGE_START && CHECK_REPAIRED.0 < ARXFS_RANGE_END);
    assert!(CHECK_UNRECOVERABLE.0 >= ARXFS_RANGE_START && CHECK_UNRECOVERABLE.0 < ARXFS_RANGE_END);
    assert!(CHECK_DENIED.0 >= ARXFS_RANGE_START && CHECK_DENIED.0 < ARXFS_RANGE_END);
    assert!(RESCUE_COMPLETE.0 >= ARXFS_RANGE_START && RESCUE_COMPLETE.0 < ARXFS_RANGE_END);
    assert!(RESCUE_NO_ROOT.0 >= ARXFS_RANGE_START && RESCUE_NO_ROOT.0 < ARXFS_RANGE_END);
    assert!(RESCUE_DENIED.0 >= ARXFS_RANGE_START && RESCUE_DENIED.0 < ARXFS_RANGE_END);
};

/// The structured outcome of an offline [`ARXFS::check`]
/// (`docs/src/filesystem/arxfs-spec.md` §12). It carries the shared
/// verification core's [`ScrubReport`] plus the structural validation and
/// rebuild that `check` performs on top of it.
#[derive(Copy, Clone, Debug, Default, Eq, PartialEq)]
pub struct CheckReport {
    /// `true` once the whole offline pass has run.
    pub complete: bool,
    /// What the pass can say about the volume's structure.
    pub structure: StructureVerdict,
    /// The metadata / data / refcount verification (and copy-repair / refcount
    /// reconcile) shared with the online scrub.
    pub verification: ScrubReport,
    /// Directories reached from the root and validated.
    pub directories_checked: u64,
    /// Directory entries pointing at an inode that does not exist: a finding
    /// `check` reports rather than auto-deleting (removing a live name is not a
    /// safe automatic repair).
    pub dangling_entries: u64,
    /// Live inodes not reachable from the root through any directory: orphans.
    pub orphaned_inodes: u64,
    /// Orphaned inodes `check` reclaimed (freeing their blocks and inode slot).
    pub orphans_reclaimed: u64,
    /// `true` once the rebuildable derived state — the free-space bitmap
    /// and the in-memory dedupe index — was rebuilt from the
    /// authoritative trees, so a corrupt derivation can never keep a sound
    /// volume unmountable. A completed check always rebuilds both.
    pub rebuilt_derived_state: bool,
    /// Findings `check` could not safely repair (unrepairable metadata, data
    /// integrity faults, dangling directory entries, and refcount/reverse-ref
    /// divergences it could not reconcile).
    pub unrecoverable_findings: u64,
    /// Incompatible-feature bits `check` had to add to the superblock because
    /// the volume held the structure a bit names without declaring it.
    /// Widening the declared set is always safe: it can only make a reader
    /// that lacks the feature refuse the volume, never misread it.
    pub features_declared: u32,
    /// Inodes whose stored name count disagreed with the number of directory
    /// entries that actually name them, and which `check` rewrote to the
    /// counted truth.
    ///
    /// The count decides when an unlink frees an inode's blocks, so a stored
    /// value above the truth leaks storage no name reaches and one below it
    /// frees storage a live name still does. Both are repaired by counting.
    pub link_counts_corrected: u64,
}

/// What an offline [`ARXFS::check`] can say about the volume's structure.
///
/// The distinction is not cosmetic: the directory-reachability, orphan, and
/// name-count passes each need a transient scratch array over the inode space
/// (`docs/src/filesystem/arxfs-spec.md` §12), because the truth they derive is
/// one value per inode
/// and that cannot be held in RAM on the machine a very large volume has to be
/// served from. When the volume can spare no run for one, the pass has walked
/// nothing — and must say so rather than report a soundness no walk
/// established.
#[derive(Copy, Clone, Debug, Default, Eq, PartialEq)]
pub enum StructureVerdict {
    /// Nothing walked the structure, so nothing vouches for it either way. The
    /// verification, rebuild, and feature checks still ran.
    #[default]
    NotWalked,
    /// Walked, and every finding was repaired or was benign.
    Sound,
    /// Walked, and findings remain that the pass could not safely repair.
    Unsound,
}

impl CheckReport {
    /// Whether the pass repaired or reclaimed anything (so a clean check, which
    /// changes nothing on disk, is distinguishable from one that did work).
    #[must_use]
    pub fn made_repairs(&self) -> bool {
        self.verification.metadata_repaired != 0
            || self.verification.divergences_corrected != 0
            || self.orphans_reclaimed != 0
            || self.features_declared != 0
            || self.link_counts_corrected != 0
    }

    /// Emit the closing check event to `sink` (log
    /// security-relevant findings with a stable event ID).
    fn log_outcome(&self, sink: &dyn Sink) {
        let (id, level, message) = if self.unrecoverable_findings != 0 {
            (
                CHECK_UNRECOVERABLE,
                Level::Warn,
                "arxfs check found unrecoverable structural findings",
            )
        } else if self.made_repairs() {
            (
                CHECK_REPAIRED,
                Level::Info,
                "arxfs check repaired structural findings",
            )
        } else {
            (CHECK_CLEAN, Level::Info, "arxfs check clean")
        };
        log(
            sink,
            &Event {
                level,
                id,
                message,
                fields: &[],
            },
        );
    }
}

/// The structured outcome of a [`ARXFS::rescue`] pass
/// (`docs/src/filesystem/arxfs-spec.md` §12).
#[derive(Copy, Clone, Debug, Default, Eq, PartialEq)]
pub struct RescueReport {
    /// Valid self-identifying transaction roots discovered by the device scan.
    pub roots_found: u64,
    /// Generation of the highest-generation valid root extraction ran from
    /// (`0` when none was found).
    pub best_generation: u64,
    /// File inodes whose metadata was recovered and walked.
    pub files_mapped: u64,
    /// File-data blocks that passed the Stage 5/6 integrity pipeline and were
    /// emitted to the sink.
    pub blocks_extracted: u64,
    /// File-data blocks skipped because they failed the integrity pipeline
    /// (never emitted — a corrupt block is never handed back).
    pub blocks_skipped: u64,
    /// Inodes whose metadata (extent tree) could not be read at all.
    pub unreadable_inodes: u64,
    /// Symbolic links the walk reached and deliberately did not extract: the
    /// sink carries file bytes, so a link's target has nowhere to go that
    /// would not silently turn it into a regular file.
    pub links_skipped: u64,
}

impl RescueReport {
    /// Whether a valid root was found to extract from.
    #[must_use]
    pub fn found_root(&self) -> bool {
        self.roots_found != 0
    }

    /// Emit the closing rescue event to `sink`.
    fn log_outcome(&self, sink: &dyn Sink) {
        let (id, level, message) = if self.found_root() {
            (
                RESCUE_COMPLETE,
                Level::Info,
                "arxfs rescue extracted readable files from a recovered root",
            )
        } else {
            (
                RESCUE_NO_ROOT,
                Level::Warn,
                "arxfs rescue found no valid root",
            )
        };
        log(
            sink,
            &Event {
                level,
                id,
                message,
                fields: &[],
            },
        );
    }
}

/// Where [`ARXFS::rescue`] delivers the file data it recovers. The driver
/// owns no destination filesystem, so extraction streams each integrity-checked
/// plaintext block to a caller-supplied sink (a recovery host writes it to a
/// safe volume). Only blocks that pass the Stage 5/6 integrity pipeline are
/// ever delivered (`docs/src/filesystem/arxfs-spec.md` §6, §12).
pub trait RescueSink {
    /// Receive one recovered, integrity-verified plaintext data block.
    ///
    /// * `inode` — the file inode number the block belongs to.
    /// * `logical_block` — the block's logical offset within that file.
    /// * `size` — the file's recorded size in bytes, so the caller can trim the
    ///   final block (a logical block always carries a full block of plaintext
    ///   even when the file ends mid-block).
    /// * `data` — the recovered plaintext (one logical block).
    fn emit_block(&mut self, inode: u32, logical_block: u64, size: u64, data: &[u8]);
}

impl<B: Block> ARXFS<B> {
    /// Run an offline `check`: the structural validation, repair, and
    /// rebuild the online scrub does not attempt
    /// (`docs/src/filesystem/arxfs-spec.md` §12).
    ///
    /// `check` runs on a **mounted handle**: a volume that opens is the input,
    /// and `check` is the offline superset of [`Self::scrub`]'s checks plus
    /// structural rebuild. It verifies and repairs metadata copies, data
    /// integrity, and refcounts (reusing the shared verification core),
    /// **rebuilds** the rebuildable derived state (the free-space bitmap and the
    /// dedupe index) from the authoritative metadata so a corrupt derivation can
    /// never keep a sound volume unmountable, validates the directory tree,
    /// and detects and reclaims orphaned inodes. It reports what it could not
    /// safely fix. A clean check changes nothing on disk and is idempotent.
    ///
    /// # Errors
    ///
    /// * [`DriverError::PermissionDenied`] if `caps` does not grant
    ///   [`CapabilityId::FS_MOUNT`] (fail-closed).
    /// * [`DriverError::DeviceFault`] / [`DriverError::NoSpace`] on an
    ///   unrecoverable device error while validating or persisting a repair
    ///   (never a panic).
    ///
    /// # Capabilities
    ///
    /// Requires [`CapabilityId::FS_MOUNT`].
    pub fn check(
        &mut self,
        caps: &dyn CapabilityQuery,
        sink: &dyn Sink,
    ) -> Result<CheckReport, DriverError> {
        if !caps.holds(CapabilityId::FS_MOUNT) {
            log(
                sink,
                &Event {
                    level: Level::Warn,
                    id: CHECK_DENIED,
                    message: "arxfs check denied: missing CAP_FS_MOUNT",
                    fields: &[],
                },
            );
            return Err(DriverError::PermissionDenied);
        }
        self.begin()?;
        match self.check_run() {
            Ok((report, mutated)) => {
                if mutated {
                    self.commit()?;
                } else {
                    self.finish_unpublished();
                }
                report.log_outcome(sink);
                Ok(report)
            }
            Err(err) => {
                self.rollback();
                Err(err)
            }
        }
    }

    /// The body of a check pass. Returns the report and whether it mutated
    /// on-disk tree state (so the caller commits only then, keeping a clean
    /// check idempotent).
    fn check_run(&mut self) -> Result<(CheckReport, bool), DriverError> {
        // Every pass that needs a scratch array clears this if it could not
        // place one, so the report never claims a structure nothing walked.
        let mut report = CheckReport::default();
        // Cleared by any pass that could not place the scratch array its
        // per-inode state needs, so the report never claims a structure
        // nothing walked.
        let mut walked = true;

        // Rebuild the rebuildable derived state first, from the authoritative
        // trees, before any allocation: a corrupt free-space derivation must
        // never keep a sound volume unmountable.
        self.rebuild_free_space()?;
        report.rebuilt_derived_state = true;

        // Reuse the online scrub's verification core: metadata both-copies
        // verify/repair, data integrity classification, and refcount /
        // reverse-reference reconcile against the live extents.
        let refcounts_corrected = self.verify_everything(&mut report.verification)?;

        // Structural validation scrub does not do: walk the directory tree from
        // the root, validate every entry's target, and reclaim orphaned inodes.
        let reclaimed = self.check_directories_and_orphans(&mut report, &mut walked)?;

        // The feature word must cover what the volume actually holds, or a
        // reader lacking the feature would mount and misread instead of
        // refusing.
        report.features_declared = self.check_declared_features()?;

        // The stored name counts must match the names that exist, or an
        // unlink frees storage too early or never at all.
        report.link_counts_corrected = self.check_link_counts(&mut walked)?;

        let v = &report.verification;
        let divergences = v.refcount_divergences + v.reverse_ref_divergences;
        report.unrecoverable_findings = v.metadata_unrepairable
            + v.metadata_damaged
            + v.data_physical_faults
            + v.data_aead_faults
            + v.data_logical_faults
            + report.dangling_entries
            + divergences.saturating_sub(v.divergences_corrected);
        report.structure = match (walked, report.unrecoverable_findings) {
            (false, _) => StructureVerdict::NotWalked,
            (true, 0) => StructureVerdict::Sound,
            (true, _) => StructureVerdict::Unsound,
        };
        report.complete = true;

        Ok((
            report,
            refcounts_corrected
                || reclaimed
                || report.features_declared != 0
                || report.link_counts_corrected != 0,
        ))
    }

    /// Confirm the superblock's incompatible-feature word covers the
    /// structure the volume really holds, widening it when it does not and
    /// returning how many bits were added.
    ///
    /// Today the only such structure is a symbolic link. The write path sets
    /// the bit in the very transaction that mints the first link, and both
    /// the inode tree and the superblock are sealed under the same key, so a
    /// volume can only reach here understating itself through a driver
    /// defect — which is exactly what an offline structural check is for.
    /// Widening fails safe in one direction only: a reader without the
    /// feature then refuses the volume rather than reading a link as a
    /// corrupt inode.
    fn check_declared_features(&mut self) -> Result<u32, DriverError> {
        if self.incompat & superblock::INCOMPAT_SYMLINKS != 0 {
            return Ok(0);
        }
        let spec = inode_spec();
        let mut walk = TreeWalk::new(self.block_size)?;
        while self.btree_next_leaf(self.inode_tree_root, spec, &mut walk)? {
            for (_, value) in walk.entries() {
                let Some(inode) = Inode::decode(value)? else {
                    continue;
                };
                if inode.kind == InodeKind::Link {
                    self.incompat |= superblock::INCOMPAT_SYMLINKS;
                    return Ok(1);
                }
            }
        }
        Ok(0)
    }

    /// Reconcile every inode's stored name count against the directory
    /// entries that really name it, rewriting the ones that disagree and
    /// returning how many were corrected.
    ///
    /// The invariant is uniform across kinds, because `.` and `..` are real
    /// entries here: an inode's count is simply **how many directory entries
    /// name it**. A file or symbolic link with two names counts two; an empty
    /// directory counts two (its parent's entry and its own `.`) and gains
    /// one per child directory (each child's `..`); the root counts its own
    /// `.` and `..` plus one per child directory.
    ///
    /// This is the hard-link analogue of the feature-word widening above.
    /// The count decides when an unlink frees an inode's blocks, so a stored
    /// value that has drifted either leaks storage nothing reaches or frees
    /// storage a live name still does — the second being the corruption the
    /// feature bit exists to keep an unaware reader from causing.
    ///
    /// The counts live in a transient scratch array over the inode space, not
    /// in RAM: one per inode is proportional to the volume, and a 100 TB
    /// volume has to be checkable from a machine that cannot hold that.
    fn check_link_counts(&mut self, walked: &mut bool) -> Result<u64, DriverError> {
        let Some(mut names) = self.scratch_alloc(
            ReservedOwner::ScratchNames,
            ElementWidth::Word,
            self.next_ino,
            1,
        )?
        else {
            *walked = false;
            return Ok(0);
        };
        let outcome = self.count_and_fix_link_counts(&mut names);
        let released = self.scratch_release(names);
        if outcome.is_ok() {
            released?;
        }
        outcome
    }

    /// [`Self::check_link_counts`] with its scratch array in hand.
    fn count_and_fix_link_counts(&mut self, names: &mut ScratchArray) -> Result<u64, DriverError> {
        let spec = inode_spec();
        let mut walk = TreeWalk::new(self.block_size)?;
        let mut scan = DirScan::new(self.block_size)?;
        while self.btree_next_leaf(self.inode_tree_root, spec, &mut walk)? {
            for (_, value) in walk.entries() {
                let Some(inode) = Inode::decode(value)? else {
                    continue;
                };
                if !inode.is_dir() {
                    continue;
                }
                scan.seek(0);
                while let Some((_, child)) = self.dir_next(&inode, &mut scan)? {
                    self.scratch_bump(names, u64::from(child))?;
                }
            }
        }
        let mut corrected = 0;
        walk.restart();
        while self.btree_next_leaf(self.inode_tree_root, spec, &mut walk)? {
            // Rewriting an inode copies-on-writes the tree the walk is
            // reading, so one correction per step and then resume past it.
            let mut wrong = None;
            for (ino_key, value) in walk.entries() {
                let Some(inode) = Inode::decode(value)? else {
                    continue;
                };
                let ino = u32::try_from(ino_key).map_err(|_| DriverError::DeviceFault)?;
                let counted = self.scratch_get(names, ino_key)?;
                // An inode no entry names is an orphan, already reclaimed
                // above; rewriting its count here would only fight that
                // repair.
                if counted == 0 || inode.nlink == counted {
                    continue;
                }
                wrong = Some((
                    ino_key,
                    ino,
                    Inode {
                        nlink: counted,
                        ..inode
                    },
                ));
                break;
            }
            let Some((key, ino, inode)) = wrong else {
                continue;
            };
            self.write_inode(ino, &inode)?;
            corrected += 1;
            match key.checked_add(1) {
                Some(next) => walk.seek(next),
                None => walk.stop(),
            }
        }
        Ok(corrected)
    }

    /// Walk the directory tree from the root, validate every entry's target
    /// exists, and detect and reclaim orphaned inodes — live inodes the
    /// directory tree no longer reaches (`docs/src/filesystem/arxfs-spec.md`
    /// §12). Reclaiming frees the orphan's data blocks (releasing any shared
    /// chunk references) and its inode slot. Returns whether anything was
    /// reclaimed (so the caller commits). An entry pointing at a missing inode
    /// is a *dangling* finding: it is reported, not auto-deleted, since removing
    /// a live name is not a safe automatic repair.
    ///
    /// Which inodes are reachable, and which directories are still owed an
    /// expansion, both live in transient scratch arrays over the inode space
    /// ([`crate::scratch`]) rather than in RAM: a set holding every reachable
    /// inode and a stack holding every directory are each proportional to the
    /// volume, and the machine a 100 TB volume must be checkable from cannot
    /// hold either. Each directory enters the frontier once — the reachable
    /// bit is what guarantees it — so the queue can never outgrow the inode
    /// space it is sized for, and a directory cycle terminates instead of
    /// looping.
    fn check_directories_and_orphans(
        &mut self,
        report: &mut CheckReport,
        walked: &mut bool,
    ) -> Result<bool, DriverError> {
        let Some(mut reachable) = self.scratch_alloc(
            ReservedOwner::ScratchReachable,
            ElementWidth::Bit,
            self.next_ino,
            1,
        )?
        else {
            *walked = false;
            return Ok(false);
        };
        let frontier = self.scratch_alloc(
            ReservedOwner::ScratchFrontier,
            ElementWidth::Word,
            self.next_ino,
            1,
        );
        let mut frontier = match frontier {
            Ok(Some(frontier)) => frontier,
            Ok(None) => {
                *walked = false;
                self.scratch_release(reachable)?;
                return Ok(false);
            }
            Err(err) => {
                let _ = self.scratch_release(reachable);
                return Err(err);
            }
        };
        let outcome = self.walk_directories(&mut reachable, &mut frontier, report);
        let released = self
            .scratch_release(frontier)
            .and(self.scratch_release(reachable));
        if outcome.is_ok() {
            released?;
        }
        outcome
    }

    /// [`Self::check_directories_and_orphans`] with its scratch arrays in hand.
    fn walk_directories(
        &mut self,
        reachable: &mut ScratchArray,
        frontier: &mut ScratchArray,
        report: &mut CheckReport,
    ) -> Result<bool, DriverError> {
        let mut scan = DirScan::new(self.block_size)?;
        self.scratch_set(reachable, u64::from(ROOT_INO), 1)?;
        self.scratch_set(frontier, 0, ROOT_INO)?;
        let mut head = 0;
        let mut tail = 1;
        while head < tail {
            let dir_ino = self.scratch_get(frontier, head)?;
            head += 1;
            let dir = self.read_inode(dir_ino)?;
            if !dir.is_dir() {
                continue;
            }
            report.directories_checked += 1;
            scan.seek(0);
            while let Some((_, child)) = self.dir_next(&dir, &mut scan)? {
                if scan.is_dot() {
                    continue;
                }
                if !self.inode_exists(child)? {
                    report.dangling_entries += 1;
                    continue;
                }
                if u64::from(child) >= self.next_ino {
                    // A live inode above the high-water mark the committed
                    // root records: the reachability array has no bit for it,
                    // so its own children would look like orphans and be
                    // reclaimed. Both the root and the inode tree are sealed
                    // under the same key, so this is a driver defect rather
                    // than a volume to repair — fail closed instead of
                    // guessing which of the two is wrong.
                    return Err(DriverError::DeviceFault);
                }
                if self.scratch_get(reachable, u64::from(child))? != 0 {
                    continue;
                }
                if tail >= self.next_ino {
                    // Every inode enters the frontier at most once and the
                    // queue is sized for the inode space, so this is
                    // unreachable — but dropping a directory here would make
                    // the orphan sweep reclaim a live subtree, so it fails
                    // closed rather than silently.
                    return Err(DriverError::DeviceFault);
                }
                self.scratch_set(reachable, u64::from(child), 1)?;
                self.scratch_set(frontier, tail, child)?;
                tail += 1;
            }
        }
        self.reclaim_orphans(reachable, report)
    }

    /// Reclaim every live inode the directory walk did not reach.
    ///
    /// Reclaiming one removes it from the tree being walked, so a step
    /// reclaims at most one and then resumes past it.
    fn reclaim_orphans(
        &mut self,
        reachable: &mut ScratchArray,
        report: &mut CheckReport,
    ) -> Result<bool, DriverError> {
        let spec = inode_spec();
        let mut walk = TreeWalk::new(self.block_size)?;
        while self.btree_next_leaf(self.inode_tree_root, spec, &mut walk)? {
            let mut orphan = None;
            for (ino_key, value) in walk.entries() {
                if Inode::decode(value)?.is_none() {
                    continue;
                }
                let ino = u32::try_from(ino_key).map_err(|_| DriverError::DeviceFault)?;
                if ino == ROOT_INO {
                    continue;
                }
                if ino_key >= self.next_ino {
                    // As above: an inode outside the allocated range makes the
                    // range untrustworthy, and reclaiming on the strength of
                    // it would free live storage.
                    return Err(DriverError::DeviceFault);
                }
                if self.scratch_get(reachable, ino_key)? != 0 {
                    continue;
                }
                orphan = Some((ino_key, ino));
                break;
            }
            let Some((key, ino)) = orphan else {
                continue;
            };
            report.orphaned_inodes += 1;
            let mut inode = self.read_inode(ino)?;
            self.free_all_blocks(&mut inode, ino)?;
            self.free_inode(ino)?;
            report.orphans_reclaimed += 1;
            match key.checked_add(1) {
                Some(next) => walk.seek(next),
                None => walk.stop(),
            }
        }
        Ok(report.orphans_reclaimed > 0)
    }

    /// Whether inode `ino` exists in the inode tree.
    fn inode_exists(&mut self, ino: u32) -> Result<bool, DriverError> {
        Ok(self
            .btree_get(self.inode_tree_root, u64::from(ino), inode_spec())?
            .and_then(|value| Inode::decode(&value).ok().flatten())
            .is_some())
    }

    /// Recover readable files from a volume too damaged to mount normally
    /// (`docs/src/filesystem/arxfs-spec.md` §12).
    ///
    /// `rescue` does not require a mountable filesystem. It recovers the volume
    /// keys from a surviving superblock discovery header (the wrapped master
    /// key, plaintext at rest), **scans** the whole device for self-identifying
    /// transaction roots whose inline commit record validates, picks the
    /// highest-generation valid root, walks the inode/extent metadata it names,
    /// and **extracts** the readable file data — running every recovered block
    /// through the Stage 5/6 integrity pipeline so only blocks that pass are
    /// emitted to `out` (a corrupt block is skipped, never handed back).
    ///
    /// It is **read-only** on the damaged volume: the repair-on-read paths are
    /// suppressed for the duration, so it never writes to the device.
    ///
    /// # Errors
    ///
    /// * [`DriverError::PermissionDenied`] if `caps` does not grant
    ///   [`CapabilityId::FS_MOUNT`] (fail-closed), or if a
    ///   structurally-valid superblock is present but `volume_key` does not
    ///   unwrap it (wrong key).
    /// * [`DriverError::Unsupported`] if the device block size is unsupported.
    /// * [`DriverError::BadMagic`] if the device is not a arxfs volume (no
    ///   superblock discovery header is present at all).
    /// * [`DriverError::DeviceFault`] on an unrecoverable low-level read while
    ///   scanning (never a panic).
    ///
    /// # Capabilities
    ///
    /// Requires [`CapabilityId::FS_MOUNT`].
    pub fn rescue(
        block: B,
        volume_key: &crate::VolumeKey,
        caps: &dyn CapabilityQuery,
        sink: &dyn Sink,
        out: &mut dyn RescueSink,
    ) -> Result<RescueReport, DriverError> {
        if !caps.holds(CapabilityId::FS_MOUNT) {
            log(
                sink,
                &Event {
                    level: Level::Warn,
                    id: RESCUE_DENIED,
                    message: "arxfs rescue denied: missing CAP_FS_MOUNT",
                    fields: &[],
                },
            );
            return Err(DriverError::PermissionDenied);
        }
        let mut fs = Self::bootstrap(block)?;
        fs.read_only = true;
        let mut buf = [0u8; MAX_BLOCK_SIZE];
        // Recover the key hierarchy from a surviving superblock discovery
        // header. This authenticates nothing yet (the discovery header is
        // plaintext); it fails closed on a wrong key or a non-arxfs device.
        fs.establish_keys(volume_key, &mut buf)?;

        let mut report = RescueReport::default();
        let Some(root) = fs.rescue_find_best_root(&mut report) else {
            report.log_outcome(sink);
            return Ok(report);
        };
        report.best_generation = root.generation;
        fs.inode_tree_root = root.inode_tree_root;
        fs.next_ino = root.next_ino;
        fs.chunk_tree_root = root.chunk_tree_root;
        fs.reverse_ref_tree_root = root.reverse_ref_tree_root;
        fs.generation = root.generation;
        fs.rescue_extract(out, &mut report)?;
        report.log_outcome(sink);
        Ok(report)
    }

    /// Scan every physical block for a self-identifying transaction root
    /// whose inline commit record validates, returning the highest-generation
    /// one (or `None` when the scan finds none). Counts every distinct valid
    /// root found. Read-only: a misdirected or torn block simply does not
    /// decode and is skipped.
    fn rescue_find_best_root(&mut self, report: &mut RescueReport) -> Option<TxnRoot> {
        let bs = self.block_size;
        let mut buf = [0u8; MAX_BLOCK_SIZE];
        let mut best: Option<TxnRoot> = None;
        for phys in 0..self.total_blocks {
            if self.read_block(phys, &mut buf).is_err() {
                continue;
            }
            if let Some(root) = TxnRoot::decode_any(&buf[..bs], self.fs_uuid, phys, &self.mac_key) {
                report.roots_found += 1;
                if best.is_none_or(|b| root.generation > b.generation) {
                    best = Some(root);
                }
            }
        }
        best
    }

    /// Walk the inode tree the recovered root names and emit every readable
    /// file-data block to `out`. A block that fails the Stage 5/6 integrity
    /// pipeline is skipped (counted, never emitted); an inode whose extent tree
    /// cannot be read at all is counted as unreadable and skipped. Damage in
    /// the inode tree ends the enumeration there and keeps every file the
    /// readable part of it already named, rather than failing the pass or
    /// discarding what it recovered.
    ///
    /// A symbolic link is **counted and skipped**, never extracted: the sink
    /// takes file bytes, so emitting a link's target through it would recreate
    /// the link on the destination as a regular file holding the target's
    /// text — a silent change of kind, which is worse than an honest omission
    /// (`docs/src/filesystem/arxfs-spec.md` §20).
    fn rescue_extract(
        &mut self,
        out: &mut dyn RescueSink,
        report: &mut RescueReport,
    ) -> Result<(), DriverError> {
        let cap = as_usize(self.data_capacity());
        let spec = inode_spec();
        let mut walk = TreeWalk::new(self.block_size)?;
        let mut extent_walk = TreeWalk::new(self.block_size)?;
        let mut buf = [0u8; MAX_BLOCK_SIZE];
        loop {
            match self.btree_next_leaf(self.inode_tree_root, spec, &mut walk) {
                Ok(true) => {}
                // Extraction keeps whatever the readable part of the tree
                // named: a damaged region ends the walk, it does not discard
                // the files already emitted.
                Ok(false) | Err(_) => return Ok(()),
            }
            for (ino_key, value) in walk.entries() {
                let Some(inode) = Inode::decode(value)? else {
                    continue;
                };
                match inode.kind {
                    InodeKind::File => {}
                    InodeKind::Dir => continue,
                    InodeKind::Link => {
                        report.links_skipped += 1;
                        continue;
                    }
                }
                let ino = u32::try_from(ino_key).map_err(|_| DriverError::DeviceFault)?;
                report.files_mapped += 1;
                let extent_spec = extent_spec(ino);
                extent_walk.restart();
                loop {
                    match self.btree_next_leaf(inode.extent_root, extent_spec, &mut extent_walk) {
                        Ok(true) => {}
                        Ok(false) => break,
                        Err(_) => {
                            report.unreadable_inodes += 1;
                            break;
                        }
                    }
                    for (start, ev) in extent_walk.entries() {
                        let Ok(ext) = Extent::decode(ev) else {
                            // A malformed extent value cannot name its blocks;
                            // count one skip and keep extracting the rest of
                            // the file.
                            report.blocks_skipped += 1;
                            continue;
                        };
                        if ext.compressed {
                            // A cluster extracts whole or not at all: its
                            // blocks only exist through the decompressed frame.
                            match self.read_data_cluster(&ext) {
                                Ok(plain) => {
                                    for b in 0..ext.len {
                                        let slice = &plain[as_usize(b) * cap..][..cap];
                                        out.emit_block(ino, start + b, inode.size, slice);
                                        report.blocks_extracted += 1;
                                    }
                                }
                                Err(_) => report.blocks_skipped += ext.len,
                            }
                            continue;
                        }
                        for b in 0..ext.len {
                            let logical = start + b;
                            if self
                                .read_data_block_classified(ext.phys + b, &mut buf)
                                .is_ok()
                            {
                                out.emit_block(ino, logical, inode.size, &buf[..cap]);
                                report.blocks_extracted += 1;
                            } else {
                                report.blocks_skipped += 1;
                            }
                        }
                    }
                }
            }
        }
    }
}
