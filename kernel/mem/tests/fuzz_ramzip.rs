//! Deterministic fuzz harness for the `ramzip` compressed-entry
//! restore path (`plans/SWAPSWAPSWAP.md` SWAP2).
//!
//! A sealed entry lives in kernel RAM, but the restore path still
//! treats its bytes as untrusted: RAM corruption, a stray DMA write, or
//! a logic defect must never yield forged plaintext. The harness
//! drives random compress → tamper/truncate → fault cycles through the
//! tier's *public* operations over the host page-table and physical-map
//! doubles and asserts, for arbitrary corruption of any sealed field
//! (nonce, tag, ciphertext) and arbitrary metadata truncation:
//!
//! 1. No input panics.
//! 2. An untampered entry round-trips to the exact page bytes.
//! 3. Any tampering fails closed (`Authentication` / `Corrupt`), maps
//!    nothing, and discards the entry.
//! 4. The ledger balances back to zero after every cycle: no frame,
//!    entry, or metadata leak no matter how the restore ended.
//!
//! Mirrors `fuzz_swap`'s seed/budget discipline: a plain `cargo test`
//! runs the fixed smoke sweep once from a fresh, logged seed;
//! `cargo xtask fuzz` extends the same PRNG stream until the exported
//! wall-clock budget elapses.

use rustos_kernel_mem::{
    AddressSpace, BootMemoryMap, CompressRefusal, EntropySource, FaultError, FrameAllocator,
    FreeMemorySource, HostPageTable, MapFlags, MemoryPressure, MemoryRegion, Page, PageCandidate,
    PhysAddr, PhysMap, PressureBand, Ramzip, RamzipCaps, RegionKind, SealError, SimPhysMap,
    VirtAddr, VmContext, PAGE_SIZE,
};

const SMOKE_ITERATIONS: u64 = 2_000;
const TOTAL_FRAMES: usize = 512;
const SPACE: u64 = 1;

/// xor-shift* PRNG. Deterministic, fast, zero-allocation.
struct Rng(u64);
impl Rng {
    const fn new(seed: u64) -> Self {
        Self(seed | 1)
    }
    fn next_u64(&mut self) -> u64 {
        let mut x = self.0;
        x ^= x << 13;
        x ^= x >> 7;
        x ^= x << 17;
        self.0 = x;
        x.wrapping_mul(0x2545_F491_4F6C_DD1D)
    }
    fn byte(&mut self) -> u8 {
        (self.next_u64() & 0xFF) as u8
    }
}

/// PRNG-seeded entropy source (test-only; not a real CSPRNG).
struct RngEntropy(Rng);
impl EntropySource for RngEntropy {
    fn fill(&mut self, out: &mut [u8]) -> Result<(), SealError> {
        for b in out.iter_mut() {
            *b = self.0.byte();
        }
        Ok(())
    }
}

