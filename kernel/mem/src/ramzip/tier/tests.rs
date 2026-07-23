//! Tier-level tests: the `plans/SWAPSWAPSWAP.md` section 18 matrix,
//! driven over the production shapes — a real [`FrameAllocator`], the
//! [`SimPhysMap`] physical-RAM stand-in, a [`HostPageTable`]-backed
//! address space, and the pressure gauge sampling the allocator itself,
//! so pressure bands in these tests come from genuinely scarce frames.

use super::*;

extern crate std;
use std::vec::Vec;

use crate::bootinfo::{BootMemoryMap, MemoryRegion, RegionKind};
use crate::frame::Frame;
use crate::phys::SimPhysMap;
use crate::ramzip::PageKind;
use crate::vmm::{HostPageTable, VirtAddr};

/// Backing frames for the default test machine (512 × 4 KiB = 2 MiB).
const TOTAL_FRAMES: usize = 512;

/// The address-space id every test uses.
const SPACE: u64 = 1;

/// The default owning task.
const TASK: u64 = 42;

/// User anonymous mapping flags for the tests.
fn user_rw() -> MapFlags {
    MapFlags::READ | MapFlags::WRITE | MapFlags::USER
}

/// Audit sink that discards records (the audit-emission tests live in
/// the audit module).
struct NullSink;

impl tairix_log::Sink for NullSink {
    fn write_event(&self, _event: &tairix_log::Event<'_>) {}
}

static NULL_SINK: NullSink = NullSink;

/// One self-contained test machine.
struct Env {
    frames: &'static FrameAllocator,
    pressure: MemoryPressure,
    physmap: SimPhysMap,
    space: AddressSpace<HostPageTable>,
    held: Vec<Frame>,
}

impl Env {
    fn new() -> Self {
        Self::with_total_frames(TOTAL_FRAMES)
    }

    /// A test machine with `total` backing frames, so a benchmark can
    /// represent both a small (Pi-class) and a larger (desktop) RAM
    /// profile from the one harness.
    fn with_total_frames(total: usize) -> Self {
        let mut map = BootMemoryMap::new();
        map.push(MemoryRegion {
            start: crate::frame::PhysAddr::new(0),
            length: (total * PAGE_SIZE) as u64,
            kind: RegionKind::Usable,
        });
        let frames: &'static FrameAllocator = std::boxed::Box::leak(std::boxed::Box::new(
            FrameAllocator::new(&map).expect("allocator"),
        ));
        let pressure = MemoryPressure::over(frames);
        Self {
            frames,
            pressure,
            physmap: SimPhysMap::new(crate::frame::PhysAddr::new(0), total * PAGE_SIZE),
            space: AddressSpace::new(HostPageTable::new()),
            held: Vec::new(),
        }
    }

    /// Hold frames until the sampled band reaches `band` exactly.
    fn press_to(&mut self, band: PressureBand) {
        // Bound the loop by the machine's own frame count, not the
        // default-machine constant, so a larger-RAM profile can be
        // pressed down too.
        let total_frames = FreeMemorySource::total_bytes(self.frames) / PAGE_SIZE;
        let mut guard = 0;
        while self.pressure.sample() != band {
            self.held.push(self.frames.alloc().expect("pressure frame"));
            guard += 1;
            assert!(guard <= total_frames, "band {band:?} never reached");
        }
    }

    /// Release every held frame and sample back to normal pressure.
    fn relax(&mut self) {
        while let Some(frame) = self.held.pop() {
            self.frames.free(frame).expect("free held frame");
        }
        for _ in 0..8 {
            if self.pressure.sample() == PressureBand::Normal {
                return;
            }
        }
        panic!("gauge failed to relax to normal");
    }

    /// Map an anonymous test page at `page_number` filled with a
    /// compressible pattern derived from `seed`.
    fn map_page(&mut self, page_number: u64, seed: u8) -> Page {
        let frame = self.frames.alloc().expect("page frame");
        let page = page_at(page_number);
        self.write_frame(frame, seed);
        self.space.map(page, frame, user_rw()).expect("map");
        page
    }

    /// Fill `frame` with the compressible pattern for `seed`.
    fn write_frame(&mut self, frame: Frame, seed: u8) {
        let bytes = self.frame_bytes_mut(frame);
        for (i, byte) in bytes.iter_mut().enumerate() {
            *byte = seed.wrapping_add(u8::try_from((i / 128) % 5).expect("small"));
        }
    }

    /// Borrow the direct-map bytes of `frame` mutably.
    fn frame_bytes_mut(&mut self, frame: Frame) -> &mut [u8] {
        let ptr = self
            .physmap
            .translate(frame.start(), PAGE_SIZE)
            .expect("frame in window");
        // SAFETY: `translate` proved the window; the tests are
        // single-threaded, so nothing aliases the borrow.
        unsafe { slice_within(ptr.as_ptr(), PAGE_SIZE, 0, PAGE_SIZE) }.expect("page slice")
    }

    /// The page's mapped bytes, via translate + direct map.
    fn page_bytes(&self, page: Page) -> Vec<u8> {
        let (frame, _) = self.space.translate(page).expect("mapped");
        let ptr = self
            .physmap
            .translate(frame.start(), PAGE_SIZE)
            .expect("frame in window");
        // SAFETY: `translate` proved the window; read-only snapshot,
        // and the tests are single-threaded.
        unsafe { core::slice::from_raw_parts(ptr.as_ptr(), PAGE_SIZE) }.to_vec()
    }
}

