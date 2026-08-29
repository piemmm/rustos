//! Reconciling the chunk refcounts and reverse-reference sets against the live
//! extents (`docs/src/filesystem/arxfs-spec.md` §9, §12).
//!
//! The extent trees are the liveness truth: a physical data block is shared
//! exactly as often as an extent maps it. The chunk tree's refcount and the
//! reverse-reference tree's referrer list are derived from that, and a
//! divergence between them either leaks storage nothing reaches (a refcount
//! too high) or frees storage a live name still uses (too low) — so both
//! `scrub` and the offline `check` recompute the truth and reconcile.
//!
//! # Why any of this needs a scratch array
//!
//! Two of the three questions are answerable with bounded lookups, because
//! the write path keeps the referrer list **complete**: sharing that would
//! exceed [`REVERSE_REF_CAP`] declines to dedupe instead, so a lawful record
//! satisfies `refcount == referrers.len()` with every referrer named. Walking
//! the chunk tree and checking each stored referrer against the extent it
//! claims to come from therefore costs one bounded lookup per referrer and no
//! accumulated state at all.
//!
//! The third question — *is a block with no chunk record claimed by exactly
//! one extent?* — is irreducibly global. No index answers it, because the only
//! record of a claim is the extent that makes it, and the extent trees are
//! ordered by `(inode, logical block)` rather than by physical block. So the
//! pass streams every claim through a per-block **claim count** in a transient
//! on-disk array ([`crate::scratch`]) and reads the exact count back. Four
//! bits per block covers every lawful count exactly — a legal refcount never
//! exceeds the cap of eight — which is what makes "the refcount says two but
//! three extents claim it" detectable rather than merely suspected.
//!
//! Where the volume cannot spare a run for the array (a read-only handle, a
//! nearly-full or badly fragmented volume) the pass still runs the bounded
//! half and **reports that it did not count claims**. It makes no correction
//! at all in that case: without the exact count a refcount can only be lowered
//! on a guess, and a refcount lowered wrongly frees a block a live extent
//! still maps.

use tairix_abi::driver::block::Block;
use tairix_abi::DriverError;

use crate::btree::TreeWalk;
use crate::dedupe::{chunk_spec, decode_reverse_ref_into, ChunkRecord, Referrer, REVERSE_REF_CAP};
use crate::header::ReservedOwner;
use crate::scratch::{ElementWidth, ScratchArray, MAX_RECONCILE_WINDOWS};
use crate::scrub::ScrubReport;
use crate::{extent_spec, inode_spec, Extent, Inode, ARXFS};

/// What a pass knows about how many extents claim one physical block.
#[derive(Copy, Clone, Debug, Eq, PartialEq)]
enum ClaimCount {
    /// Exactly this many extents claim it.
    Exact(u32),
    /// More claims than the array can count. Only a volume whose write path
    /// did not produce them can reach this, so no refcount is derived from it:
    /// the record is reported and left alone.
    Saturated,
    /// The pass had no scratch array, so claims were never counted.
    Unknown,
}

/// What one pass over a window's chunk records changed, and what it left
/// owing to the referrer-recovery pass.
struct ChunkPass {
    /// On-disk state changed, so the caller commits.
    corrected: bool,
    /// A referrer list is short of the claims counted for its block, so the
    /// identities the stored record did not name have to be recovered from the
    /// extents that make those claims.
    incomplete: bool,
}

/// What verifying one chunk record concluded.
enum ChunkFix {
    /// The record agrees with the claims and every stored referrer is real.
    Clean,
    /// Fewer than two extents claim the block, so the record is stale: the
    /// block returns to the implicit refcount of one.
    Remove,
    /// The refcount, the referrer list, or both must be rewritten.
    Rewrite {
        refcount: u64,
        referrers: [Referrer; REVERSE_REF_CAP],
        listed: usize,
        refcount_wrong: bool,
        referrers_wrong: bool,
        /// Referrers the stored record did not name, so their identities have
        /// to be recovered from the extents that claim the block.
        incomplete: bool,
    },
    /// The record diverges but the truth is not known well enough to correct
    /// it without risking a refcount that frees a live block.
    Report {
        refcount_wrong: bool,
        referrers_wrong: bool,
    },
}

