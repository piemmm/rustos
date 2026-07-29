//! The tier's operations: compress-out, restore (demand fault,
//! cluster, warm-up), and the shared frame plumbing they build on.
//!
//! Split from the type definitions in [`super`] to keep each file
//! focused; this module is the only place tier state is mutated.

use super::{
    bump, decompression_floor, eligibility, entry_aad, free_bytes, log_ramzip_failure, open_page,
    ramzip_handoff, seal_page, slice_within, zero_frame, CompressRefusal, Entry, FaultError,
    MemoryPressure, OpenFailure, Page, PageCandidate, PageTable, PressureBand, Ramzip,
    RamzipAuditEvent, RamzipHandoff, RecentFault, SealFailure, VmContext, WarmOutcome,
    CLUSTER_EVENT_WINDOW, CLUSTER_MAX_PAGES, CLUSTER_RADIUS, ENTRY_METADATA_BYTES,
    FORBIDDEN_FLAG_BITS, MAX_COMPRESSED_LEN, PAGE_SIZE, RECENT_FAULTS, WARM_BATCH_PAGES,
    WARM_RADIUS,
};
use crate::vmm::MapFlags;

/// Whether opportunistic restores may run at all: normal pressure
/// *and* free memory above the warm-up start watermark (hysteresis
/// against the stop watermark is applied per page in the step loops).
fn warm_gate<P: PageTable>(pressure: &MemoryPressure, ctx: &VmContext<'_, P>) -> bool {
    pressure.sample() == PressureBand::Normal
        && free_bytes(ctx) > pressure.thresholds().warmup_start()
}

/// How a restore was initiated: a demand fault records locality and
/// feeds thrash detection; cluster and warm-up restores are
/// opportunistic and feed only their own counters.
#[derive(Copy, Clone, Eq, PartialEq)]
enum RestoreKind {
    DemandFault,
    Cluster,
    Warm,
}

/// Rebuild a [`Page`] from its page number. `None` if the number is
/// outside the address space (corrupt hint, never a panic).
fn page_from_number(number: u64) -> Option<Page> {
    let addr = number.checked_mul(PAGE_SIZE as u64)?;
    Page::from_addr(crate::vmm::VirtAddr::new(addr)).ok()
}

/// Borrow the direct-map bytes of `frame` as one page.
///
/// Fails closed (`None`) if the frame is outside the direct map.
fn frame_page<'a, P: PageTable>(
    ctx: &VmContext<'_, P>,
    frame: crate::frame::Frame,
) -> Option<&'a mut [u8; PAGE_SIZE]> {
    let ptr = ctx.physmap.translate(frame.start(), PAGE_SIZE)?;
    // SAFETY: `translate` proved the pointer is valid for `PAGE_SIZE`
    // bytes inside the kernel direct map. The tier's concurrency
    // contract (module docs) guarantees the owning task is not running,
    // and the frame is either still privately mapped in the paused
    // task's space (compress read) or freshly allocated and not yet
    // mapped anywhere (restore write), so nothing aliases the window.
    let slice = unsafe { slice_within(ptr.as_ptr(), PAGE_SIZE, 0, PAGE_SIZE) }?;
    slice.try_into().ok()
}

