//! Online scrub: a resumable, interrupt-safe verify-and-repair pass
//! (`docs/src/filesystem/arxfs-spec.md` §12, §15.8).
//!
//! Scrub walks the live filesystem **while it stays mounted** and, leaning on
//! the repair seams the earlier stages built, verifies and — where it safely
//! can — repairs:
//!
//! * **Metadata** — every live metadata block (the committed superblock slot,
//!   the transaction root, the inode and per-file extent B-trees, and the
//!   chunk and reverse-reference trees) is authenticated in **both** physical
//!   copies. A copy that fails the keyed authenticator is **repaired from its
//!   good companion** (the Stage 3 redundancy seam); a both-copies-bad block
//!   is recorded as an unrepairable finding, fail-closed and never a panic.
//! * **Data** — every live file-data block is run through the Stage 5/6 read
//!   pipeline and any failure is classified by its
//!   [`crate::integrity::DataFault`] (`Physical` / `Aead` / `Logical`) and
//!   **recorded**. Deep repair / reconstruction of data is a later stage;
//!   scrub records honestly rather than pretending to fix what it cannot.
//! * **Refcounts + reverse references** — the chunk refcounts and reverse-ref
//!   sets are **recomputed from the live inode/extent trees** and compared with
//!   the on-disk chunk and reverse-reference trees. A divergence is a
//!   finding; correcting it without losing a referrer is the repair.
//!
//! Scrub is **resumable**: a scrub-progress record (a `BlockType::ScrubProgress`
//! block) reached from the
//! transaction root persists the cursor and the accumulated counts, so a
//! bounded call can stop and a later call resumes exactly where it left off
//! and reaches the same result as one uninterrupted pass. The record is
//! rebuildable metadata: a crash mid-scrub leaves a mountable volume and
//! ordinary crash recovery never requires scrub.
//!
//! Scrub is **capability-gated** (`CAP_FS_MOUNT`) and reports its findings
//! through a structured [`ScrubReport`], logging security-relevant findings
//! through `lib/log` with stable event IDs. It
//! never silently mutates: a clean scrub of a clean volume changes nothing.

use tairix_log::{log, Event, EventId, Level, Sink};

/// Range start (inclusive) reserved for `arxfs` scrub event identifiers.
pub const ARXFS_RANGE_START: u32 = 12_000;
/// Range end (exclusive) reserved for `arxfs` scrub event identifiers.
pub const ARXFS_RANGE_END: u32 = 13_000;

/// A scrub pass finished and the volume verified clean (no faults found).
pub const SCRUB_CLEAN: EventId = EventId(12_001);
/// A scrub pass finished after detecting (and where safe, repairing) faults.
pub const SCRUB_FAULTS_FOUND: EventId = EventId(12_002);
/// A scrub pass was bounded by its budget and persisted a resumable cursor.
pub const SCRUB_PAUSED: EventId = EventId(12_003);
/// A scrub was refused because the caller lacks `CAP_FS_MOUNT`.
pub const SCRUB_DENIED: EventId = EventId(12_004);

/// Every scrub event identifier falls inside the reserved `arxfs` range, so
/// the stable IDs external audit-log consumers rely on never collide with
/// another subsystem's range.
const _: () = {
    assert!(SCRUB_CLEAN.0 >= ARXFS_RANGE_START && SCRUB_CLEAN.0 < ARXFS_RANGE_END);
    assert!(SCRUB_FAULTS_FOUND.0 >= ARXFS_RANGE_START && SCRUB_FAULTS_FOUND.0 < ARXFS_RANGE_END);
    assert!(SCRUB_PAUSED.0 >= ARXFS_RANGE_START && SCRUB_PAUSED.0 < ARXFS_RANGE_END);
    assert!(SCRUB_DENIED.0 >= ARXFS_RANGE_START && SCRUB_DENIED.0 < ARXFS_RANGE_END);
};

