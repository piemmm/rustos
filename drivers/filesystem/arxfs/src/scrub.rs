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
//!
//! On a **read-only handle it writes nothing at all** — no copy-repair, no
//! refcount correction, no cursor, no transaction — because the state exists
//! for a medium whose contents must not be touched. It still verifies, and it
//! still reports every finding: a mirror it may not rewrite counts as
//! [`ScrubReport::metadata_damaged`], never as a repair that did not happen.

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
/// A scrub pass was bounded by its budget and persisted **no** cursor, because
/// the handle is read-only: the volume past this pass's reach stays unverified
/// however often the call is repeated.
pub const SCRUB_STOPPED: EventId = EventId(12_005);

/// Every scrub event identifier falls inside the reserved `arxfs` range, so
/// the stable IDs external audit-log consumers rely on never collide with
/// another subsystem's range.
const _: () = {
    assert!(SCRUB_CLEAN.0 >= ARXFS_RANGE_START && SCRUB_CLEAN.0 < ARXFS_RANGE_END);
    assert!(SCRUB_FAULTS_FOUND.0 >= ARXFS_RANGE_START && SCRUB_FAULTS_FOUND.0 < ARXFS_RANGE_END);
    assert!(SCRUB_PAUSED.0 >= ARXFS_RANGE_START && SCRUB_PAUSED.0 < ARXFS_RANGE_END);
    assert!(SCRUB_DENIED.0 >= ARXFS_RANGE_START && SCRUB_DENIED.0 < ARXFS_RANGE_END);
    assert!(SCRUB_STOPPED.0 >= ARXFS_RANGE_START && SCRUB_STOPPED.0 < ARXFS_RANGE_END);
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

/// How far one scrub pass got, and whether a later call continues it — the
/// counterpart of [`crate::StructureVerdict`] for the online pass.
///
/// The distinction between the two stopped states is not cosmetic: only a pass
/// that persisted its cursor is continued by the next call, so a bounded pass
/// on a handle that cannot persist one never reaches past its own budget
/// however often it is repeated. A caller that cannot tell them apart cannot
/// tell a volume being progressively verified from one being re-verified from
/// the start forever.
#[derive(Copy, Clone, Debug, Default, Eq, PartialEq)]
pub enum PassVerdict {
    /// The whole volume was verified.
    Complete,
    /// The budget stopped the pass and its cursor was persisted, so a later
    /// call resumes it and reaches the report an uninterrupted pass would.
    Paused,
    /// The budget stopped the pass and nothing was persisted, because the
    /// handle is read-only and writes nothing to the medium it holds. The next
    /// call starts afresh.
    #[default]
    Stopped,
}

/// The structured outcome of a scrub pass (`docs/src/filesystem/arxfs-spec.md`
/// §12). Counts accumulate across the resumable calls of a single scrub, so a
/// completed scrub's report is identical whether it ran in one call or many.
#[derive(Copy, Clone, Debug, Default, Eq, PartialEq)]
pub struct ScrubReport {
    /// How far this pass got, and whether a later call continues it.
    pub pass: PassVerdict,
    /// Metadata blocks (counting each mirrored block once) authenticated.
    pub metadata_blocks_checked: u64,
    /// Metadata blocks whose bad copy was repaired from its good companion.
    pub metadata_repaired: u64,
    /// Metadata blocks with one bad copy the pass left as it is, because the
    /// handle is read-only and writes nothing to the medium it holds. The good
    /// copy served the read; the mirror is still degraded.
    pub metadata_damaged: u64,
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
    /// Whether the pass counted, exactly, how many extents claim each physical
    /// block — the refcount reconcile's one volume-wide question
    /// (`docs/src/filesystem/arxfs-spec.md` §12). `false` when the volume could
    /// spare no run for the transient array that counting needs (a read-only
    /// handle, a nearly-full or badly fragmented volume): the refcount and
    /// reverse-reference checks that need no count still ran, and no correction
    /// was made from a partial truth.
    pub claims_counted: bool,
}

impl ScrubReport {
    /// Whether the pass recorded any fault or divergence (clean scrub is all
    /// zero on the fault counters).
    #[must_use]
    pub fn found_faults(&self) -> bool {
        self.metadata_repaired != 0
            || self.metadata_damaged != 0
            || self.metadata_unrepairable != 0
            || self.data_physical_faults != 0
            || self.data_aead_faults != 0
            || self.data_logical_faults != 0
            || self.refcount_divergences != 0
            || self.reverse_ref_divergences != 0
    }

    /// Emit the closing scrub event for this report to `sink` (log security-relevant findings with a stable event ID).
    pub(crate) fn log_outcome(&self, sink: &dyn Sink) {
        let (id, level, message) = match self.pass {
            PassVerdict::Paused => (SCRUB_PAUSED, Level::Info, "arxfs scrub paused (resumable)"),
            // A bounded pass that kept no position leaves the volume past its
            // reach unverified, so it is a finding in its own right rather
            // than an ordinary pause.
            PassVerdict::Stopped => (
                SCRUB_STOPPED,
                Level::Warn,
                "arxfs scrub stopped at its budget (read-only: no cursor kept)",
            ),
            PassVerdict::Complete if self.found_faults() => {
                (SCRUB_FAULTS_FOUND, Level::Warn, "arxfs scrub found faults")
            }
            PassVerdict::Complete => (SCRUB_CLEAN, Level::Info, "arxfs scrub clean"),
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

use alloc::vec::Vec;

use tairix_abi::driver::block::Block;
use tairix_abi::{CapabilityId, CapabilityQuery, DriverError};

use crate::btree::{TreeWalk, MAX_TREE_LEVEL};
use crate::header::{BlockHeader, BlockType, ReservedOwner, HEADER_LEN};
use crate::integrity::DataFault;
use crate::{
    as_usize, extent_spec, inode_spec, rd_u64, wr_u64, Extent, Inode, ARXFS, MAX_BLOCK_SIZE,
    ROOT_INO,
};

/// Owner object stamped in the scrub-progress block header.
const SCRUB_PROGRESS_OWNER: u64 = ReservedOwner::ScrubProgress.sentinel();

/// Magic in the scrub-progress payload: `"RFSSCRB1"`.
const SCRUB_MAGIC: u64 = 0x5246_5353_4352_4231;

// Scrub-progress payload field offsets, relative to the end of the header.
const SP_MAGIC: usize = HEADER_LEN;
const SP_CURSOR: usize = HEADER_LEN + 8;
const SP_COUNTS: usize = HEADER_LEN + 16;
/// Number of `u64` count fields persisted after the cursor.
const SP_COUNT_FIELDS: usize = 10;

/// The next unvisited child of the deepest tree level still owing one, with
/// the level it must sit at, dropping levels that have none left.
fn next_pending_child(pending: &mut Vec<(u32, Vec<u64>)>) -> Option<(u64, u32)> {
    loop {
        let frame = pending.last_mut()?;
        let level = frame.0;
        match frame.1.pop() {
            Some(child) => return Some((child, level)),
            None => {
                pending.pop();
            }
        }
    }
}

/// Outcome of verifying one metadata block's two physical copies.
#[derive(Copy, Clone, Debug, Eq, PartialEq)]
pub(crate) enum MetaStatus {
    /// Both copies authenticated.
    Clean,
    /// One copy was bad and repaired from its good companion.
    Repaired,
    /// One copy was bad and left as it is: the good copy served the read, but
    /// the handle is read-only and must not write to the medium it holds. The
    /// block is readable and its mirror is still degraded.
    Damaged,
    /// Both copies failed: an unrepairable finding (recorded, never panicked).
    Unrepairable,
}

impl MetaStatus {
    /// The verdict for a block whose bad copy the one repair rule either
    /// rewrote or declined to.
    pub(crate) fn from_repair(repaired: bool) -> Self {
        if repaired {
            Self::Repaired
        } else {
            Self::Damaged
        }
    }
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
    /// A **read-only handle verifies and reports without writing anything**:
    /// no copy-repair (the damaged mirror is reported through
    /// [`ScrubReport::metadata_damaged`] instead), no refcount correction, no
    /// persisted cursor, and no transaction. Its bounded pass is therefore not
    /// resumable and says so through [`ScrubReport::pass`].
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
        let (mut report, cursor) = self.scrub_resume_point()?;
        let mut mutated = false;

        let limit = match budget {
            ScrubBudget::Unlimited => usize::MAX,
            ScrubBudget::Inodes(n) => as_usize(n),
        };
        let spec = inode_spec();
        let mut walk = TreeWalk::new(self.block_size)?;
        let mut extent_walk = TreeWalk::new(self.block_size)?;
        walk.seek(cursor);
        let mut processed = 0usize;
        let mut paused = None;
        'walk: while self.btree_next_leaf(self.inode_tree_root, spec, &mut walk)? {
            for (ino_key, value) in walk.entries() {
                if processed >= limit {
                    // The budget stops the pass *before* this inode, so the
                    // cursor it persists is the one a resumed pass starts at.
                    paused = Some(ino_key);
                    break 'walk;
                }
                let ino = u32::try_from(ino_key).map_err(|_| DriverError::DeviceFault)?;
                if let Some(inode) = Inode::decode(value)? {
                    self.scrub_inode(ino, &inode, &mut extent_walk, &mut report)?;
                }
                processed += 1;
            }
        }

        if let Some(cursor) = paused {
            // A read-only handle persists nothing, so its bounded pass is not
            // resumable: it reports what it verified and says the next call
            // starts afresh.
            if self.read_only {
                report.pass = PassVerdict::Stopped;
                return Ok((report, false));
            }
            report.pass = PassVerdict::Paused;
            self.store_scrub_progress(cursor, &report)?;
            return Ok((report, true));
        }

        // Every inode has been verified: recompute and reconcile the chunk
        // refcounts and reverse-reference sets against the on-disk trees.
        if self.reconcile_refcounts(&mut report)? {
            mutated = true;
        }
        // Clear the resumable progress record now the pass is complete. A
        // read-only handle leaves it where it is: it writes nothing, and
        // dropping the reference in memory alone would hide a record the
        // committed root still names.
        if !self.read_only && self.scrub_progress_root != 0 {
            let old = self.scrub_progress_root;
            self.free_meta(old);
            self.scrub_progress_root = 0;
            mutated = true;
        }
        report.pass = PassVerdict::Complete;
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
        let spec = inode_spec();
        let mut walk = TreeWalk::new(self.block_size)?;
        let mut extent_walk = TreeWalk::new(self.block_size)?;
        while self.btree_next_leaf(self.inode_tree_root, spec, &mut walk)? {
            for (ino_key, value) in walk.entries() {
                let ino = u32::try_from(ino_key).map_err(|_| DriverError::DeviceFault)?;
                if let Some(inode) = Inode::decode(value)? {
                    self.scrub_inode(ino, &inode, &mut extent_walk, report)?;
                }
            }
        }
        let corrected = self.reconcile_refcounts(report)?;
        report.pass = PassVerdict::Complete;
        Ok(corrected)
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
        walk: &mut TreeWalk,
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
        let mut buf = [0u8; MAX_BLOCK_SIZE];
        walk.restart();
        while self.btree_next_leaf(inode.extent_root, spec, walk)? {
            for (_, value) in walk.entries() {
                let ext = Extent::decode(value)?;
                if ext.compressed {
                    if mirrored {
                        // A directory never holds a compressed extent; record
                        // the impossible shape rather than scan the wrong
                        // blocks.
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
        // One frame per tree level, holding that level's not-yet-visited
        // children, so the walk costs the tree's depth rather than its node
        // count and cannot overrun a kernel stack on a deep tree. Levels must
        // decrease by one on the way down, so a pointer leading back to an
        // ancestor is refused instead of walked forever.
        let mut pending: Vec<(u32, Vec<u64>)> = Vec::new();
        let mut all_ok = true;
        let mut next = Some((root, None));
        while let Some((phys, expect_level)) = next {
            let status = self.scrub_meta_into(phys, BlockType::Btree, &mut buf)?;
            report.note_meta(status);
            if status == MetaStatus::Unrepairable {
                // An unrepairable node's children cannot be reached, let alone
                // verified: record it and carry on with its siblings.
                all_ok = false;
            } else {
                let (level, mut children) = self.btree_node_children(&buf)?;
                if expect_level.is_some_and(|expect| expect != level) {
                    return Err(DriverError::DeviceFault);
                }
                if level > 0 {
                    if pending.len() >= MAX_TREE_LEVEL as usize {
                        return Err(DriverError::DeviceFault);
                    }
                    // Visited by popping the tail, so reversing keeps the
                    // ascending key order the recursive walk had.
                    children.reverse();
                    pending.try_reserve(1).map_err(|_| DriverError::NoSpace)?;
                    pending.push((level - 1, children));
                }
            }
            next = next_pending_child(&mut pending).map(|(child, level)| (child, Some(level)));
        }
        Ok(all_ok)
    }

    /// Verify the two physical copies of the metadata block at `phys`,
    /// repairing a bad copy from its good companion (Stage 3 seam). Discards
    /// the verified bytes; use [`Self::scrub_meta_into`] when the caller needs
    /// them (to walk into a tree).
    pub(crate) fn scrub_meta(
        &mut self,
        phys: u64,
        expect_type: BlockType,
    ) -> Result<MetaStatus, DriverError> {
        let mut buf = [0u8; MAX_BLOCK_SIZE];
        self.scrub_meta_into(phys, expect_type, &mut buf)
    }

    /// Verify the two physical copies of the metadata block at `phys` and, on
    /// success, leave the good copy's bytes in `out`.
    ///
    /// A bad copy is repaired from its good companion through the one repair
    /// rule ([`crate::ARXFS::repair_meta_copy`]), so a read-only handle
    /// reports [`MetaStatus::Damaged`] rather than writing to the medium it
    /// holds. A both-copies-bad block is [`MetaStatus::Unrepairable`]
    /// (recorded, never panicked).
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
                let repaired = self.repair_meta_copy(comp, &primary[..bs])?;
                out[..bs].copy_from_slice(&primary[..bs]);
                Ok(MetaStatus::from_repair(repaired))
            }
            (false, true) => {
                let repaired = self.repair_meta_copy(phys, &companion[..bs])?;
                out[..bs].copy_from_slice(&companion[..bs]);
                Ok(MetaStatus::from_repair(repaired))
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
            MetaStatus::Damaged => self.metadata_damaged += 1,
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
            // The cursor exists on the device, so this pass is a continuation
            // of a paused one until the call that finishes the walk says so.
            pass: PassVerdict::Paused,
            // A resumed pass has not reached the reconcile, so it has counted
            // nothing yet; the call that finishes the walk sets this.
            claims_counted: false,
            metadata_blocks_checked: counts[0],
            metadata_repaired: counts[1],
            // Only a read-only handle leaves a bad copy unrepaired, and a
            // read-only handle persists no cursor, so a resumed pass has none.
            metadata_damaged: 0,
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