impl<B: Block> ARXFS<B> {
    /// Recompute the chunk refcounts and reverse-reference sets from the live
    /// extents and reconcile the on-disk trees with them. Returns `true` when
    /// a correction was made (so the caller commits the new root).
    ///
    /// # Errors
    ///
    /// [`DriverError::DeviceFault`] / [`DriverError::NoSpace`] on an
    /// unrecoverable device or allocation error (never a panic).
    pub(crate) fn reconcile_refcounts(
        &mut self,
        report: &mut ScrubReport,
    ) -> Result<bool, DriverError> {
        let mut claims = self.scratch_alloc(
            ReservedOwner::ScratchClaims,
            ElementWidth::Nibble,
            self.total_blocks,
            MAX_RECONCILE_WINDOWS,
        )?;
        let outcome = if let Some(array) = claims.as_mut() {
            report.claims_counted = true;
            self.reconcile_windowed(array, report)
        } else {
            report.claims_counted = false;
            self.verify_chunk_range(0, u64::MAX, None, report)
                .map(|pass| pass.corrected)
        };
        if let Some(array) = claims {
            // The run is handed back whether the pass succeeded or not; a
            // reservation must never outlive the pass that made it.
            let released = self.scratch_release(array);
            if outcome.is_ok() {
                released?;
            }
        }
        outcome
    }

    /// Reconcile the whole volume through an array of exactly `run` blocks, so
    /// a test can drive the multi-window path deterministically instead of
    /// waiting for a volume too full to hold a single-window array.
    #[cfg(test)]
    pub(crate) fn reconcile_refcounts_in_windows(
        &mut self,
        run: u64,
        report: &mut ScrubReport,
    ) -> Result<bool, DriverError> {
        let Some(mut claims) = self.scratch_place(
            ReservedOwner::ScratchClaims,
            ElementWidth::Nibble,
            self.total_blocks,
            run,
        )?
        else {
            return Err(DriverError::NoSpace);
        };
        report.claims_counted = true;
        let outcome = self.reconcile_windowed(&mut claims, report);
        let released = self.scratch_release(claims);
        if outcome.is_ok() {
            released?;
        }
        outcome
    }

    /// Reconcile the whole volume one scratch window at a time.
    fn reconcile_windowed(
        &mut self,
        claims: &mut ScratchArray,
        report: &mut ScrubReport,
    ) -> Result<bool, DriverError> {
        let mut corrected = false;
        let mut base = 0;
        while base < self.total_blocks {
            if base != 0 {
                self.scratch_rebase(claims, base)?;
            }
            let end = claims.window_end();
            self.count_claims(claims, report)?;
            let pass = self.verify_chunk_range(base, end, Some(claims), report)?;
            corrected |= pass.corrected;
            if pass.incomplete {
                corrected |= self.restore_referrers(claims, report)?;
            }
            base = end;
        }
        // Windows cover the device, so a record keyed past its last block is
        // reached only here. Nothing can claim a block the volume does not
        // have, so the record is stale.
        let pass = self.verify_chunk_range(self.total_blocks, u64::MAX, Some(claims), report)?;
        Ok(corrected | pass.corrected)
    }

    /// Stream every extent of every file and link through the claim array,
    /// counting how many claim each block of the current window.
    ///
    /// A block whose count reaches two must be an explicit shared chunk. The
    /// check happens here, at the moment sharing is discovered, so it costs
    /// one lookup per shared block rather than one per block on the volume. A
    /// block that has no record is reported and left alone: recreating one
    /// needs the chunk's logical length and hash, which only the data carries.
    fn count_claims(
        &mut self,
        claims: &mut ScratchArray,
        report: &mut ScrubReport,
    ) -> Result<(), DriverError> {
        let mut walk = TreeWalk::new(self.block_size)?;
        let mut extent_walk = TreeWalk::new(self.block_size)?;
        while self.btree_next_leaf(self.inode_tree_root, inode_spec(), &mut walk)? {
            for (ino_key, value) in walk.entries() {
                let Some(inode) = Inode::decode(value)? else {
                    continue;
                };
                // A directory's blocks are mirrored metadata, never shared
                // chunks, so they take no part in refcounting.
                if inode.kind.content_is_metadata() {
                    continue;
                }
                let ino = u32::try_from(ino_key).map_err(|_| DriverError::DeviceFault)?;
                self.count_inode_claims(ino, &inode, &mut extent_walk, claims, report)?;
            }
        }
        Ok(())
    }