/// The page at `number`.
fn page_at(number: u64) -> Page {
    Page::from_addr(VirtAddr::new(number * PAGE_SIZE as u64)).expect("aligned")
}

/// A tier with caps derived from the test machine's RAM.
fn tier(env: &Env) -> Ramzip {
    tier_with_caps(RamzipCaps::from_physical(FreeMemorySource::total_bytes(
        env.frames,
    )))
}

/// A tier with explicit caps (for the cap/share refusal tests).
fn tier_with_caps(caps: RamzipCaps) -> Ramzip {
    struct CountingEntropy(u8);
    impl EntropySource for CountingEntropy {
        fn fill(&mut self, out: &mut [u8]) -> Result<(), SealError> {
            for byte in out.iter_mut() {
                self.0 = self.0.wrapping_add(1);
                *byte = self.0;
            }
            Ok(())
        }
    }
    Ramzip::new(caps, &mut CountingEntropy(0)).expect("tier")
}

/// Build the context over `env`'s parts (split borrows).
macro_rules! ctx {
    ($env:expr) => {
        VmContext {
            space_id: SPACE,
            space: &mut $env.space,
            physmap: &$env.physmap,
            frames: $env.frames,
            sink: &NULL_SINK,
        }
    };
}

/// Compress `page` expecting success, at the env's current band.
fn compress(env: &mut Env, ramzip: &mut Ramzip, page: Page) {
    try_compress(env, ramzip, page, TASK).expect("compress accepted");
}

/// Compress `page` for `task`, returning the tier's verdict.
fn try_compress(
    env: &mut Env,
    ramzip: &mut Ramzip,
    page: Page,
    task: u64,
) -> Result<(), CompressRefusal> {
    let pressure = &env.pressure;
    let mut ctx = ctx!(env);
    ramzip.compress_out(
        pressure,
        0,
        &mut ctx,
        page,
        task,
        &PageCandidate::cold_anonymous(),
    )
}

/// Fault `page` back in, returning the tier's verdict.
fn try_fault(env: &mut Env, ramzip: &mut Ramzip, page: Page) -> Result<(), FaultError> {
    let mut ctx = ctx!(env);
    ramzip.fault_in(&mut ctx, page)
}

#[test]
fn near_zero_idle_cost_and_no_eager_reservation() {
    let env = Env::new();
    let before = env.frames.free_frames();
    let ramzip = tier(&env);
    // Construction allocated no frames and accounts for nothing: the
    // minimum guarantee is capacity policy, not eagerly stolen RAM.
    assert_eq!(env.frames.free_frames(), before);
    assert_eq!(ramzip.ledger().footprint(), 0);
    assert_eq!(ramzip.ledger().entries(), 0);
    assert!(ramzip.caps().min() > 0);
}

#[test]
fn purge_space_drops_entries_and_rebalances_the_ledger() {
    let mut env = Env::new();
    let mut ramzip = tier(&env);
    env.press_to(PressureBand::Moderate);
    // Compress three anonymous pages of SPACE into the tier.
    let pages: Vec<Page> = (10..13).map(|n| env.map_page(n, 0x40)).collect();
    for &page in &pages {
        compress(&mut env, &mut ramzip, page);
    }
    assert_eq!(ramzip.ledger().entries(), 3);
    assert!(ramzip.ledger().footprint() > 0);

    // Tearing the space down (a task exit) purges every entry and
    // rebalances the ledger to zero — no orphaned RAM or ledger charge.
    assert_eq!(ramzip.purge_space(SPACE), 3);
    assert_eq!(ramzip.ledger().entries(), 0);
    assert_eq!(ramzip.ledger().footprint(), 0);
    assert_eq!(ramzip.ledger().task_usage(TASK).entries, 0);
    // Idempotent: a second purge of the emptied space finds nothing.
    assert_eq!(ramzip.purge_space(SPACE), 0);
    // A purged page has no entry (it would fault as a wild access).
    assert_eq!(
        try_fault(&mut env, &mut ramzip, pages[0]),
        Err(FaultError::NoEntry)
    );
}