impl Ramzip {
    /// Compress `page` of the context's address space into the tier
    /// and free its frame.
    ///
    /// The full gate order (each refusal typed, the page untouched):
    /// pressure handoff → poison check → thrash check → eligibility →
    /// mapping lookup and flag defence → band cap → per-task share →
    /// decompression floor → compression acceptance. Only after the
    /// sealed entry is stored and charged is the page unmapped and its
    /// frame scrubbed (zero-on-free) and returned.
    ///
    /// `reclaimable_residue` is the clean + transform cache bytes still
    /// resident, from the caller's reclaim accounting: compression
    /// waits for cheaper reclaim first.
    ///
    /// # Errors
    ///
    /// See [`CompressRefusal`]; feed the refusal and the sampled band
    /// to [`escalate_refusal`](crate::escalate_refusal) for the
    /// deterministic next step.
    pub fn compress_out<P: PageTable>(
        &mut self,
        pressure: &MemoryPressure,
        reclaimable_residue: usize,
        ctx: &mut VmContext<'_, P>,
        page: Page,
        task: u64,
        candidate: &PageCandidate,
    ) -> Result<(), CompressRefusal> {
        let (frame, flags) =
            self.admit_compress(pressure, reclaimable_residue, ctx, page, task, *candidate)?;
        let key = (ctx.space_id, page.number());

        let Some(plaintext) = frame_page(ctx, frame) else {
            return Err(CompressRefusal::PhysUnmapped);
        };
        let aad = entry_aad(ctx.space_id, page.number(), flags);
        let blob = match seal_page(
            &self.key,
            &mut self.nonces,
            &aad,
            plaintext,
            MAX_COMPRESSED_LEN,
        ) {
            Ok(blob) => blob,
            Err(SealFailure::Incompressible) => {
                bump(&mut self.ledger.counters_mut().rejected_incompressible);
                return Err(CompressRefusal::Incompressible);
            }
            Err(SealFailure::Alloc) => return Err(CompressRefusal::OutOfMemory),
            Err(SealFailure::NonceExhausted) => return Err(CompressRefusal::NonceExhausted),
            Err(SealFailure::Seal) => return Err(CompressRefusal::Seal),
        };

        let compressed = blob.ciphertext.len();
        let stored = blob.stored_len();
        if self
            .ledger
            .charge(task, PAGE_SIZE, compressed, stored, ENTRY_METADATA_BYTES)
            .is_err()
        {
            self.poisoned = true;
            return Err(CompressRefusal::Poisoned);
        }
        let sealed_at = self.tick();
        self.entries.insert(
            key,
            Entry {
                task,
                flags,
                sealed_at,
                charged_compressed: compressed,
                charged_stored: stored,
                blob,
            },
        );

        let freed = match ctx.space.unmap(page) {
            Ok(freed) => freed,
            Err(e) => {
                // Roll the entry back: the page is still mapped and
                // authoritative, so the tier must not hold a copy.
                self.entries.remove(&key);
                if self
                    .ledger
                    .release(task, PAGE_SIZE, compressed, stored, ENTRY_METADATA_BYTES)
                    .is_err()
                {
                    self.poisoned = true;
                }
                return Err(CompressRefusal::PageTable(e));
            }
        };

        // Zero-on-free: the frame held user bytes. A scrub or free
        // failure keeps the entry (the data is safe in the tier) and
        // surfaces the defect; the frame is deliberately not recycled
        // unscrubbed.
        if zero_frame(ctx.physmap, freed).is_err() || ctx.frames.free(freed).is_err() {
            return Err(CompressRefusal::FrameRelease);
        }
        bump(&mut self.ledger.counters_mut().accepted);
        Ok(())
    }