    /// [`Self::count_claims`] for one inode's extents.
    fn count_inode_claims(
        &mut self,
        ino: u32,
        inode: &Inode,
        walk: &mut TreeWalk,
        claims: &mut ScratchArray,
        report: &mut ScrubReport,
    ) -> Result<(), DriverError> {
        let spec = extent_spec(ino);
        let (base, end) = (claims.base(), claims.window_end());
        walk.restart();
        while self.btree_next_leaf(inode.extent_root, spec, walk)? {
            for (_, value) in walk.entries() {
                let ext = Extent::decode(value, self.total_blocks)?;
                // A compressed cluster is shared as a unit, so it is one
                // claim on its first physical block.
                let (lo, hi) = if ext.compressed {
                    (ext.phys, ext.phys.saturating_add(1))
                } else {
                    (ext.phys, ext.phys.saturating_add(ext.len))
                };
                for block in lo.max(base)..hi.min(end) {
                    if self.scratch_bump(claims, block)? == 2 && self.chunk_get(block)?.is_none() {
                        report.refcount_divergences += 1;
                    }
                }
            }
        }
        Ok(())
    }

    /// Verify every chunk record with a key in `lo..hi` against the claims,
    /// correcting what the pass knows exactly.
    ///
    /// `claims` is the counted truth, and `None` means the pass never counted:
    /// it then verifies only what a record can say about itself and corrects
    /// nothing, which is also what keeps a read-only handle — the one that can
    /// never place an array — from reaching a write at all.
    fn verify_chunk_range(
        &mut self,
        lo: u64,
        hi: u64,
        mut claims: Option<&mut ScratchArray>,
        report: &mut ScrubReport,
    ) -> Result<ChunkPass, DriverError> {
        let spec = chunk_spec();
        let mut walk = TreeWalk::new(self.block_size)?;
        walk.seek(lo);
        let mut pass = ChunkPass {
            corrected: false,
            incomplete: false,
        };
        loop {
            if !self.btree_next_leaf(self.chunk_tree_root, spec, &mut walk)? {
                break;
            }
            // A correction copy-on-writes the tree the walk is reading, so one
            // per step and then resume past it. Records before it in this leaf
            // are already verified and are never revisited.
            let mut pending = None;
            let mut done = false;
            for (phys, value) in walk.entries() {
                if phys >= hi {
                    done = true;
                    break;
                }
                let record = ChunkRecord::decode(value).ok_or(DriverError::DeviceFault)?;
                let counted = match claims.as_deref_mut() {
                    // The volume has no such block, so nothing can claim it.
                    Some(_) if phys >= self.total_blocks => ClaimCount::Exact(0),
                    Some(array) => {
                        let covered = array.covers(phys);
                        let seen = self.scratch_get(array, phys)?;
                        Self::claim_count(covered, seen)
                    }
                    None => ClaimCount::Unknown,
                };
                match self.chunk_verdict(phys, &record, counted)? {
                    ChunkFix::Clean => {}
                    fix => {
                        pending = Some((phys, record, fix));
                        break;
                    }
                }
            }
            if let Some((phys, record, fix)) = pending {
                if matches!(
                    fix,
                    ChunkFix::Rewrite {
                        incomplete: true,
                        ..
                    }
                ) {
                    pass.incomplete = true;
                }
                if self.apply_chunk_fix(phys, &record, &fix, report)? {
                    pass.corrected = true;
                }
                match phys.checked_add(1) {
                    Some(next) => walk.seek(next),
                    None => walk.stop(),
                }
                continue;
            }
            if done {
                break;
            }
        }
        Ok(pass)
    }

    /// Turn a raw claim-array reading into what it means, so a saturated count
    /// is never mistaken for an exact one.
    fn claim_count(covered: bool, seen: u32) -> ClaimCount {
        if !covered {
            return ClaimCount::Unknown;
        }
        if seen >= ElementWidth::Nibble.ceiling() {
            return ClaimCount::Saturated;
        }
        ClaimCount::Exact(seen)
    }