#[test]
fn compress_and_fault_round_trip_restores_exact_bytes_and_flags() {
    let mut env = Env::new();
    let mut ramzip = tier(&env);
    let page = env.map_page(10, 7);
    let original = env.page_bytes(page);
    let free_mapped = env.frames.free_frames();

    env.press_to(PressureBand::Moderate);
    compress(&mut env, &mut ramzip, page);
    assert!(env.space.translate(page).is_none(), "page unmapped");
    assert!(ramzip.has_entry(SPACE, page));
    assert_eq!(ramzip.ledger().entries(), 1);
    assert_eq!(ramzip.ledger().logical_bytes(), PAGE_SIZE);
    assert!(ramzip.ledger().footprint() < PAGE_SIZE);
    assert_eq!(ramzip.ledger().task_usage(TASK).entries, 1);

    try_fault(&mut env, &mut ramzip, page).expect("fault in");
    let (_, flags) = env.space.translate(page).expect("mapped again");
    assert_eq!(flags, user_rw(), "restore preserves mapping flags");
    assert_eq!(env.page_bytes(page), original, "exact bytes restored");
    // Move-only: the entry is gone and the books balance to zero.
    assert!(!ramzip.has_entry(SPACE, page));
    assert_eq!(ramzip.ledger().entries(), 0);
    assert_eq!(ramzip.ledger().footprint(), 0);
    env.relax();
    assert_eq!(env.frames.free_frames(), free_mapped, "no frame leak");
}

#[test]
fn compression_frees_the_frame_and_scrubs_it() {
    let mut env = Env::new();
    let mut ramzip = tier(&env);
    let page = env.map_page(11, 9);
    let (frame, _) = env.space.translate(page).expect("mapped");

    env.press_to(PressureBand::Moderate);
    let free_before = env.frames.free_frames();
    compress(&mut env, &mut ramzip, page);
    // The frame went back to the allocator (one more free than while
    // mapped, ignoring the pressure-holding frames)…
    assert_eq!(env.frames.free_frames(), free_before + 1);
    // …and its contents were zeroed first (zero-on-free).
    assert!(env.frame_bytes_mut(frame).iter().all(|b| *b == 0));
}

#[test]
fn handoff_gate_refuses_outside_moderate_and_severe() {
    let mut env = Env::new();
    let mut ramzip = tier(&env);
    let page = env.map_page(12, 3);
    // Normal pressure: never compress.
    assert_eq!(
        try_compress(&mut env, &mut ramzip, page, TASK),
        Err(CompressRefusal::PressurePolicy)
    );
    // Moderate pressure but cheaper caches still resident: hold.
    env.press_to(PressureBand::Moderate);
    let pressure = &env.pressure;
    let mut ctx = ctx!(env);
    assert_eq!(
        ramzip.compress_out(
            pressure,
            4096,
            &mut ctx,
            page,
            TASK,
            &PageCandidate::cold_anonymous(),
        ),
        Err(CompressRefusal::PressurePolicy)
    );
    assert_eq!(ramzip.ledger().counters().rejected_policy, 2);
    assert!(env.space.translate(page).is_some(), "page untouched");
}

#[test]
fn ineligible_candidates_are_refused_with_the_reason() {
    let mut env = Env::new();
    let mut ramzip = tier(&env);
    let page = env.map_page(13, 3);
    env.press_to(PressureBand::Moderate);
    let pressure = &env.pressure;
    let mut ctx = ctx!(env);
    let candidate = PageCandidate {
        kind: PageKind::Unknown,
        ..PageCandidate::cold_anonymous()
    };
    assert_eq!(
        ramzip.compress_out(pressure, 0, &mut ctx, page, TASK, &candidate),
        Err(CompressRefusal::Ineligible(Ineligible::UnknownKind))
    );
    assert_eq!(ramzip.ledger().counters().rejected_ineligible, 1);
}

#[test]
fn unmapped_page_is_refused() {
    let mut env = Env::new();
    let mut ramzip = tier(&env);
    env.press_to(PressureBand::Moderate);
    assert_eq!(
        try_compress(&mut env, &mut ramzip, page_at(99), TASK),
        Err(CompressRefusal::NotMapped)
    );
}

#[test]
fn device_flagged_mapping_is_refused_in_depth() {
    let mut env = Env::new();
    let mut ramzip = tier(&env);
    let frame = env.frames.alloc().expect("frame");
    let page = page_at(14);
    env.space
        .map(
            page,
            frame,
            MapFlags::READ | MapFlags::WRITE | MapFlags::DMA_COHERENT,
        )
        .expect("map");
    env.press_to(PressureBand::Moderate);
    assert_eq!(
        try_compress(&mut env, &mut ramzip, page, TASK),
        Err(CompressRefusal::ForbiddenMapping)
    );
}