    /// The admission gates that run before any byte of `page` is
    /// touched, in the audited order: pressure handoff → poison →
    /// thrash → eligibility → mapping lookup and flag defence → band
    /// cap → per-task share → decompression floor → double-entry
    /// bookkeeping defence. Every refusal is typed and counted.
    fn admit_compress<P: PageTable>(
        &mut self,
        pressure: &MemoryPressure,
        reclaimable_residue: usize,
        ctx: &VmContext<'_, P>,
        page: Page,
        task: u64,
        candidate: PageCandidate,
    ) -> Result<(crate::frame::Frame, MapFlags), CompressRefusal> {
        bump(&mut self.ledger.counters_mut().attempts);
        let band = pressure.sample();
        if ramzip_handoff(band, reclaimable_residue) != RamzipHandoff::CompressColdAnonymous {
            bump(&mut self.ledger.counters_mut().rejected_policy);
            return Err(CompressRefusal::PressurePolicy);
        }
        if self.poisoned {
            return Err(CompressRefusal::Poisoned);
        }
        if self.thrash.is_thrashing(task, self.event_clock) {
            bump(&mut self.ledger.counters_mut().rejected_thrash);
            return Err(CompressRefusal::TaskThrashing);
        }
        if let Err(reason) = eligibility(&candidate) {
            bump(&mut self.ledger.counters_mut().rejected_ineligible);
            return Err(CompressRefusal::Ineligible(reason));
        }

        let Some((frame, flags)) = ctx.space.translate(page) else {
            return Err(CompressRefusal::NotMapped);
        };
        if flags.bits() & FORBIDDEN_FLAG_BITS != 0 {
            bump(&mut self.ledger.counters_mut().rejected_ineligible);
            return Err(CompressRefusal::ForbiddenMapping);
        }

        // Worst-case cost pre-checks: a stored entry is always strictly
        // smaller than one page (the acceptance bound guarantees it),
        // so PAGE_SIZE is a sound, conservative bound for the cap, the
        // fair share, and the transient reserve cost alike.
        let cap = self.caps.band_cap(band);
        if self.ledger.footprint().saturating_add(PAGE_SIZE) > cap {
            bump(&mut self.ledger.counters_mut().rejected_cap);
            return Err(CompressRefusal::CapReached);
        }
        let share = self.caps.task_share(band);
        let task_stored = self.ledger.task_usage(task).stored_bytes;
        if task_stored.saturating_add(PAGE_SIZE) > share {
            bump(&mut self.ledger.counters_mut().rejected_task_share);
            return Err(CompressRefusal::TaskShareReached);
        }
        let floor = decompression_floor(pressure.thresholds().reserve());
        if free_bytes(ctx).saturating_sub(PAGE_SIZE) <= floor {
            bump(&mut self.ledger.counters_mut().rejected_reserve);
            return Err(CompressRefusal::ReserveProtected);
        }

        if self.entries.contains_key(&(ctx.space_id, page.number())) {
            // A mapped page can never also have a compressed entry;
            // the books cannot be trusted any further.
            self.poisoned = true;
            return Err(CompressRefusal::Poisoned);
        }
        Ok((frame, flags))
    }

    /// Restore the compressed entry for `page` on a demand fault:
    /// move-only (the blob is deleted once the page is mapped), feeds
    /// thrash detection and the warm-up locality hints.
    ///
    /// # Errors
    ///
    /// See [`FaultError`]. `Authentication` / `Corrupt` mean the page
    /// is unrecoverable: the caller escalates through the VM policy
    /// (the task cannot continue without the page), and no plaintext
    /// was produced.
    pub fn fault_in<P: PageTable>(
        &mut self,
        ctx: &mut VmContext<'_, P>,
        page: Page,
    ) -> Result<(), FaultError> {
        self.restore_entry(ctx, page, RestoreKind::DemandFault)
    }

    /// Opportunistically restore entries adjacent to a page just
    /// restored by [`Self::fault_in`] (`plans/SWAPSWAPSWAP.md` section
    /// 11): same space, within `CLUSTER_RADIUS` pages, sealed within
    /// `CLUSTER_EVENT_WINDOW` events of the faulted entry, at most
    /// `CLUSTER_MAX_PAGES` pages — and only while memory is
    /// comfortably above the warm-up threshold with the decompression
    /// floor protected. Failures never propagate: the original fault
    /// already succeeded, so cluster work is best-effort by design.
    ///
    /// Returns the number of pages restored.
    pub fn cluster_after_fault<P: PageTable>(
        &mut self,
        pressure: &MemoryPressure,
        ctx: &mut VmContext<'_, P>,
        around: Page,
    ) -> usize {
        if !warm_gate(pressure, ctx) {
            return 0;
        }
        // The faulted page's locality hint carries the seal time the
        // cluster window is measured against.
        let Some(hint) = self
            .recent_faults
            .iter()
            .flatten()
            .find(|hint| hint.space == ctx.space_id && hint.page_number == around.number())
        else {
            return 0;
        };
        let sealed_at = hint.sealed_at;
        let candidates = self.nearby_entries(
            ctx.space_id,
            around.number(),
            CLUSTER_RADIUS,
            CLUSTER_MAX_PAGES,
            Some(sealed_at),
        );

        let floor = decompression_floor(pressure.thresholds().reserve());
        let mut restored = 0;
        for number in candidates {
            if pressure.sample() != PressureBand::Normal
                || free_bytes(ctx).saturating_sub(PAGE_SIZE) <= floor
            {
                break;
            }
            let Some(page) = page_from_number(number) else {
                continue;
            };
            match self.restore_entry(ctx, page, RestoreKind::Cluster) {
                Ok(()) => restored += 1,
                // Out of frames or an unmapped direct map will not
                // improve within this event; stop. Any other failure
                // affected one entry only; try the next_nonce.
                Err(FaultError::OutOfMemory | FaultError::PhysUnmapped) => break,
                Err(_) => {}
            }
        }
        restored
    }