    /// Compare one chunk record with the truth and decide what to do about it.
    ///
    /// Read-only: the caller applies the verdict once it has finished reading
    /// the leaf the record came from.
    fn chunk_verdict(
        &mut self,
        phys: u64,
        record: &ChunkRecord,
        counted: ClaimCount,
    ) -> Result<ChunkFix, DriverError> {
        let mut stored = [(0u32, 0u64); REVERSE_REF_CAP];
        let held = self.reverse_refs_into(phys, &mut stored)?;
        // Keep only the referrers an extent really backs, and only once each:
        // a repeated `(inode, logical block)` is one claim, not two.
        let mut real = [(0u32, 0u64); REVERSE_REF_CAP];
        let mut kept = 0;
        for referrer in stored.into_iter().take(held) {
            if real[..kept].contains(&referrer) {
                continue;
            }
            if self.referrer_backs_block(referrer, phys)? {
                real[kept] = referrer;
                kept += 1;
            }
        }
        let listed_ok = kept == held;
        let truth = match counted {
            ClaimCount::Exact(count) => u64::from(count),
            // Without an exact count the only safe reading is the record's own
            // consistency; a refcount is never derived from a partial one.
            ClaimCount::Saturated | ClaimCount::Unknown => {
                let refcount_wrong = record.refcount != held as u64
                    || record.refcount < 2
                    || record.refcount > REVERSE_REF_CAP as u64;
                if refcount_wrong || !listed_ok {
                    return Ok(ChunkFix::Report {
                        refcount_wrong,
                        referrers_wrong: !listed_ok,
                    });
                }
                return Ok(ChunkFix::Clean);
            }
        };
        if truth < 2 {
            return Ok(ChunkFix::Remove);
        }
        let listed_target = truth.min(REVERSE_REF_CAP as u64);
        let refcount_wrong = record.refcount != truth;
        let referrers_wrong = !listed_ok || (kept as u64) != listed_target;
        if !refcount_wrong && !referrers_wrong {
            return Ok(ChunkFix::Clean);
        }
        Ok(ChunkFix::Rewrite {
            refcount: truth,
            referrers: real,
            listed: kept,
            refcount_wrong,
            referrers_wrong,
            incomplete: (kept as u64) < listed_target,
        })
    }

    /// Carry out one chunk verdict, recording it in the report. Returns
    /// whether on-disk state changed.
    fn apply_chunk_fix(
        &mut self,
        phys: u64,
        record: &ChunkRecord,
        fix: &ChunkFix,
        report: &mut ScrubReport,
    ) -> Result<bool, DriverError> {
        match *fix {
            ChunkFix::Clean => Ok(false),
            ChunkFix::Report {
                refcount_wrong,
                referrers_wrong,
            } => {
                if refcount_wrong {
                    report.refcount_divergences += 1;
                }
                if referrers_wrong {
                    report.reverse_ref_divergences += 1;
                }
                Ok(false)
            }
            ChunkFix::Remove => {
                report.refcount_divergences += 1;
                self.chunk_remove(phys)?;
                self.reverse_refs_remove(phys)?;
                report.divergences_corrected += 1;
                Ok(true)
            }
            ChunkFix::Rewrite {
                refcount,
                referrers,
                listed,
                refcount_wrong,
                referrers_wrong,
                incomplete,
            } => {
                if refcount_wrong {
                    report.refcount_divergences += 1;
                    self.chunk_put(
                        phys,
                        &ChunkRecord {
                            refcount,
                            ..*record
                        },
                    )?;
                    report.divergences_corrected += 1;
                }
                if referrers_wrong {
                    report.reverse_ref_divergences += 1;
                    self.reverse_refs_put(phys, &referrers[..listed])?;
                    // A referrer set the stored record did not name is only
                    // whole once the extents that claim the block have been
                    // walked for the missing identities.
                    if !incomplete {
                        report.divergences_corrected += 1;
                    }
                }
                Ok(true)
            }
        }
    }

    /// Whether the extent tree of `referrer`'s inode really maps its logical
    /// block to `phys`.
    ///
    /// This is the bounded half of the reconcile: one floor lookup in one
    /// extent tree, with no state carried between referrers.
    fn referrer_backs_block(&mut self, referrer: Referrer, phys: u64) -> Result<bool, DriverError> {
        let (ino, logical) = referrer;
        let Some(value) = self.btree_get(self.inode_tree_root, u64::from(ino), inode_spec())?
        else {
            return Ok(false);
        };
        let Some(inode) = Inode::decode(&value)? else {
            return Ok(false);
        };
        if inode.kind.content_is_metadata() {
            return Ok(false);
        }
        let Some((start, ev)) =
            self.btree_get_floor(inode.extent_root, logical, extent_spec(ino))?
        else {
            return Ok(false);
        };
        let ext = Extent::decode(&ev, self.total_blocks)?;
        if ext.compressed {
            // A cluster's referrer names the cluster's own logical start.
            return Ok(start == logical && ext.phys == phys);
        }
        if logical >= start.saturating_add(ext.len) {
            return Ok(false);
        }
        Ok(ext.phys.saturating_add(logical - start) == phys)
    }