#[test]
fn incompressible_page_is_refused_and_stays_mapped() {
    let mut env = Env::new();
    let mut ramzip = tier(&env);
    let frame = env.frames.alloc().expect("frame");
    let page = page_at(15);
    // PRNG noise: incompressible by construction.
    let bytes = env.frame_bytes_mut(frame);
    let mut state = 0x1234_5678_9ABC_DEF0_u64;
    for byte in bytes.iter_mut() {
        state = state
            .wrapping_mul(6_364_136_223_846_793_005)
            .wrapping_add(1);
        *byte = (state >> 33).to_le_bytes()[0];
    }
    env.space.map(page, frame, user_rw()).expect("map");
    env.press_to(PressureBand::Moderate);
    assert_eq!(
        try_compress(&mut env, &mut ramzip, page, TASK),
        Err(CompressRefusal::Incompressible)
    );
    assert!(env.space.translate(page).is_some());
    assert_eq!(ramzip.ledger().counters().rejected_incompressible, 1);
    assert_eq!(ramzip.ledger().footprint(), 0);
}

#[test]
fn band_cap_is_enforced_and_escalation_is_deterministic() {
    let mut env = Env::new();
    // A machine so small the hard cap is two pages. Each page belongs
    // to a distinct task so the fair-share bound (half the cap) never
    // fires first; the cap must be the refusal that stops growth.
    let mut ramzip = tier_with_caps(RamzipCaps::from_physical(32 * 1024));
    let pages: Vec<Page> = (20..70).map(|n| env.map_page(n, 1)).collect();
    env.press_to(PressureBand::Moderate);
    let mut refusal = None;
    for (i, page) in pages.iter().enumerate() {
        match try_compress(&mut env, &mut ramzip, *page, TASK + i as u64) {
            Ok(()) => {
                // Re-hold the frame each acceptance frees, so the
                // pressure band stays pinned at moderate while the
                // tier's footprint grows toward the cap.
                env.held.push(env.frames.alloc().expect("re-hold"));
            }
            Err(e) => {
                refusal = Some(e);
                break;
            }
        }
    }
    assert_eq!(refusal, Some(CompressRefusal::CapReached));
    assert!(ramzip.ledger().footprint() <= ramzip.caps().hard());
    assert_eq!(ramzip.ledger().counters().rejected_cap, 1);
    // Hard-cap escalation: caches drained at deep pressure goes to the
    // VM policy, deterministically.
    assert_eq!(
        escalate_refusal(PressureBand::Severe, 0),
        EscalationStep::VmPolicy
    );
    assert_eq!(
        escalate_refusal(PressureBand::Severe, 4096),
        EscalationStep::ReclaimCaches
    );
    assert_eq!(
        escalate_refusal(PressureBand::Normal, 0),
        EscalationStep::Hold
    );
    assert_eq!(
        escalate_refusal(PressureBand::Critical, 0),
        EscalationStep::VmPolicy
    );
}

#[test]
fn per_task_share_is_enforced_per_owner() {
    let mut env = Env::new();
    // Hard cap two pages, share (half) one page: one entry per task.
    let mut ramzip = tier_with_caps(RamzipCaps::from_physical(32 * 1024));
    let first = env.map_page(22, 1);
    let second = env.map_page(23, 2);
    let third = env.map_page(24, 3);
    env.press_to(PressureBand::Moderate);
    compress(&mut env, &mut ramzip, first);
    assert_eq!(
        try_compress(&mut env, &mut ramzip, second, TASK),
        Err(CompressRefusal::TaskShareReached)
    );
    // A different task still has its own share.
    try_compress(&mut env, &mut ramzip, third, TASK + 1).expect("other task admitted");
    assert_eq!(ramzip.ledger().counters().rejected_task_share, 1);
}

#[test]
fn reserve_floor_refuses_compression_but_not_restore() {
    let mut env = Env::new();
    let mut ramzip = tier(&env);
    let early = env.map_page(30, 5);
    env.press_to(PressureBand::Moderate);
    compress(&mut env, &mut ramzip, early);

    // Drive free memory down to just above the decompression floor:
    // compression must refuse rather than dip in.
    let floor = decompression_floor(env.pressure.thresholds().reserve());
    let late = env.map_page(31, 6);
    while FreeMemorySource::free_bytes(env.frames).saturating_sub(PAGE_SIZE) > floor {
        env.held.push(env.frames.alloc().expect("hold"));
    }
    assert_eq!(
        try_compress(&mut env, &mut ramzip, late, TASK),
        Err(CompressRefusal::ReserveProtected)
    );
    assert_eq!(ramzip.ledger().counters().rejected_reserve, 1);
    // A restore still succeeds: fault-in is mandatory work and may use
    // memory down to the emergency reserve.
    try_fault(&mut env, &mut ramzip, early).expect("restore under pressure");
}