    /// One bounded background warm-up step (`plans/SWAPSWAPSWAP.md`
    /// section 12): restore up to `WARM_BATCH_PAGES` entries near
    /// recent demand faults in the context's space, only while free
    /// memory is comfortably above the warm-up threshold, re-checking
    /// the gate before every page and stopping instantly when it
    /// closes. Candidates without fault-locality evidence are never
    /// touched — keeping cold pages compressed is the design, so an
    /// idle tier reports [`WarmOutcome::NothingToDo`].
    ///
    /// The caller controls the cadence (there is no timer here); each
    /// step is budgeted by pages, and every restored page is counted.
    pub fn warm_step<P: PageTable>(
        &mut self,
        pressure: &MemoryPressure,
        ctx: &mut VmContext<'_, P>,
    ) -> WarmOutcome {
        bump(&mut self.ledger.counters_mut().warm_attempts);
        if !warm_gate(pressure, ctx) {
            bump(&mut self.ledger.counters_mut().warm_stopped);
            return WarmOutcome::Stopped;
        }

        let hints: [Option<RecentFault>; RECENT_FAULTS] = self.recent_faults;
        let mut candidates = alloc::vec::Vec::new();
        for hint in hints.iter().flatten() {
            if hint.space != ctx.space_id {
                continue;
            }
            let nearby = self.nearby_entries(
                ctx.space_id,
                hint.page_number,
                WARM_RADIUS,
                WARM_BATCH_PAGES,
                None,
            );
            for number in nearby {
                if !candidates.contains(&number) {
                    candidates.push(number);
                }
            }
            if candidates.len() >= WARM_BATCH_PAGES {
                candidates.truncate(WARM_BATCH_PAGES);
                break;
            }
        }
        if candidates.is_empty() {
            return WarmOutcome::NothingToDo;
        }

        let stop = pressure.thresholds().warmup_stop();
        let floor = decompression_floor(pressure.thresholds().reserve());
        let mut restored = 0;
        for number in candidates {
            if pressure.sample() != PressureBand::Normal
                || free_bytes(ctx) <= stop
                || free_bytes(ctx).saturating_sub(PAGE_SIZE) <= floor
            {
                bump(&mut self.ledger.counters_mut().warm_stopped);
                return WarmOutcome::Stopped;
            }
            let Some(page) = page_from_number(number) else {
                continue;
            };
            match self.restore_entry(ctx, page, RestoreKind::Warm) {
                Ok(()) => restored += 1,
                Err(FaultError::OutOfMemory | FaultError::PhysUnmapped) => {
                    bump(&mut self.ledger.counters_mut().warm_stopped);
                    return WarmOutcome::Stopped;
                }
                Err(_) => {}
            }
        }
        WarmOutcome::Restored(restored)
    }

    /// Compressed entries of `space` within `radius` pages of
    /// `center`, nearest first, excluding `center` itself, bounded by
    /// `limit`. With `sealed_near`, only entries sealed within
    /// [`CLUSTER_EVENT_WINDOW`] events of that seal time qualify.
    fn nearby_entries(
        &self,
        space: u64,
        center: u64,
        radius: u64,
        limit: usize,
        sealed_near: Option<u64>,
    ) -> alloc::vec::Vec<u64> {
        let low = center.saturating_sub(radius);
        let high = center.saturating_add(radius);
        let mut found: alloc::vec::Vec<u64> = self
            .entries
            .range((space, low)..=(space, high))
            .filter(|((_, number), entry)| {
                *number != center
                    && sealed_near
                        .is_none_or(|near| entry.sealed_at.abs_diff(near) <= CLUSTER_EVENT_WINDOW)
            })
            .map(|((_, number), _)| *number)
            .collect();
        found.sort_by_key(|number| number.abs_diff(center));
        found.truncate(limit);
        found
    }