/// Audit sink that discards records.
struct NullSink;
impl rustos_log::Sink for NullSink {
    fn write_event(&self, _event: &rustos_log::Event<'_>) {}
}
static NULL_SINK: NullSink = NullSink;

/// Hold or release frames until the gauge reads exactly `Moderate`:
/// compression frees a frame each cycle and a failed restore does not
/// re-take it, so the pinned band drifts without this rebalance.
fn rebalance_to_moderate(
    pressure: &MemoryPressure,
    frames: &'static FrameAllocator,
    held: &mut Vec<rustos_kernel_mem::Frame>,
) {
    while pressure.sample() == PressureBand::Normal || pressure.sample() == PressureBand::Mild {
        held.push(frames.alloc().expect("pressure frame"));
    }
    while pressure.sample() != PressureBand::Moderate {
        frames
            .free(held.pop().expect("held frame to release"))
            .expect("free held frame");
    }
}

/// Map one page of patterned (compressible) content — run length and
/// seed vary so blob sizes differ per cycle — and snapshot it.
fn map_patterned_page(
    rng: &mut Rng,
    frames: &'static FrameAllocator,
    physmap: &SimPhysMap,
    space: &mut AddressSpace<HostPageTable>,
    flags: MapFlags,
) -> (Page, Vec<u8>) {
    let number = 16 + rng.next_u64() % 64;
    let page = Page::from_addr(VirtAddr::new(number * PAGE_SIZE as u64)).expect("page");
    let frame = frames.alloc().expect("page frame");
    let run = 32 + usize::try_from(rng.next_u64() % 224).expect("run");
    let seed = rng.byte();
    {
        let ptr = physmap.translate(frame.start(), PAGE_SIZE).expect("frame");
        // SAFETY: the window is exactly one page inside the
        // simulator's storage and nothing else borrows it.
        let bytes = unsafe { core::slice::from_raw_parts_mut(ptr.as_ptr(), PAGE_SIZE) };
        for (i, b) in bytes.iter_mut().enumerate() {
            *b = seed.wrapping_add(u8::try_from((i / run) % 13).expect("small"));
        }
    }
    space.map(page, frame, flags).expect("map");
    let original: Vec<u8> = {
        let ptr = physmap.translate(frame.start(), PAGE_SIZE).expect("frame");
        // SAFETY: as above; read-only snapshot before compression.
        unsafe { core::slice::from_raw_parts(ptr.as_ptr(), PAGE_SIZE) }.to_vec()
    };
    (page, original)
}

/// Corrupt the sealed entry for `page` — a bit-flip anywhere in the
/// sealed form, a metadata truncation, or (half the time) nothing.
/// Returns whether the entry was corrupted.
fn maybe_corrupt_entry(rng: &mut Rng, ramzip: &mut Ramzip, page: Page) -> bool {
    let sealed_len = ramzip.entry_sealed_len(SPACE, page).expect("entry");
    match rng.next_u64() % 4 {
        0 => {
            let off = usize::try_from(rng.next_u64() % sealed_len as u64).expect("off");
            assert!(ramzip.tamper_entry(SPACE, page, off));
            true
        }
        1 => {
            let cipher_len = sealed_len - 28;
            let len = usize::try_from(rng.next_u64() % cipher_len as u64).expect("len");
            assert!(ramzip.truncate_entry(SPACE, page, len));
            true
        }
        _ => false,
    }
}

#[test]
fn fuzz_ramzip_restore_is_fail_closed() {
    let mut rng = Rng::new(rustos_fuzzseed::start(
        "fuzz_ramzip_restore_is_fail_closed",
        rustos_fuzzseed::FUZZ_SEED_ENV,
    ));

    let mut map = BootMemoryMap::new();
    map.push(MemoryRegion {
        start: PhysAddr::new(0),
        length: (TOTAL_FRAMES * PAGE_SIZE) as u64,
        kind: RegionKind::Usable,
    });
    let frames: &'static FrameAllocator =
        Box::leak(Box::new(FrameAllocator::new(&map).expect("allocator")));
    let pressure = MemoryPressure::over(frames);
    let physmap = SimPhysMap::new(PhysAddr::new(0), TOTAL_FRAMES * PAGE_SIZE);
    let mut space: AddressSpace<HostPageTable> = AddressSpace::new(HostPageTable::new());
    let caps = RamzipCaps::from_physical(FreeMemorySource::total_bytes(frames));
    let mut ramzip = Ramzip::new(caps, &mut RngEntropy(Rng::new(7))).expect("tier");

    // Pin the gauge at moderate pressure so the compression gate is
    // open; the harness rebalances the held frames each cycle.
    let mut held = Vec::new();
    while pressure.sample() != PressureBand::Moderate {
        held.push(frames.alloc().expect("pressure frame"));
    }

    let flags = MapFlags::READ | MapFlags::WRITE | MapFlags::USER;
    let mut round_trips = 0u64;
    let mut tamper_rejected = 0u64;
    let deadline = rustos_fuzzseed::budget_deadline(rustos_fuzzseed::FUZZ_BUDGET_ENV);
    loop {
        for _ in 0..SMOKE_ITERATIONS {
            rebalance_to_moderate(&pressure, frames, &mut held);
            let (page, original) =
                map_patterned_page(&mut rng, frames, &physmap, &mut space, flags);

            let mut ctx = VmContext {
                space_id: SPACE,
                space: &mut space,
                physmap: &physmap,
                frames,
                sink: &NULL_SINK,
            };
            // Rotate the owning task so the (correct) thrash detector
            // rarely trips; when it does, that is a legal refusal.
            let task = rng.next_u64() % 32;
            match ramzip.compress_out(
                &pressure,
                0,
                &mut ctx,
                page,
                task,
                &PageCandidate::cold_anonymous(),
            ) {
                Ok(()) => {}
                Err(CompressRefusal::Incompressible | CompressRefusal::TaskThrashing) => {
                    // Legal refusals: the page stays mapped; clean up.
                    let freed = space.unmap(page).expect("unmap refused page");
                    frames.free(freed).expect("free refused frame");
                    continue;
                }
                Err(other) => panic!("unexpected refusal: {other:?}"),
            }

            let corrupted = maybe_corrupt_entry(&mut rng, &mut ramzip, page);

            let mut ctx = VmContext {
                space_id: SPACE,
                space: &mut space,
                physmap: &physmap,
                frames,
                sink: &NULL_SINK,
            };
            let verdict = ramzip.fault_in(&mut ctx, page);
            if corrupted {
                assert!(
                    matches!(
                        verdict,
                        Err(FaultError::Authentication | FaultError::Corrupt)
                    ),
                    "corruption must fail closed, got {verdict:?}"
                );
                assert!(
                    space.translate(page).is_none(),
                    "no plaintext may be mapped from a corrupt entry"
                );
                tamper_rejected += 1;
            } else {
                verdict.expect("untampered restore");
                let (restored, restored_flags) = space.translate(page).expect("mapped");
                assert_eq!(restored_flags, flags, "flags preserved");
                let ptr = physmap
                    .translate(restored.start(), PAGE_SIZE)
                    .expect("frame");
                // SAFETY: as above; read-only comparison.
                let bytes = unsafe { core::slice::from_raw_parts(ptr.as_ptr(), PAGE_SIZE) };
                assert_eq!(bytes, original.as_slice(), "round-trip must be faithful");
                round_trips += 1;
                let freed = space.unmap(page).expect("unmap restored page");
                frames.free(freed).expect("free restored frame");
            }

            // The books balance after every cycle, however it ended.
            assert_eq!(ramzip.ledger().entries(), 0, "entry leak");
            assert_eq!(ramzip.ledger().footprint(), 0, "footprint leak");
        }
        if !rustos_fuzzseed::within_budget(deadline) {
            break;
        }
    }

    assert!(round_trips > 0, "fuzz produced no round-trips");
    assert!(tamper_rejected > 0, "fuzz never exercised the tamper path");
}