#[test]
fn fault_on_missing_or_mapped_page_is_typed() {
    let mut env = Env::new();
    let mut ramzip = tier(&env);
    assert_eq!(
        try_fault(&mut env, &mut ramzip, page_at(40)),
        Err(FaultError::NoEntry)
    );
    let page = env.map_page(41, 4);
    assert_eq!(
        try_fault(&mut env, &mut ramzip, page),
        Err(FaultError::NoEntry)
    );
}

#[test]
fn tampered_entry_fails_closed_with_audit_and_no_plaintext() {
    let mut env = Env::new();
    let mut ramzip = tier(&env);
    let page = env.map_page(50, 8);
    env.press_to(PressureBand::Moderate);
    compress(&mut env, &mut ramzip, page);

    // Tamper one ciphertext byte (offset past nonce and tag).
    let sealed_len = ramzip.entry_sealed_len(SPACE, page).expect("entry");
    assert!(ramzip.tamper_entry(SPACE, page, sealed_len - 1));
    assert_eq!(
        try_fault(&mut env, &mut ramzip, page),
        Err(FaultError::Authentication)
    );
    // Fail closed: the entry is discarded, nothing was mapped, the
    // books balance, and the loss is counted.
    assert!(!ramzip.has_entry(SPACE, page));
    assert!(env.space.translate(page).is_none());
    assert_eq!(ramzip.ledger().entries(), 0);
    assert_eq!(ramzip.ledger().footprint(), 0);
    assert_eq!(ramzip.ledger().counters().auth_failures, 1);
    // A second fault reports the entry gone.
    assert_eq!(
        try_fault(&mut env, &mut ramzip, page),
        Err(FaultError::NoEntry)
    );
}

#[test]
fn truncated_entry_metadata_fails_closed_as_corrupt() {
    let mut env = Env::new();
    let mut ramzip = tier(&env);
    let page = env.map_page(51, 8);
    env.press_to(PressureBand::Moderate);
    compress(&mut env, &mut ramzip, page);
    assert!(ramzip.truncate_entry(SPACE, page, 0));
    assert_eq!(
        try_fault(&mut env, &mut ramzip, page),
        Err(FaultError::Corrupt)
    );
    assert!(!ramzip.has_entry(SPACE, page));
    assert_eq!(ramzip.ledger().counters().decode_failures, 1);
    // Regression (found by the fuzz harness): the release must use the
    // figures charged at compression time, not the truncated blob's,
    // so the books balance to exactly zero.
    assert_eq!(ramzip.ledger().entries(), 0);
    assert_eq!(ramzip.ledger().footprint(), 0);
    assert_eq!(ramzip.ledger().task_usage(TASK).stored_bytes, 0);
}

#[test]
fn repeated_cycles_leak_no_frames_and_no_metadata() {
    let mut env = Env::new();
    let mut ramzip = tier(&env);
    let page = env.map_page(60, 2);
    let baseline = env.frames.free_frames();
    env.press_to(PressureBand::Moderate);
    for _ in 0..4 {
        compress(&mut env, &mut ramzip, page);
        try_fault(&mut env, &mut ramzip, page).expect("fault");
    }
    env.relax();
    assert_eq!(env.frames.free_frames(), baseline);
    assert_eq!(ramzip.ledger().entries(), 0);
    assert_eq!(ramzip.ledger().footprint(), 0);
    assert_eq!(ramzip.ledger().task_usage(TASK).entries, 0);
}

#[test]
fn thrashing_task_is_detected_and_refused() {
    let mut env = Env::new();
    let mut ramzip = tier(&env);
    let page = env.map_page(70, 1);
    env.press_to(PressureBand::Moderate);
    // Churn: compress and immediately fault back, repeatedly.
    for _ in 0..8 {
        compress(&mut env, &mut ramzip, page);
        try_fault(&mut env, &mut ramzip, page).expect("fault");
    }
    assert_eq!(ramzip.ledger().counters().thrash_detected, 1);
    assert_eq!(
        try_compress(&mut env, &mut ramzip, page, TASK),
        Err(CompressRefusal::TaskThrashing)
    );
    assert_eq!(ramzip.ledger().counters().rejected_thrash, 1);
    // Escalation for a thrashing refusal is the same deterministic
    // policy: caches first, then the VM policy.
    assert_eq!(
        escalate_refusal(PressureBand::Moderate, 0),
        EscalationStep::VmPolicy
    );
}