    /// The one restore path all three initiators share: allocate a
    /// frame, authenticate + decrypt + decompress into it, map it, and
    /// delete the blob (move-only). Every failure is typed and
    /// fail-closed; a lost entry (authentication or decode failure) is
    /// audit-logged and never yields plaintext.
    fn restore_entry<P: PageTable>(
        &mut self,
        ctx: &mut VmContext<'_, P>,
        page: Page,
        kind: RestoreKind,
    ) -> Result<(), FaultError> {
        let key = (ctx.space_id, page.number());
        let Some(entry) = self.entries.get(&key) else {
            return Err(FaultError::NoEntry);
        };
        let (task, flags, sealed_at) = (entry.task, entry.flags, entry.sealed_at);
        // Release exactly what was charged, never a figure recomputed
        // from the blob: a corrupted blob must not unbalance the books.
        let compressed = entry.charged_compressed;
        let stored = entry.charged_stored;
        if ctx.space.translate(page).is_some() {
            return Err(FaultError::AlreadyMapped);
        }

        let Ok(frame) = ctx.frames.alloc() else {
            return Err(FaultError::OutOfMemory);
        };
        let Some(out) = frame_page(ctx, frame) else {
            // The freshly allocated frame holds no plaintext yet; it
            // can be returned without a scrub.
            let _ = ctx.frames.free(frame);
            return Err(FaultError::PhysUnmapped);
        };

        let aad = entry_aad(ctx.space_id, page.number(), flags);
        // Reborrow for the blob; the earlier copies keep the metadata.
        let Some(entry) = self.entries.get(&key) else {
            let _ = ctx.frames.free(frame);
            return Err(FaultError::NoEntry);
        };
        if let Err(failure) = open_page(&self.key, &aad, &entry.blob, out) {
            // The entry is unrecoverable: discard it, log the loss,
            // return the (zeroed-by-open) frame, and fail closed.
            self.entries.remove(&key);
            if self
                .ledger
                .release(task, PAGE_SIZE, compressed, stored, ENTRY_METADATA_BYTES)
                .is_err()
            {
                self.poisoned = true;
            }
            let _ = ctx.frames.free(frame);
            let (event, counter_is_auth) = match failure {
                OpenFailure::Authentication => (RamzipAuditEvent::AuthenticationFailure, true),
                OpenFailure::Corrupt | OpenFailure::Decode => {
                    (RamzipAuditEvent::EntryCorrupt, false)
                }
            };
            log_ramzip_failure(ctx.sink, event, ctx.space_id, page.number(), task);
            if counter_is_auth {
                bump(&mut self.ledger.counters_mut().auth_failures);
                return Err(FaultError::Authentication);
            }
            bump(&mut self.ledger.counters_mut().decode_failures);
            return Err(FaultError::Corrupt);
        }

        if let Err(e) = ctx.space.map(page, frame, flags) {
            // The frame briefly held restored plaintext: scrub before
            // returning it. The entry is retained; the data is intact.
            let _ = zero_frame(ctx.physmap, frame);
            let _ = ctx.frames.free(frame);
            return Err(FaultError::PageTable(e));
        }

        // Move-only: the blob is deleted the moment the page is live.
        self.entries.remove(&key);
        if self
            .ledger
            .release(task, PAGE_SIZE, compressed, stored, ENTRY_METADATA_BYTES)
            .is_err()
        {
            // The page is restored and correct; only the books are
            // broken. Poison admission and keep serving restores.
            self.poisoned = true;
        }
        let now = self.tick();
        match kind {
            RestoreKind::DemandFault => {
                bump(&mut self.ledger.counters_mut().fault_ins);
                if self.thrash.on_restore(task, sealed_at, now) {
                    bump(&mut self.ledger.counters_mut().thrash_detected);
                }
                self.recent_faults[self.next_fault_slot] = Some(RecentFault {
                    space: ctx.space_id,
                    page_number: page.number(),
                    sealed_at,
                });
                self.next_fault_slot = (self.next_fault_slot + 1) % RECENT_FAULTS;
            }
            RestoreKind::Cluster => {
                bump(&mut self.ledger.counters_mut().cluster_restored);
            }
            RestoreKind::Warm => {
                bump(&mut self.ledger.counters_mut().warm_restored);
            }
        }
        Ok(())
    }