    /// Walk the extents again and add every claim the window's shared blocks
    /// are missing from their referrer lists.
    ///
    /// Only reached when [`Self::verify_chunk_range`] found a list the stored
    /// record could not complete on its own, so a healthy volume never pays
    /// for it. Adding a referrer copy-on-writes the reverse-reference tree
    /// alone, so neither walk below is disturbed by it. Returns whether any
    /// list changed.
    fn restore_referrers(
        &mut self,
        claims: &mut ScratchArray,
        report: &mut ScrubReport,
    ) -> Result<bool, DriverError> {
        let mut walk = TreeWalk::new(self.block_size)?;
        let mut extent_walk = TreeWalk::new(self.block_size)?;
        let mut changed = false;
        while self.btree_next_leaf(self.inode_tree_root, inode_spec(), &mut walk)? {
            for (ino_key, value) in walk.entries() {
                let Some(inode) = Inode::decode(value)? else {
                    continue;
                };
                if inode.kind.content_is_metadata() {
                    continue;
                }
                let ino = u32::try_from(ino_key).map_err(|_| DriverError::DeviceFault)?;
                if self.restore_inode_referrers(ino, &inode, &mut extent_walk, claims, report)? {
                    changed = true;
                }
            }
        }
        Ok(changed)
    }

    /// [`Self::restore_referrers`] for one inode's extents.
    fn restore_inode_referrers(
        &mut self,
        ino: u32,
        inode: &Inode,
        walk: &mut TreeWalk,
        claims: &mut ScratchArray,
        report: &mut ScrubReport,
    ) -> Result<bool, DriverError> {
        let spec = extent_spec(ino);
        let (base, end) = (claims.base(), claims.window_end());
        let mut changed = false;
        walk.restart();
        while self.btree_next_leaf(inode.extent_root, spec, walk)? {
            for (start, value) in walk.entries() {
                let ext = Extent::decode(value, self.total_blocks)?;
                let (lo, hi) = if ext.compressed {
                    (ext.phys, ext.phys.saturating_add(1))
                } else {
                    (ext.phys, ext.phys.saturating_add(ext.len))
                };
                for block in lo.max(base)..hi.min(end) {
                    if self.scratch_get(claims, block)? < 2 {
                        continue;
                    }
                    let logical = if ext.compressed {
                        start
                    } else {
                        start.saturating_add(block - ext.phys)
                    };
                    if self.list_referrer(block, (ino, logical), report)? {
                        changed = true;
                    }
                }
            }
        }
        Ok(changed)
    }

    /// Add `referrer` to `phys`'s reverse-reference record unless it is
    /// already named there, or the record is already full — more claims than
    /// the format can name leaves the divergence standing, reported rather
    /// than papered over. Returns whether the record changed.
    fn list_referrer(
        &mut self,
        phys: u64,
        referrer: Referrer,
        report: &mut ScrubReport,
    ) -> Result<bool, DriverError> {
        let mut stored = [(0u32, 0u64); REVERSE_REF_CAP];
        let held = self.reverse_refs_into(phys, &mut stored)?;
        if held >= REVERSE_REF_CAP || stored[..held].contains(&referrer) {
            return Ok(false);
        }
        stored[held] = referrer;
        self.reverse_refs_put(phys, &stored[..=held])?;
        report.divergences_corrected += 1;
        Ok(true)
    }

    /// Decode `phys`'s reverse-reference record into `out`, returning how many
    /// referrers it names (`0` when the block records none).
    fn reverse_refs_into(
        &mut self,
        phys: u64,
        out: &mut [Referrer; REVERSE_REF_CAP],
    ) -> Result<usize, DriverError> {
        let Some(value) = self.btree_get(
            self.reverse_ref_tree_root,
            phys,
            crate::dedupe::reverse_ref_spec(),
        )?
        else {
            return Ok(0);
        };
        decode_reverse_ref_into(&value, out).ok_or(DriverError::DeviceFault)
    }
}