#[test]
fn cluster_restores_only_nearby_contemporaneous_entries() {
    let mut env = Env::new();
    let mut ramzip = tier(&env);
    let pages: Vec<Page> = (100..105).map(|n| env.map_page(n, 1)).collect();
    let far = env.map_page(200, 9);
    env.press_to(PressureBand::Moderate);
    for page in &pages {
        compress(&mut env, &mut ramzip, *page);
    }
    compress(&mut env, &mut ramzip, far);

    // Back to comfortable memory: the demand fault plus clustering.
    env.relax();
    try_fault(&mut env, &mut ramzip, pages[2]).expect("fault");
    let pressure = &env.pressure;
    let mut ctx = ctx!(env);
    let restored = ramzip.cluster_after_fault(pressure, &mut ctx, pages[2]);
    assert_eq!(restored, 4, "the four neighbours came back");
    for page in &pages {
        assert!(env.space.translate(*page).is_some());
    }
    // The distant page stays compressed: clustering is local.
    assert!(ramzip.has_entry(SPACE, far));
    assert_eq!(ramzip.ledger().counters().cluster_restored, 4);
}

#[test]
fn cluster_does_nothing_under_pressure() {
    let mut env = Env::new();
    let mut ramzip = tier(&env);
    let a = env.map_page(110, 1);
    let b = env.map_page(111, 2);
    env.press_to(PressureBand::Moderate);
    compress(&mut env, &mut ramzip, a);
    compress(&mut env, &mut ramzip, b);
    // Fault one back while still under pressure…
    try_fault(&mut env, &mut ramzip, a).expect("fault");
    // …then ask for clustering without relaxing: the gate is closed.
    let pressure = &env.pressure;
    let mut ctx = ctx!(env);
    assert_eq!(ramzip.cluster_after_fault(pressure, &mut ctx, a), 0);
    assert!(ramzip.has_entry(SPACE, b));
}

#[test]
fn warm_step_restores_near_recent_faults_only_when_comfortable() {
    let mut env = Env::new();
    let mut ramzip = tier(&env);
    let pages: Vec<Page> = (120..126).map(|n| env.map_page(n, 3)).collect();
    env.press_to(PressureBand::Moderate);
    for page in &pages {
        compress(&mut env, &mut ramzip, *page);
    }

    // Without a demand fault there is no warm-up evidence: everything
    // stays compressed by design.
    env.relax();
    {
        let pressure = &env.pressure;
        let mut ctx = ctx!(env);
        assert_eq!(
            ramzip.warm_step(pressure, &mut ctx),
            WarmOutcome::NothingToDo
        );
    }

    // A demand fault provides locality evidence; the next_nonce warm step
    // brings the neighbours back, budget-bounded.
    try_fault(&mut env, &mut ramzip, pages[0]).expect("fault");
    {
        let pressure = &env.pressure;
        let mut ctx = ctx!(env);
        assert_eq!(
            ramzip.warm_step(pressure, &mut ctx),
            WarmOutcome::Restored(5)
        );
    }
    for page in &pages {
        assert!(env.space.translate(*page).is_some());
    }
    assert_eq!(ramzip.ledger().counters().warm_restored, 5);
}

#[test]
fn warm_step_stops_immediately_under_pressure() {
    let mut env = Env::new();
    let mut ramzip = tier(&env);
    let a = env.map_page(130, 1);
    let b = env.map_page(131, 2);
    env.press_to(PressureBand::Moderate);
    compress(&mut env, &mut ramzip, a);
    compress(&mut env, &mut ramzip, b);
    try_fault(&mut env, &mut ramzip, a).expect("fault");
    // Still under pressure: the step must stop without restoring.
    let restored_before = ramzip.ledger().counters().warm_restored;
    let pressure = &env.pressure;
    let mut ctx = ctx!(env);
    assert_eq!(ramzip.warm_step(pressure, &mut ctx), WarmOutcome::Stopped);
    assert_eq!(ramzip.ledger().counters().warm_restored, restored_before);
    assert!(ramzip.has_entry(SPACE, b), "nothing was decompressed");
    assert!(ramzip.ledger().counters().warm_stopped >= 1);
}

// A stored entry is always strictly smaller than a page.
const _: () = assert!(MAX_COMPRESSED_LEN + SEAL_OVERHEAD + ENTRY_METADATA_BYTES < PAGE_SIZE + 1);

#[test]
fn entry_metadata_fits_the_accounted_bound() {
    // The accounting bound must cover the real bookkeeping: the map
    // key and the entry struct.
    let real = core::mem::size_of::<(u64, u64)>() + core::mem::size_of::<Entry>();
    assert!(
        real <= ENTRY_METADATA_BYTES,
        "entry bookkeeping ({real} bytes) exceeds the accounted bound"
    );
}