    /// Drop every compressed entry belonging to `space_id`, releasing
    /// its ledger charges and freeing its sealed blobs, and return the
    /// number purged.
    ///
    /// Called when an address space is torn down (its owning
    /// [`crate::live::LiveSpace`] drops): a global pool must not keep a
    /// dead task's entries — their space id would never fault them back
    /// in, so their RAM and ledger charge would leak. The sealed blobs
    /// are freed as their `Entry` values drop (their bytes are
    /// ciphertext, already scrubbed of plaintext at seal time), so no
    /// separate zeroisation is needed here.
    pub fn purge_space(&mut self, space_id: u64) -> usize {
        // Collect the doomed keys first (a range over the space's id),
        // then remove and un-charge each — the two-phase walk avoids
        // mutating the map while iterating it.
        let doomed: alloc::vec::Vec<u64> = self
            .entries
            .range((space_id, u64::MIN)..=(space_id, u64::MAX))
            .map(|((_, page), _)| *page)
            .collect();
        let mut purged = 0;
        for page in doomed {
            if let Some(entry) = self.entries.remove(&(space_id, page)) {
                if self
                    .ledger
                    .release(
                        entry.task,
                        PAGE_SIZE,
                        entry.charged_compressed,
                        entry.charged_stored,
                        ENTRY_METADATA_BYTES,
                    )
                    .is_err()
                {
                    // The books no longer balance; poison admission but
                    // keep purging (the memory is still being freed).
                    self.poisoned = true;
                }
                purged += 1;
            }
        }
        purged
    }
}

#[cfg(any(test, feature = "host-tests"))]
impl Ramzip {
    /// Test-only: flip one bit of a stored entry's sealed form.
    ///
    /// `offset` addresses the logical concatenation
    /// `nonce ‖ tag ‖ ciphertext`, so a fuzz harness can tamper any
    /// field. Returns `false` if no entry exists or the offset is out
    /// of range.
    pub fn tamper_entry(&mut self, space_id: u64, page: Page, offset: usize) -> bool {
        use tairix_crypto::aead::{AEAD_NONCE_LEN, AEAD_TAG_LEN};
        let Some(entry) = self.entries.get_mut(&(space_id, page.number())) else {
            return false;
        };
        let blob = &mut entry.blob;
        if offset < AEAD_NONCE_LEN {
            blob.nonce[offset] ^= 0x01;
            return true;
        }
        let offset = offset - AEAD_NONCE_LEN;
        if offset < AEAD_TAG_LEN {
            blob.tag[offset] ^= 0x01;
            return true;
        }
        let offset = offset - AEAD_TAG_LEN;
        match blob.ciphertext.get_mut(offset) {
            Some(byte) => {
                *byte ^= 0x01;
                true
            }
            None => false,
        }
    }

    /// Test-only: truncate a stored entry's ciphertext to `len` bytes,
    /// so metadata validation paths are fuzzable. Returns `false` if
    /// no entry exists or `len` is not shorter than the ciphertext.
    pub fn truncate_entry(&mut self, space_id: u64, page: Page, len: usize) -> bool {
        let Some(entry) = self.entries.get_mut(&(space_id, page.number())) else {
            return false;
        };
        if len >= entry.blob.ciphertext.len() {
            return false;
        }
        entry.blob.ciphertext.truncate(len);
        true
    }

    /// Test-only: the sealed length of an entry's ciphertext, for
    /// driving [`Self::tamper_entry`] across the whole sealed form.
    #[must_use]
    pub fn entry_sealed_len(&self, space_id: u64, page: Page) -> Option<usize> {
        self.entries
            .get(&(space_id, page.number()))
            .map(|entry| entry.blob.stored_len())
    }
}