/// How much work one [`crate::ARXFS::scrub`] call may do before persisting a
/// resumable cursor and returning.
#[derive(Copy, Clone, Debug, Eq, PartialEq)]
pub enum ScrubBudget {
    /// Verify the whole volume in this call (resuming any in-progress scrub).
    Unlimited,
    /// Verify at most this many inodes in this call, then persist progress and
    /// return so the caller can resume later. `0` performs only the one-time
    /// global-metadata pass of a fresh scrub.
    Inodes(u64),
}

/// The structured outcome of a scrub pass (`docs/src/filesystem/arxfs-spec.md`
/// §12). Counts accumulate across the resumable calls of a single scrub, so a
/// completed scrub's report is identical whether it ran in one call or many.
#[derive(Copy, Clone, Debug, Default, Eq, PartialEq)]
pub struct ScrubReport {
    /// `true` once the whole volume has been verified; `false` when the budget
    /// paused the pass and a resumable cursor was persisted.
    pub complete: bool,
    /// Metadata blocks (counting each mirrored block once) authenticated.
    pub metadata_blocks_checked: u64,
    /// Metadata blocks whose bad copy was repaired from its good companion.
    pub metadata_repaired: u64,
    /// Metadata blocks whose **both** copies failed: an unrepairable finding.
    pub metadata_unrepairable: u64,
    /// File-data blocks run through the integrity read pipeline.
    pub data_blocks_checked: u64,
    /// Data blocks that failed the fast physical checksum.
    pub data_physical_faults: u64,
    /// Data blocks whose AEAD tag failed (ciphertext / crypto-trailer tamper).
    pub data_aead_faults: u64,
    /// Data blocks whose decrypted plaintext did not match its logical hash.
    pub data_logical_faults: u64,
    /// Chunk records whose refcount diverged from the recomputed truth.
    pub refcount_divergences: u64,
    /// Reverse-reference records whose referrer set diverged from the truth.
    pub reverse_ref_divergences: u64,
    /// Divergences scrub corrected in place (without losing a referrer).
    pub divergences_corrected: u64,
}

impl ScrubReport {
    /// Whether the pass recorded any fault or divergence (clean scrub is all
    /// zero on the fault counters).
    #[must_use]
    pub fn found_faults(&self) -> bool {
        self.metadata_repaired != 0
            || self.metadata_unrepairable != 0
            || self.data_physical_faults != 0
            || self.data_aead_faults != 0
            || self.data_logical_faults != 0
            || self.refcount_divergences != 0
            || self.reverse_ref_divergences != 0
    }