#[test]
fn counters_track_attempts_and_acceptances() {
    let mut env = Env::new();
    let mut ramzip = tier(&env);
    let page = env.map_page(140, 4);
    // One refusal at normal pressure, one acceptance at moderate.
    let _ = try_compress(&mut env, &mut ramzip, page, TASK);
    env.press_to(PressureBand::Moderate);
    compress(&mut env, &mut ramzip, page);
    let counters = ramzip.ledger().counters();
    assert_eq!(counters.attempts, 2);
    assert_eq!(counters.accepted, 1);
    assert_eq!(counters.rejected_policy, 1);
    try_fault(&mut env, &mut ramzip, page).expect("fault");
    assert_eq!(ramzip.ledger().counters().fault_ins, 1);
}

// -------------------------------------------------------------------------
// Performance evidence (`plans/SWAPSWAPSWAP.md` section 19).
//
// Following the repository's established benchmark-evidence style
// (`kernel/core::reclaim_integration_tests::bench_evidence_*`): the
// deterministic assertions prove the *work avoided* — a compressible
// cold page shrinks far below its logical size, and a move-only fault-in
// leaves no duplicate copy or leaked frame — while the printed wall-clock
// figures are estimates for threshold tuning, never assertions. They run
// on the host over the same production shapes every other tier test uses
// (a real `FrameAllocator`, `SimPhysMap`, and `HostPageTable`).
// -------------------------------------------------------------------------

/// Compress `pages` for a single task at the env's current band,
/// re-holding a frame per acceptance so the freed frame does not relax
/// the gauge out of the compression band, and return how many were
/// accepted. Mirrors the re-hold discipline of the cap-enforcement test.
fn compress_run(env: &mut Env, ramzip: &mut Ramzip, pages: &[Page]) -> usize {
    let mut accepted = 0;
    for &page in pages {
        match try_compress(env, ramzip, page, TASK) {
            Ok(()) => {
                accepted += 1;
                env.held.push(env.frames.alloc().expect("re-hold frame"));
            }
            Err(_) => break,
        }
    }
    accepted
}

/// Map an anonymous page filled with PRNG noise (incompressible by
/// construction), so a benchmark can measure the worst-case refusal cost.
fn map_incompressible_page(env: &mut Env, page_number: u64) -> Page {
    let frame = env.frames.alloc().expect("frame");
    let page = page_at(page_number);
    let bytes = env.frame_bytes_mut(frame);
    let mut state = 0x1234_5678_9ABC_DEF0_u64 ^ page_number;
    for byte in bytes.iter_mut() {
        state = state
            .wrapping_mul(6_364_136_223_846_793_005)
            .wrapping_add(1);
        *byte = (state >> 33).to_le_bytes()[0];
    }
    env.space.map(page, frame, user_rw()).expect("map");
    page
}

#[test]
fn bench_evidence_memory_saved_and_move_only_round_trip() {
    const PAGES: u64 = 48;

    let mut env = Env::new();
    let mut ramzip = tier(&env);
    let pages: Vec<Page> = (300..300 + PAGES).map(|n| env.map_page(n, 0x30)).collect();
    // Free count with the pages mapped: compress-out frees each frame and
    // fault-in re-maps it, so a leak-free round trip returns to exactly this.
    let baseline = env.frames.free_frames();

    // Compress-out latency under moderate pressure (the ordinary
    // pressure-relief band).
    env.press_to(PressureBand::Moderate);
    let started = std::time::Instant::now();
    let accepted = compress_run(&mut env, &mut ramzip, &pages);
    let compress_time = started.elapsed();
    assert_eq!(accepted, pages.len(), "every eligible cold page compressed");

    // Memory saved (deterministic): the tier represents the full logical
    // size of the pages, but its accounted footprint is far smaller — the
    // whole point of the tier. A conservative "over half saved" bound.
    let logical = ramzip.ledger().logical_bytes();
    let footprint = ramzip.ledger().footprint();
    let compressed = ramzip.ledger().compressed_bytes();
    assert_eq!(logical, pages.len() * PAGE_SIZE, "logical bytes tracked");
    assert!(
        footprint.saturating_mul(2) < logical,
        "compressible cold pages shrink far below their logical size \
         (footprint {footprint} B vs logical {logical} B)"
    );

    // Decompression (fault-in) latency, with plenty of free memory so
    // fault-in never competes for frames.
    env.relax();
    let started = std::time::Instant::now();
    for &page in &pages {
        try_fault(&mut env, &mut ramzip, page).expect("fault");
    }
    let fault_time = started.elapsed();

    // Move-only invariant: restoring every page leaves no duplicate
    // compressed copy, no ledger charge, and no leaked frame.
    assert_eq!(
        ramzip.ledger().entries(),
        0,
        "no entry retained after restore"
    );
    assert_eq!(ramzip.ledger().footprint(), 0, "books balance to zero");
    assert_eq!(env.frames.free_frames(), baseline, "no frame leak");

    std::eprintln!(
        "ramzip bench estimate (not a guarantee): Pi-class 2 MiB profile, \
         {PAGES} compressible pages: compress-out {compress_time:?}, \
         fault-in {fault_time:?}; memory saved {saved}% \
         (logical {logical} B -> compressed {compressed} B, stored+meta {footprint} B)",
        saved = 100 - (footprint.saturating_mul(100) / logical.max(1))
    );
}

