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

use alloc::collections::BTreeSet;
use alloc::vec::Vec;

use tairix_abi::driver::block::Block;
use tairix_abi::{CapabilityId, CapabilityQuery, DriverError};
use tairix_log::{log, Event, EventId, Level, Sink};

use crate::header::{BlockType, HEADER_LEN};
use crate::scrub::{ScrubReport, ARXFS_RANGE_END, ARXFS_RANGE_START};
use crate::superblock;
use crate::transaction::TxnRoot;
use crate::{
    as_usize, extent_spec, inode_spec, rd_u32, Extent, Inode, InodeKind, ARXFS, DIRENT_SIZE,
    MAX_BLOCK_SIZE, NAME_MAX, ROOT_INO,
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
    /// `true` when the pass found nothing it could not safely repair.
    pub structure_sound: bool,
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
        self.begin();
        match self.check_run() {
            Ok((report, mutated)) => {
                if mutated {
                    self.commit()?;
                } else {
                    self.rollback();
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
        let mut report = CheckReport::default();

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
        let reclaimed = self.check_directories_and_orphans(&mut report)?;

        // The feature word must cover what the volume actually holds, or a
        // reader lacking the feature would mount and misread instead of
        // refusing.
        report.features_declared = self.check_declared_features()?;

        let v = &report.verification;
        let divergences = v.refcount_divergences + v.reverse_ref_divergences;
        report.unrecoverable_findings = v.metadata_unrepairable
            + v.data_physical_faults
            + v.data_aead_faults
            + v.data_logical_faults
            + report.dangling_entries
            + divergences.saturating_sub(v.divergences_corrected);
        report.structure_sound = report.unrecoverable_findings == 0;
        report.complete = true;

        Ok((
            report,
            refcounts_corrected || reclaimed || report.features_declared != 0,
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
        for (_, value) in self.btree_collect_entries(self.inode_tree_root, inode_spec())? {
            let Some(inode) = Inode::decode(&value)? else {
                continue;
            };
            if inode.kind == InodeKind::Link {
                self.incompat |= superblock::INCOMPAT_SYMLINKS;
                return Ok(1);
            }
        }
        Ok(0)
    }

    /// Collect a directory's `(child inode, is "." or "..")` entries, reusing
    /// the same on-disk directory layout as [`Self::dir_lookup`]. Self and
    /// parent links (`.` / `..`) are flagged so the reachability walk and the
    /// orphan detection ignore them (an entry's own directory must not count as
    /// a reference to itself).
    fn dir_entries(&mut self, dir: &Inode) -> Result<Vec<(u32, bool)>, DriverError> {
        let per = self.dirents_per_block();
        let mut buf = [0u8; MAX_BLOCK_SIZE];
        let mut out = Vec::new();
        for blk in 0..self.dir_block_count(dir) {
            let ptr = self.block_ptr(dir, blk)?;
            if ptr == 0 {
                continue;
            }
            self.read_meta(ptr, BlockType::Directory, &mut buf)?;
            for slot in 0..per {
                let base = HEADER_LEN + slot * DIRENT_SIZE;
                let ino = rd_u32(&buf, base);
                if ino == 0 {
                    continue;
                }
                let name_len = as_usize(u64::from(rd_u32(&buf, base + 4)));
                if name_len > NAME_MAX {
                    return Err(DriverError::DeviceFault);
                }
                let name = &buf[base + 8..base + 8 + name_len];
                let dot = name == b"." || name == b"..";
                out.push((ino, dot));
            }
        }
        Ok(out)
    }

    /// Walk the directory tree from the root, validate every entry's target
    /// exists, and detect and reclaim orphaned inodes — live inodes the
    /// directory tree no longer reaches (`docs/src/filesystem/arxfs-spec.md`
    /// §12). Reclaiming frees the orphan's data blocks (releasing any shared
    /// chunk references) and its inode slot. Returns whether anything was
    /// reclaimed (so the caller commits). An entry pointing at a missing inode
    /// is a *dangling* finding: it is reported, not auto-deleted, since removing
    /// a live name is not a safe automatic repair.
    fn check_directories_and_orphans(
        &mut self,
        report: &mut CheckReport,
    ) -> Result<bool, DriverError> {
        let mut reachable: BTreeSet<u32> = BTreeSet::new();
        reachable.insert(ROOT_INO);
        let mut stack: Vec<u32> = alloc::vec![ROOT_INO];
        while let Some(dir_ino) = stack.pop() {
            let dir = self.read_inode(dir_ino)?;
            if !dir.is_dir() {
                continue;
            }
            report.directories_checked += 1;
            for (child, is_dot) in self.dir_entries(&dir)? {
                if is_dot {
                    continue;
                }
                let spec = inode_spec();
                let exists = self
                    .btree_get(self.inode_tree_root, u64::from(child), spec)?
                    .and_then(|value| Inode::decode(&value).ok().flatten())
                    .is_some();
                if !exists {
                    report.dangling_entries += 1;
                    continue;
                }
                if reachable.insert(child) {
                    stack.push(child);
                }
            }
        }

        // Any live inode the walk did not reach (other than the root) is an
        // orphan.
        let inodes = self.btree_collect_entries(self.inode_tree_root, inode_spec())?;
        let mut orphans: Vec<u32> = Vec::new();
        for (ino_key, value) in &inodes {
            if Inode::decode(value)?.is_none() {
                continue;
            }
            let ino = u32::try_from(*ino_key).map_err(|_| DriverError::DeviceFault)?;
            if ino == ROOT_INO {
                continue;
            }
            if !reachable.contains(&ino) {
                report.orphaned_inodes += 1;
                orphans.push(ino);
            }
        }
        for ino in orphans {
            let mut inode = self.read_inode(ino)?;
            self.free_all_blocks(&mut inode, ino)?;
            self.free_inode(ino)?;
            report.orphans_reclaimed += 1;
        }
        Ok(report.orphans_reclaimed > 0)
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
    /// cannot be read at all is counted as unreadable and skipped. An inode
    /// tree too damaged to enumerate leaves the report's root-found state but
    /// extracts nothing rather than failing.
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
        let Ok(inodes) = self.btree_collect_entries(self.inode_tree_root, inode_spec()) else {
            return Ok(());
        };
        let mut buf = [0u8; MAX_BLOCK_SIZE];
        for (ino_key, value) in inodes {
            let Some(inode) = Inode::decode(&value)? else {
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
            let spec = extent_spec(ino);
            let Ok(entries) = self.btree_collect_entries(inode.extent_root, spec) else {
                report.unreadable_inodes += 1;
                continue;
            };
            for (start, ev) in entries {
                let Ok(ext) = Extent::decode(&ev) else {
                    // A malformed extent value cannot name its blocks; count
                    // one skip and keep extracting the rest of the file.
                    report.blocks_skipped += 1;
                    continue;
                };
                if ext.compressed {
                    // A cluster extracts whole or not at all: its blocks only
                    // exist through the decompressed frame.
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
        Ok(())
    }
}