    /// Emit the closing scrub event for this report to `sink` (log security-relevant findings with a stable event ID).
    pub(crate) fn log_outcome(&self, sink: &dyn Sink) {
        let (id, level, message) = if !self.complete {
            (SCRUB_PAUSED, Level::Info, "arxfs scrub paused (resumable)")
        } else if self.found_faults() {
            (SCRUB_FAULTS_FOUND, Level::Warn, "arxfs scrub found faults")
        } else {
            (SCRUB_CLEAN, Level::Info, "arxfs scrub clean")
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

use alloc::collections::BTreeMap;
use alloc::vec::Vec;

use tairix_abi::driver::block::Block;
use tairix_abi::{CapabilityId, CapabilityQuery, DriverError};

use crate::dedupe::{ChunkRecord, Referrer, REVERSE_REF_CAP};
use crate::header::{BlockHeader, BlockType, HEADER_LEN};
use crate::integrity::DataFault;
use crate::{
    as_usize, extent_spec, inode_spec, rd_u64, wr_u64, Extent, Inode, ARXFS, MAX_BLOCK_SIZE,
    ROOT_INO,
};

/// Owner object stamped in the scrub-progress block header; a reserved
/// sentinel distinct from any inode number and from the chunk and
/// reverse-reference tree owners (`crate::dedupe`).
const SCRUB_PROGRESS_OWNER: u64 = u64::MAX - 3;

/// Magic in the scrub-progress payload: `"RFSSCRB1"`.
const SCRUB_MAGIC: u64 = 0x5246_5353_4352_4231;

// Scrub-progress payload field offsets, relative to the end of the header.
const SP_MAGIC: usize = HEADER_LEN;
const SP_CURSOR: usize = HEADER_LEN + 8;
const SP_COUNTS: usize = HEADER_LEN + 16;
/// Number of `u64` count fields persisted after the cursor.
const SP_COUNT_FIELDS: usize = 10;

/// Outcome of verifying one metadata block's two physical copies.
#[derive(Copy, Clone, Debug, Eq, PartialEq)]
pub(crate) enum MetaStatus {
    /// Both copies authenticated.
    Clean,
    /// One copy was bad and repaired from its good companion.
    Repaired,
    /// Both copies failed: an unrepairable finding (recorded, never panicked).
    Unrepairable,
}

impl<B: Block> ARXFS<B> {
    /// Run an online scrub, resuming any scrub already in progress
    /// (`docs/src/filesystem/arxfs-spec.md` §12).
    ///
    /// `budget` bounds how much one call does: [`ScrubBudget::Unlimited`]
    /// verifies the whole volume; [`ScrubBudget::Inodes`] verifies a bounded
    /// number of inodes, persists a resumable cursor, and returns so the
    /// caller can resume later — the accumulated [`ScrubReport`] of a completed
    /// scrub is identical either way. The closing finding is logged to `sink`
    /// with a stable event ID.
    ///
    /// # Errors
    ///
    /// * [`DriverError::PermissionDenied`] if `caps` does not grant
    ///   [`CapabilityId::FS_MOUNT`] (fail-closed).
    /// * [`DriverError::DeviceFault`] / [`DriverError::NoSpace`] on an
    ///   unrecoverable device error while reading the structure to verify or
    ///   while persisting progress (never a panic).
    ///
    /// # Capabilities
    ///
    /// Requires [`CapabilityId::FS_MOUNT`].
    pub fn scrub(
        &mut self,
        caps: &dyn CapabilityQuery,
        sink: &dyn Sink,
        budget: ScrubBudget,
    ) -> Result<ScrubReport, DriverError> {
        if !caps.holds(CapabilityId::FS_MOUNT) {
            log(
                sink,
                &Event {
                    level: Level::Warn,
                    id: SCRUB_DENIED,
                    message: "arxfs scrub denied: missing CAP_FS_MOUNT",
                    fields: &[],
                },
            );
            return Err(DriverError::PermissionDenied);
        }
        self.begin();
        match self.scrub_run(budget) {
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

    /// The body of one scrub call. Returns the (possibly partial) report and
    /// whether it mutated tree state (so the caller commits only then, keeping
    /// a clean scrub idempotent — metadata copy-repairs are direct block
    /// writes, not transactional).
    fn scrub_run(&mut self, budget: ScrubBudget) -> Result<(ScrubReport, bool), DriverError> {
        let (mut report, mut cursor) = self.scrub_resume_point()?;
        let mut mutated = false;

        let inodes = self.btree_collect_entries(self.inode_tree_root, inode_spec())?;
        let limit = match budget {
            ScrubBudget::Unlimited => usize::MAX,
            ScrubBudget::Inodes(n) => as_usize(n),
        };
        let mut processed = 0usize;
        let mut paused = false;
        for (ino_key, value) in &inodes {
            if *ino_key < cursor {
                continue;
            }
            if processed >= limit {
                paused = true;
                break;
            }
            let ino = u32::try_from(*ino_key).map_err(|_| DriverError::DeviceFault)?;
            if let Some(inode) = Inode::decode(value)? {
                self.scrub_inode(ino, &inode, &mut report)?;
            }
            processed += 1;
            cursor = ino_key.wrapping_add(1);
        }

        if paused {
            self.store_scrub_progress(cursor, &report)?;
            report.complete = false;
            return Ok((report, true));
        }

        // Every inode has been verified: recompute and reconcile the chunk
        // refcounts and reverse-reference sets against the on-disk trees.
        if self.scrub_refcounts(&inodes, &mut report)? {
            mutated = true;
        }
        // Clear the resumable progress record now the pass is complete.
        if self.scrub_progress_root != 0 {
            let old = self.scrub_progress_root;
            self.free_meta(old);
            self.scrub_progress_root = 0;
            mutated = true;
        }
        report.complete = true;
        Ok((report, mutated))
    }

    /// One unbounded verification pass over the whole volume, shared with the
    /// offline [`crate::ARXFS::check`] (Stage 9): the
    /// volume-wide metadata, then every inode's metadata and data blocks, then
    /// the refcount/reverse-reference reconcile against the live extents.
    /// Returns `true` when the reconcile corrected on-disk refcount state (so
    /// the caller commits the new root). The metadata copy-repairs it performs
    /// are direct block writes, not transactional.
    pub(crate) fn verify_everything(
        &mut self,
        report: &mut ScrubReport,
    ) -> Result<bool, DriverError> {
        self.scrub_global_metadata(report)?;
        let inodes = self.btree_collect_entries(self.inode_tree_root, inode_spec())?;
        for (ino_key, value) in &inodes {
            let ino = u32::try_from(*ino_key).map_err(|_| DriverError::DeviceFault)?;
            if let Some(inode) = Inode::decode(value)? {
                self.scrub_inode(ino, &inode, report)?;
            }
        }
        self.scrub_refcounts(&inodes, report)
    }

    /// Determine where this scrub call starts: resume the persisted cursor and
    /// counts of an in-progress scrub, or start fresh by verifying the global
    /// metadata once and beginning the inode walk at the root inode. A corrupt
    /// progress record (rebuildable) restarts the scrub rather than failing
    /// the mount.
    fn scrub_resume_point(&mut self) -> Result<(ScrubReport, u64), DriverError> {
        if self.scrub_progress_root != 0 {
            if let Some(resumed) = self.load_scrub_progress() {
                return Ok(resumed);
            }
        }
        let mut report = ScrubReport::default();
        self.scrub_global_metadata(&mut report)?;
        Ok((report, u64::from(ROOT_INO)))
    }

    /// Verify the volume-wide metadata that is not reached through an inode:
    /// the committed superblock slot, the transaction root, the inode tree,
    /// and the chunk and reverse-reference trees. Done once per scrub, at the
    /// fresh start, so an interrupted and an uninterrupted pass count it
    /// identically.
    fn scrub_global_metadata(&mut self, report: &mut ScrubReport) -> Result<(), DriverError> {
        let committed_slot = crate::superblock::slot_block(
            self.ring_pos.wrapping_sub(1) % crate::superblock::RING_SLOTS,
        );
        let status = self.scrub_meta(committed_slot, BlockType::Superblock)?;
        report.note_meta(status);
        let status = self.scrub_meta(self.root_phys, BlockType::TxnRoot)?;
        report.note_meta(status);
        self.scrub_btree(self.inode_tree_root, report)?;
        self.scrub_btree(self.chunk_tree_root, report)?;
        self.scrub_btree(self.reverse_ref_tree_root, report)?;
        Ok(())
    }

    /// Verify one inode's metadata (its extent tree) and the live blocks it
    /// maps: a directory's blocks as mirrored metadata, and a regular file's
    /// or a symbolic link's through the data integrity pipeline — a link's
    /// target is stored as node data, so it is verified exactly like file
    /// content (`docs/src/filesystem/arxfs-spec.md` §20).
    fn scrub_inode(
        &mut self,
        ino: u32,
        inode: &Inode,
        report: &mut ScrubReport,
    ) -> Result<(), DriverError> {
        let extent_ok = self.scrub_btree(inode.extent_root, report)?;
        if !extent_ok {
            // The extent tree could not be fully read; the unrepairable node is
            // already recorded. Skip the data walk rather than fail closed on a
            // read that cannot succeed (record, do not panic).
            return Ok(());
        }
        let spec = extent_spec(ino);
        let mirrored = inode.kind.content_is_metadata();
        let entries = self.btree_collect_entries(inode.extent_root, spec)?;
        let mut buf = [0u8; MAX_BLOCK_SIZE];
        for (_, value) in entries {
            let ext = Extent::decode(&value)?;
            if ext.compressed {
                if mirrored {
                    // A directory never holds a compressed extent; record the
                    // impossible shape rather than scan the wrong blocks.
                    report.note_meta(MetaStatus::Unrepairable);
                    continue;
                }
                // The whole cluster verifies in one bounded pass: every
                // stored block's integrity layers plus the frame shape and
                // its decompression.
                report.data_blocks_checked += ext.stored;
                if let Err(fault) = self.read_data_cluster_classified(&ext) {
                    report.note_data_fault(fault);
                }
                continue;
            }
            for b in 0..ext.len {
                let block = ext.phys + b;
                if mirrored {
                    let status = self.scrub_meta(block, BlockType::Directory)?;
                    report.note_meta(status);
                } else {
                    report.data_blocks_checked += 1;
                    if let Err(fault) = self.read_data_block_classified(block, &mut buf) {
                        report.note_data_fault(fault);
                    }
                }
            }
        }
        Ok(())
    }

    /// Walk a copy-on-write B-tree, verifying every node's two physical copies
    /// (repairing a bad copy from its companion, the Stage 3 seam) and
    /// recording an unrepairable node. Returns `true` when every node in the
    /// tree was readable. The walk authenticates each node itself rather than
    /// going through the repair-on-read [`crate::ARXFS::read_meta`], so a
    /// repair it performs is counted exactly once.
    pub(crate) fn scrub_btree(
        &mut self,
        root: u64,
        report: &mut ScrubReport,
    ) -> Result<bool, DriverError> {
        if root == 0 {
            return Ok(true);
        }
        let mut buf = [0u8; MAX_BLOCK_SIZE];
        let status = self.scrub_meta_into(root, BlockType::Btree, &mut buf)?;
        report.note_meta(status);
        if status == MetaStatus::Unrepairable {
            return Ok(false);
        }
        let (level, children) = self.btree_node_children(&buf);
        if level == 0 {
            return Ok(true);
        }
        let mut all_ok = true;
        for child in children {
            if !self.scrub_btree(child, report)? {
                all_ok = false;
            }
        }
        Ok(all_ok)
    }

    /// Verify the two physical copies of the metadata block at `phys`,
    /// repairing a bad copy from its good companion (Stage 3 seam). Discards
    /// the verified bytes; use [`Self::scrub_meta_into`] when the caller needs
    /// them (to recurse into a tree).
    pub(crate) fn scrub_meta(
        &mut self,
        phys: u64,
        expect_type: BlockType,
    ) -> Result<MetaStatus, DriverError> {
        let mut buf = [0u8; MAX_BLOCK_SIZE];
        self.scrub_meta_into(phys, expect_type, &mut buf)
    }

    /// Verify the two physical copies of the metadata block at `phys` and, on
    /// success, leave the good copy's bytes in `out`. Repairs a bad copy from
    /// its good companion; a both-copies-bad block is reported as
    /// [`MetaStatus::Unrepairable`] (recorded, never panicked).
    pub(crate) fn scrub_meta_into(
        &mut self,
        phys: u64,
        expect_type: BlockType,
        out: &mut [u8],
    ) -> Result<MetaStatus, DriverError> {
        let bs = self.block_size;
        let comp = Self::companion(phys);
        let mut primary = [0u8; MAX_BLOCK_SIZE];
        let mut companion = [0u8; MAX_BLOCK_SIZE];
        let primary_ok = self.read_block(phys, &mut primary).is_ok()
            && BlockHeader::decode_verify(
                &primary[..bs],
                expect_type,
                self.fs_uuid,
                phys,
                &self.mac_key,
            )
            .is_ok();
        let companion_ok = self.read_block(comp, &mut companion).is_ok()
            && BlockHeader::decode_verify(
                &companion[..bs],
                expect_type,
                self.fs_uuid,
                phys,
                &self.mac_key,
            )
            .is_ok();
        match (primary_ok, companion_ok) {
            (true, true) => {
                out[..bs].copy_from_slice(&primary[..bs]);
                Ok(MetaStatus::Clean)
            }
            (true, false) => {
                self.write_block(comp, &primary[..bs])?;
                out[..bs].copy_from_slice(&primary[..bs]);
                Ok(MetaStatus::Repaired)
            }
            (false, true) => {
                self.write_block(phys, &companion[..bs])?;
                out[..bs].copy_from_slice(&companion[..bs]);
                Ok(MetaStatus::Repaired)
            }
            (false, false) => Ok(MetaStatus::Unrepairable),
        }
    }
}

impl ScrubReport {
    /// Fold one metadata-block verification result into the report.
    pub(crate) fn note_meta(&mut self, status: MetaStatus) {
        self.metadata_blocks_checked += 1;
        match status {
            MetaStatus::Clean => {}
            MetaStatus::Repaired => self.metadata_repaired += 1,
            MetaStatus::Unrepairable => self.metadata_unrepairable += 1,
        }
    }

    /// Fold one data-block integrity fault into the report, keyed by class.
    pub(crate) fn note_data_fault(&mut self, fault: DataFault) {
        match fault {
            DataFault::Physical => self.data_physical_faults += 1,
            DataFault::Aead => self.data_aead_faults += 1,
            DataFault::Logical => self.data_logical_faults += 1,
        }
    }
}

impl<B: Block> ARXFS<B> {
    /// Recompute the chunk refcounts and reverse-reference sets from the live
    /// inode/extent trees and reconcile them with the on-disk chunk and
    /// reverse-reference trees (`docs/src/filesystem/arxfs-spec.md` §9). The
    /// extent-derived references are the liveness truth, so a divergence is
    /// corrected toward them without dropping a referrer. Returns `true` when a
    /// correction was made (so the caller commits the new root).
    pub(crate) fn scrub_refcounts(
        &mut self,
        inodes: &[(u64, Vec<u8>)],
        report: &mut ScrubReport,
    ) -> Result<bool, DriverError> {
        // Build the authoritative referrer set per physical data block from
        // every file's and link's extents (a link's target is node data, so
        // it shares chunks like any other; only directories are mirrored
        // metadata rather than shared chunks and never participate).
        let mut referrers: BTreeMap<u64, Vec<Referrer>> = BTreeMap::new();
        for (ino_key, value) in inodes {
            let Some(inode) = Inode::decode(value)? else {
                continue;
            };
            if inode.kind.content_is_metadata() {
                continue;
            }
            let ino = u32::try_from(*ino_key).map_err(|_| DriverError::DeviceFault)?;
            let spec = extent_spec(ino);
            for (start, ev) in self.btree_collect_entries(inode.extent_root, spec)? {
                let ext = Extent::decode(&ev)?;
                if ext.compressed {
                    // A cluster is shared as a unit: one referrer, keyed by
                    // its first physical block, naming its logical start.
                    referrers.entry(ext.phys).or_default().push((ino, start));
                    continue;
                }
                for b in 0..ext.len {
                    referrers
                        .entry(ext.phys + b)
                        .or_default()
                        .push((ino, start + b));
                }
            }
        }

        let mut corrected = false;
        // A block with two or more live referrers must be an explicit shared
        // chunk whose refcount and reverse-ref set match the truth.
        for (phys, refs) in &referrers {
            let count = refs.len() as u64;
            if count < 2 {
                // A single referrer is the implicit-refcount-1 case: there must
                // be no chunk record. A stale shared record is a divergence.
                if let Some(_record) = self.chunk_get(*phys)? {
                    report.refcount_divergences += 1;
                    self.chunk_remove(*phys)?;
                    self.reverse_refs_remove(*phys)?;
                    report.divergences_corrected += 1;
                    corrected = true;
                }
                continue;
            }
            match self.chunk_get(*phys)? {
                Some(record) => {
                    if record.refcount != count {
                        report.refcount_divergences += 1;
                        let fixed = ChunkRecord {
                            refcount: count,
                            ..record
                        };
                        self.chunk_put(*phys, &fixed)?;
                        report.divergences_corrected += 1;
                        corrected = true;
                    }
                    if !self.reverse_refs_match(*phys, refs)? {
                        report.reverse_ref_divergences += 1;
                        let capped: Vec<Referrer> =
                            refs.iter().take(REVERSE_REF_CAP).copied().collect();
                        self.reverse_refs_put(*phys, &capped)?;
                        report.divergences_corrected += 1;
                        corrected = true;
                    }
                }
                None => {
                    // The block is genuinely shared but carries no chunk
                    // record. Recreating it needs the chunk's logical length
                    // and hash, which scrub does not reconstruct here (that is
                    // the Stage 9 offline check). Record honestly without
                    // pretending to repair.
                    report.refcount_divergences += 1;
                }
            }
        }

        Ok(corrected)
    }

    /// Whether the on-disk reverse-reference set for the chunk at `phys` equals
    /// the recomputed `expected` set (compared as sets, capped at
    /// [`REVERSE_REF_CAP`] — the on-disk record never holds more).
    fn reverse_refs_match(
        &mut self,
        phys: u64,
        expected: &[Referrer],
    ) -> Result<bool, DriverError> {
        let stored = self.reverse_refs(phys)?;
        let want: Vec<Referrer> = expected.iter().take(REVERSE_REF_CAP).copied().collect();
        if stored.len() != want.len() {
            return Ok(false);
        }
        for r in &want {
            if !stored.contains(r) {
                return Ok(false);
            }
        }
        Ok(true)
    }

    /// Load the persisted scrub cursor and accumulated counts from the
    /// progress record. Returns `None` when the record is unreadable or its
    /// magic is wrong: the record is rebuildable, so a corrupt one simply
    /// restarts the scrub rather than failing the mount.
    fn load_scrub_progress(&mut self) -> Option<(ScrubReport, u64)> {
        let mut buf = [0u8; MAX_BLOCK_SIZE];
        self.read_meta(self.scrub_progress_root, BlockType::ScrubProgress, &mut buf)
            .ok()?;
        if rd_u64(&buf, SP_MAGIC) != SCRUB_MAGIC {
            return None;
        }
        let cursor = rd_u64(&buf, SP_CURSOR);
        let mut counts = [0u64; SP_COUNT_FIELDS];
        for (i, slot) in counts.iter_mut().enumerate() {
            *slot = rd_u64(&buf, SP_COUNTS + i * 8);
        }
        let report = ScrubReport {
            complete: false,
            metadata_blocks_checked: counts[0],
            metadata_repaired: counts[1],
            metadata_unrepairable: counts[2],
            data_blocks_checked: counts[3],
            data_physical_faults: counts[4],
            data_aead_faults: counts[5],
            data_logical_faults: counts[6],
            refcount_divergences: counts[7],
            reverse_ref_divergences: counts[8],
            divergences_corrected: counts[9],
        };
        Some((report, cursor))
    }

    /// Persist the scrub cursor and accumulated counts to the progress record,
    /// copy-on-writing it (and updating [`Self::scrub_progress_root`]) so a
    /// crash mid-scrub still leaves a mountable volume.
    fn store_scrub_progress(
        &mut self,
        cursor: u64,
        report: &ScrubReport,
    ) -> Result<(), DriverError> {
        let mut buf = [0u8; MAX_BLOCK_SIZE];
        let bs = self.block_size;
        for byte in &mut buf[HEADER_LEN..bs] {
            *byte = 0;
        }
        wr_u64(&mut buf, SP_MAGIC, SCRUB_MAGIC);
        wr_u64(&mut buf, SP_CURSOR, cursor);
        let counts = [
            report.metadata_blocks_checked,
            report.metadata_repaired,
            report.metadata_unrepairable,
            report.data_blocks_checked,
            report.data_physical_faults,
            report.data_aead_faults,
            report.data_logical_faults,
            report.refcount_divergences,
            report.reverse_ref_divergences,
            report.divergences_corrected,
        ];
        for (i, value) in counts.iter().enumerate() {
            wr_u64(&mut buf, SP_COUNTS + i * 8, *value);
        }
        let old = self.scrub_progress_root;
        let new = self.cow_meta(
            old,
            &mut buf,
            BlockType::ScrubProgress,
            SCRUB_PROGRESS_OWNER,
            0,
        )?;
        self.scrub_progress_root = new;
        Ok(())
    }
}