#[test]
fn bench_evidence_larger_ram_profile_round_trip() {
    // 4 MiB machine: a desktop/laptop-scaled profile alongside the
    // Pi-class default, so the estimate covers both ends of the range
    // the plan asks for (section 19).
    const FRAMES: usize = 1024;
    const PAGES: u64 = 48;

    let mut env = Env::with_total_frames(FRAMES);
    let mut ramzip = tier(&env);
    let pages: Vec<Page> = (600..600 + PAGES).map(|n| env.map_page(n, 0x50)).collect();

    env.press_to(PressureBand::Moderate);
    let started = std::time::Instant::now();
    let accepted = compress_run(&mut env, &mut ramzip, &pages);
    let compress_time = started.elapsed();
    assert_eq!(accepted, pages.len());

    env.relax();
    let started = std::time::Instant::now();
    for &page in &pages {
        try_fault(&mut env, &mut ramzip, page).expect("fault");
    }
    let fault_time = started.elapsed();
    assert_eq!(ramzip.ledger().entries(), 0);

    std::eprintln!(
        "ramzip bench estimate (not a guarantee): desktop 4 MiB profile, \
         {PAGES} compressible pages: compress-out {compress_time:?}, \
         fault-in {fault_time:?}"
    );
}

#[test]
fn bench_evidence_cluster_severe_and_incompressible_cost() {
    let mut env = Env::new();
    let mut ramzip = tier(&env);

    // A contiguous run so fault clustering has neighbours to restore.
    let pages: Vec<Page> = (400..416).map(|n| env.map_page(n, 0x22)).collect();
    env.press_to(PressureBand::Moderate);
    assert_eq!(
        compress_run(&mut env, &mut ramzip, &pages),
        pages.len(),
        "the contiguous run compressed"
    );

    // Fault-in *with* clustering: a demand fault on the middle page,
    // then the opportunistic cluster restore around it (comfortable
    // memory), timed separately from the demand fault so the estimate
    // isolates the clustering cost.
    env.relax();
    let mid = pages[pages.len() / 2];
    try_fault(&mut env, &mut ramzip, mid).expect("demand fault");
    let started = std::time::Instant::now();
    let restored = {
        let pressure = &env.pressure;
        let mut ctx = ctx!(env);
        ramzip.cluster_after_fault(pressure, &mut ctx, mid)
    };
    let cluster_time = started.elapsed();
    assert!(
        restored > 0,
        "clustering restored neighbours when comfortable"
    );

    // CPU cost under severe pressure (the emergency-growth band): compress
    // a fresh batch there. Severe raises the cap toward the hard cap, so a
    // small run is admitted.
    let severe_pages: Vec<Page> = (440..456).map(|n| env.map_page(n, 0x33)).collect();
    env.press_to(PressureBand::Severe);
    let started = std::time::Instant::now();
    let severe_accepted = compress_run(&mut env, &mut ramzip, &severe_pages);
    let severe_time = started.elapsed();
    assert!(
        severe_accepted > 0,
        "compression is admitted under severe pressure"
    );

    // Worst-case incompressible workload: the refusal cost (compress,
    // discover the page will not shrink, and reject it without storing).
    env.relax();
    let noise = map_incompressible_page(&mut env, 500);
    env.press_to(PressureBand::Moderate);
    let started = std::time::Instant::now();
    let verdict = try_compress(&mut env, &mut ramzip, noise, TASK + 9);
    let reject_time = started.elapsed();
    assert_eq!(
        verdict,
        Err(CompressRefusal::Incompressible),
        "incompressible pages are refused, never stored raw"
    );

    std::eprintln!(
        "ramzip bench estimate (not a guarantee): cluster restore of \
         {restored} neighbours {cluster_time:?}; {severe_accepted} pages \
         compressed under severe pressure {severe_time:?}; \
         one incompressible-page refusal {reject_time:?}"
    );
}
