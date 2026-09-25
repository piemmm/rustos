use super::DmaQuarantine;
use crate::devres::DmaQuarantineFacility;
use crate::hwtree::HwNodeLiveness;
use crate::test_alloc::{opt_in_current_thread, opt_out_current_thread, LiveBytes};
use alloc::vec::Vec;
use tairix_kernel_mem::{
    BootMemoryMap, DmaBlock, DmaCustody, DmaError, Frame, FrameAllocator, MemoryClass,
    MemoryRegion, PhysAddr, PhysMap, RegionKind, SimPhysMap, PAGE_SIZE,
};
use tairix_sync::{Once, SpinLock};

const BASE_FRAME: usize = 16;
const BASE: u64 = (BASE_FRAME * PAGE_SIZE) as u64;
const PAGES: usize = 64;
const NODE: u32 = 9;
const OTHER: u32 = 10;

/// The hardware tree as the quarantine sees it: every node live until a test
/// removes it.
struct Devices {
    removed: SpinLock<Vec<u32>>,
}

impl Devices {
    const fn new() -> Self {
        Self {
            removed: SpinLock::new(Vec::new()),
        }
    }

    fn remove(&self, node: u32) {
        self.removed.lock().push(node);
    }
}

impl HwNodeLiveness for Devices {
    fn is_live(&self, node_id: u32) -> bool {
        !self.removed.lock().contains(&node_id)
    }
}

/// The pool, its direct map, the device tree and a quarantine over them, in
/// cells of the caller's own so concurrently-running tests never share a
/// budget.
struct Fixture {
    quarantine: &'static DmaQuarantine,
    frames: &'static FrameAllocator,
    sim: &'static SimPhysMap,
    devices: &'static Devices,
}

impl Fixture {
    /// Take `node` out of the tree in order, as `hw_remove_node` does.
    fn detach(&self, node: u32) {
        self.devices.remove(node);
        self.quarantine.detach(node);
    }

    /// Take `node` out of the tree by surprise, returning what that freed.
    fn retire(&self, node: u32) -> u64 {
        self.devices.remove(node);
        self.quarantine.retire(node).expect("a wired quarantine")
    }
}

/// The pool and the direct map over exactly its RAM, in the caller's cells.
fn pool(
    frames: &'static Once<FrameAllocator>,
    sim: &'static Once<SimPhysMap>,
) -> (&'static FrameAllocator, &'static SimPhysMap) {
    let frames = frames
        .call_once_infallible(|| {
            let mut map = BootMemoryMap::new();
            map.push(MemoryRegion {
                kind: RegionKind::Usable,
                start: PhysAddr::new(BASE),
                length: (PAGES * PAGE_SIZE) as u64,
            });
            FrameAllocator::new(&map).expect("allocator over the window")
        })
        .expect("a fresh cell");
    let sim = sim
        .call_once_infallible(|| SimPhysMap::new(PhysAddr::new(BASE), PAGES * PAGE_SIZE))
        .expect("a fresh cell");
    (frames, sim)
}

fn fixture(
    frames: &'static Once<FrameAllocator>,
    sim: &'static Once<SimPhysMap>,
    devices: &'static Devices,
    quarantine: &'static Once<DmaQuarantine>,
) -> Fixture {
    let (frames, sim) = pool(frames, sim);
    let quarantine = quarantine
        .call_once_infallible(|| DmaQuarantine::new(frames, sim, devices))
        .expect("a fresh cell");
    Fixture {
        quarantine,
        frames,
        sim,
        devices,
    }
}

macro_rules! fixture {
    () => {{
        static FRAMES: Once<FrameAllocator> = Once::new();
        static SIM: Once<SimPhysMap> = Once::new();
        static DEVICES: Devices = Devices::new();
        static QUARANTINE: Once<DmaQuarantine> = Once::new();
        fixture(&FRAMES, &SIM, &DEVICES, &QUARANTINE)
    }};
}

/// Carve a block for `node` the way a driver's space does — room reserved
/// first — and fill it, so a scrub is observable.
fn carve_for(f: &Fixture, node: u32, order: u32) -> DmaBlock {
    f.quarantine.reserve(node).expect("custody reserved");
    let frame = f
        .frames
        .alloc_order(MemoryClass::Dma, order)
        .expect("a free block");
    let block = DmaBlock { frame, order };
    fill(f.sim, block, 0x5A);
    block
}

fn carve(f: &Fixture, order: u32) -> DmaBlock {
    carve_for(f, NODE, order)
}

fn fill(sim: &SimPhysMap, block: DmaBlock, byte: u8) {
    let ptr = sim
        .translate(block.frame.start(), block.len())
        .expect("in the window");
    // SAFETY: the sim map proved the pointer valid for the block, which only
    // this test references.
    unsafe { core::ptr::write_bytes(ptr.as_ptr(), byte, block.len()) };
}

fn is_zero(sim: &SimPhysMap, block: DmaBlock) -> bool {
    let ptr = sim
        .translate(block.frame.start(), block.len())
        .expect("in the window");
    // SAFETY: as in `fill`.
    let bytes = unsafe { core::slice::from_raw_parts(ptr.as_ptr(), block.len()) };
    bytes.iter().all(|&b| b == 0)
}

#[test]
fn a_dead_drivers_blocks_stay_held_until_a_later_instance_resets_the_device() {
    let f = fixture!();
    let before = f.frames.free_frames();
    let block = carve(&f, 1);
    f.quarantine.hold(NODE, 4, block);
    assert_eq!(
        f.frames.free_frames(),
        before - 2,
        "a held block stays allocated"
    );
    assert_eq!(f.quarantine.held_bytes(NODE), 2 * PAGE_SIZE as u64);

    assert_eq!(
        f.quarantine.release(NODE, 4),
        Ok(0),
        "its own generation frees nothing"
    );
    assert_eq!(f.frames.free_frames(), before - 2);

    assert_eq!(f.quarantine.release(NODE, 5), Ok(2 * PAGE_SIZE as u64));
    assert_eq!(f.frames.free_frames(), before, "the reset freed it");
    assert!(is_zero(f.sim, block), "freed only once scrubbed");
    assert_eq!(f.quarantine.held_bytes(NODE), 0);
}

#[test]
fn a_release_frees_only_earlier_generations() {
    let f = fixture!();
    for generation in [3, 6, 8] {
        let block = carve(&f, 0);
        f.quarantine.hold(NODE, generation, block);
    }
    assert_eq!(f.quarantine.release(NODE, 6), Ok(PAGE_SIZE as u64));
    assert_eq!(
        f.quarantine.held_bytes(NODE),
        2 * PAGE_SIZE as u64,
        "the releaser's own generation and a later one stay"
    );
    assert_eq!(
        f.quarantine.release(NODE, 5),
        Ok(0),
        "a stale, lower release cannot lower the bound"
    );
    assert_eq!(f.quarantine.release(NODE, 9), Ok(2 * PAGE_SIZE as u64));
}

#[test]
fn a_block_surrendered_after_its_successors_reset_is_freed_on_arrival() {
    // The dead driver's space is dropped only after its successor has
    // started, reset the device and released: the late block is safe to free.
    let f = fixture!();
    let before = f.frames.free_frames();
    let block = carve(&f, 0);
    assert_eq!(f.quarantine.release(NODE, 2), Ok(0), "nothing held yet");
    f.quarantine.hold(NODE, 1, block);
    assert_eq!(f.frames.free_frames(), before, "freed as it arrived");
    assert!(is_zero(f.sim, block));
}

#[test]
fn a_live_nodes_record_survives_going_idle() {
    // A driver that carves and frees one buffer at a time would otherwise
    // rebuild the record — a map insert and a fresh allocation — every cycle.
    static COUNTER: LiveBytes = LiveBytes::new();
    let f = fixture!();
    f.quarantine.reserve(NODE).expect("first carve");
    f.quarantine.reserve(NODE).expect("second carve");
    f.quarantine.unreserve(NODE);
    f.quarantine.unreserve(NODE);
    assert_eq!(f.quarantine.tracked_nodes(), 1, "the idle record stays");

    opt_in_current_thread(&COUNTER);
    f.quarantine
        .reserve(NODE)
        .expect("a carve after the idle spell");
    f.quarantine.unreserve(NODE);
    opt_out_current_thread();
    assert_eq!(
        COUNTER.allocations(),
        0,
        "a carve within the record's room allocates nothing"
    );
}

#[test]
fn a_reset_proof_is_kept_while_the_node_is_in_the_tree() {
    let f = fixture!();
    let before = f.frames.free_frames();
    let early = carve(&f, 0);
    f.quarantine.hold(NODE, 4, early);
    assert_eq!(f.quarantine.release(NODE, 5), Ok(PAGE_SIZE as u64));
    assert_eq!(f.quarantine.held_bytes(NODE), 0, "the record is idle");

    // A generation-4 block arriving after the idle spell is covered by the
    // reset proof the record kept; a fresh record would hold it.
    let late = carve(&f, 0);
    f.quarantine.hold(NODE, 4, late);
    assert!(is_zero(f.sim, late), "freed on arrival");
    assert_eq!(f.frames.free_frames(), before);

    // The releasing instance's own carve is never freed by its own reset.
    let own = carve(&f, 0);
    f.quarantine.hold(NODE, 5, own);
    assert_eq!(f.frames.free_frames(), before - 1);
    assert_eq!(f.quarantine.release(NODE, 6), Ok(PAGE_SIZE as u64));
}

#[test]
fn a_removed_devices_memory_is_freed_now_and_on_arrival() {
    let f = fixture!();
    let before = f.frames.free_frames();
    let dead = carve(&f, 0);
    f.quarantine.hold(NODE, 3, dead);
    // A later instance, still live, carved too; its device then vanished.
    let live = carve(&f, 0);

    assert_eq!(f.retire(NODE), PAGE_SIZE as u64);
    assert!(is_zero(f.sim, dead));

    f.quarantine.hold(NODE, 4, live);
    assert!(
        is_zero(f.sim, live),
        "the gone device's memory frees on arrival"
    );
    assert_eq!(f.frames.free_frames(), before);
    assert_eq!(
        f.quarantine.tracked_nodes(),
        0,
        "a removed node's record goes once nothing can reach it"
    );
}

#[test]
fn a_removed_device_is_handed_no_more_memory() {
    let f = fixture!();
    let live = carve(&f, 0);
    assert_eq!(f.retire(NODE), 0);
    assert_eq!(
        f.quarantine.reserve(NODE),
        Err(DmaError::DeviceGone),
        "while the record stands"
    );
    f.quarantine.hold(NODE, 4, live);
    assert_eq!(f.quarantine.tracked_nodes(), 0);
    assert_eq!(
        f.quarantine.reserve(NODE),
        Err(DmaError::DeviceGone),
        "and once it has gone"
    );
    assert_eq!(f.quarantine.tracked_nodes(), 0);
}

#[test]
fn a_carve_for_an_orderly_removed_node_is_refused_whether_or_not_its_record_exists() {
    let f = fixture!();
    // A node whose record holds a dead driver's block when it is removed.
    let held = carve_for(&f, NODE, 0);
    f.quarantine.hold(NODE, 3, held);
    // A node whose record is idle when it is removed.
    f.quarantine.reserve(OTHER).expect("a carve");
    f.quarantine.unreserve(OTHER);
    assert_eq!(f.quarantine.tracked_nodes(), 2);

    f.detach(NODE);
    f.detach(OTHER);
    assert_eq!(f.quarantine.reserve(NODE), Err(DmaError::DeviceGone));
    assert_eq!(f.quarantine.reserve(OTHER), Err(DmaError::DeviceGone));
    assert_eq!(
        f.quarantine.tracked_nodes(),
        1,
        "only the record still holding memory stays"
    );
    assert_eq!(f.quarantine.held_bytes(NODE), PAGE_SIZE as u64);
}

#[test]
fn an_orderly_removal_keeps_what_is_held_until_a_reset() {
    // An orderly removal proves nothing about the device, which may still be
    // running: its memory waits for a reset like any other node's.
    let f = fixture!();
    let before = f.frames.free_frames();
    let dead = carve(&f, 0);
    f.quarantine.hold(NODE, 3, dead);
    let live = carve(&f, 0);
    f.detach(NODE);
    assert_eq!(f.quarantine.held_bytes(NODE), PAGE_SIZE as u64);

    // The node's live driver, admitted as generation 4, resets its device.
    assert_eq!(f.quarantine.release(NODE, 4), Ok(PAGE_SIZE as u64));
    assert!(is_zero(f.sim, dead));
    f.quarantine.hold(NODE, 4, live);
    assert_eq!(
        f.frames.free_frames(),
        before - 1,
        "its own block stays held, for the boot"
    );
    assert_eq!(f.quarantine.tracked_nodes(), 1);
}

#[test]
fn a_removed_nodes_record_goes_once_its_last_reservation_does() {
    let f = fixture!();
    f.quarantine.reserve(NODE).expect("a carve");
    f.detach(NODE);
    assert_eq!(
        f.quarantine.tracked_nodes(),
        1,
        "the carve can still arrive"
    );
    f.quarantine.unreserve(NODE);
    assert_eq!(f.quarantine.tracked_nodes(), 0);
}

/// A tree that notes, each time it is asked, whether the quarantine's record
/// lock was held.
struct WitnessTree {
    quarantine: tairix_sync::OnceCell<&'static DmaQuarantine>,
    asked_under_the_lock: SpinLock<Vec<bool>>,
}

impl HwNodeLiveness for WitnessTree {
    fn is_live(&self, _node_id: u32) -> bool {
        if let Ok(Some(quarantine)) = self.quarantine.get() {
            self.asked_under_the_lock
                .lock()
                .push(quarantine.nodes.is_locked());
        }
        true
    }
}

#[test]
fn only_a_records_first_carve_asks_the_tree_and_under_the_removals_lock() {
    // A removal reports itself under the record lock, so a tree answer taken
    // outside it could let the removal slip between the answer and the record
    // it would have marked.
    static FRAMES: Once<FrameAllocator> = Once::new();
    static SIM: Once<SimPhysMap> = Once::new();
    static TREE: WitnessTree = WitnessTree {
        quarantine: tairix_sync::OnceCell::new(),
        asked_under_the_lock: SpinLock::new(Vec::new()),
    };
    static QUARANTINE: Once<DmaQuarantine> = Once::new();
    let (frames, sim) = pool(&FRAMES, &SIM);
    let quarantine = QUARANTINE
        .call_once_infallible(|| DmaQuarantine::new(frames, sim, &TREE))
        .expect("a fresh cell");
    assert!(TREE.quarantine.set(quarantine).is_ok());

    quarantine.reserve(NODE).expect("a live node");
    quarantine.reserve(NODE).expect("its record is open");
    assert_eq!(*TREE.asked_under_the_lock.lock(), [true]);
}

#[test]
fn a_removal_that_outran_the_first_carve_is_not_missed() {
    // The node went before any carve opened a record, so the retirement found
    // nothing to mark; the carve must still see the device gone.
    let f = fixture!();
    assert_eq!(f.retire(NODE), 0);
    assert_eq!(f.quarantine.tracked_nodes(), 0, "retiring records nothing");
    assert_eq!(f.quarantine.reserve(NODE), Err(DmaError::DeviceGone));
    assert_eq!(f.quarantine.tracked_nodes(), 0);
}

#[test]
fn only_the_bytes_actually_freed_are_counted() {
    // A block the direct map cannot reach, or the allocator refuses, keeps its
    // frames, so it must not be reported as released.
    let f = fixture!();
    let before = f.frames.free_frames();
    let beyond_the_map = DmaBlock {
        frame: Frame(BASE_FRAME + PAGES + 8),
        order: 0,
    };
    let already_free = {
        let frame = f
            .frames
            .alloc_order(MemoryClass::Dma, 0)
            .expect("a free block");
        f.frames.free_order(frame, 0).expect("a live block frees");
        DmaBlock { frame, order: 0 }
    };
    for block in [beyond_the_map, already_free] {
        f.quarantine.reserve(NODE).expect("custody reserved");
        f.quarantine.hold(NODE, 1, block);
    }
    assert_eq!(f.quarantine.release(NODE, 2), Ok(0));
    assert_eq!(f.frames.free_frames(), before);
}

#[test]
fn a_block_no_reservation_stands_behind_stays_allocated_for_good() {
    let f = fixture!();
    let before = f.frames.free_frames();
    let frame = f
        .frames
        .alloc_order(MemoryClass::Dma, 0)
        .expect("a free block");
    f.quarantine.hold(NODE, 1, DmaBlock { frame, order: 0 });
    assert_eq!(f.frames.free_frames(), before - 1, "never returned");
    assert_eq!(
        f.quarantine.release(NODE, u64::MAX),
        Ok(0),
        "nor reachable later"
    );
    assert_eq!(f.frames.free_frames(), before - 1);
}

#[test]
fn quieting_an_unknown_node_frees_nothing() {
    let f = fixture!();
    assert_eq!(f.quarantine.release(NODE, 3), Ok(0));
    assert_eq!(f.quarantine.retire(NODE), Ok(0));
    f.quarantine.detach(NODE);
    assert_eq!(f.quarantine.tracked_nodes(), 0, "quieting records nothing");
}

#[test]
fn a_surrender_never_allocates() {
    // The teardown that surrenders a block may be running under the very
    // memory pressure it is about to relieve, so the room it records into was
    // made when the block was carved.
    static COUNTER: LiveBytes = LiveBytes::new();
    let f = fixture!();
    let blocks: Vec<DmaBlock> = (0..8).map(|_| carve(&f, 0)).collect();
    opt_in_current_thread(&COUNTER);
    for &block in &blocks {
        f.quarantine.hold(NODE, 1, block);
    }
    opt_out_current_thread();
    assert_eq!(COUNTER.allocations(), 0);
    assert_eq!(f.quarantine.held_bytes(NODE), 8 * PAGE_SIZE as u64);
    assert_eq!(f.quarantine.release(NODE, 2), Ok(8 * PAGE_SIZE as u64));
}

/// A `PhysMap` view over the fixture's shared sim map, so the space and the
/// quarantine scrub the same simulated RAM.
struct SharedSim(&'static SimPhysMap);

impl PhysMap for SharedSim {
    fn translate(&self, phys: PhysAddr, len: usize) -> Option<core::ptr::NonNull<u8>> {
        self.0.translate(phys, len)
    }

    fn clean_invalidate(&self, phys: PhysAddr, len: usize) {
        self.0.clean_invalidate(phys, len);
    }

    fn sync_instruction_cache(&self, phys: PhysAddr, len: usize) {
        self.0.sync_instruction_cache(phys, len);
    }
}

#[test]
fn a_dead_space_surrenders_to_the_quarantine_and_its_successor_frees_it() {
    use tairix_kernel_mem::{
        AddressSpace, DmaCustodian, HostPageTable, LiveSpace, LiveUserSpace, VirtAddr,
    };

    let f = fixture!();
    let before = f.frames.free_frames();
    let custodian = DmaCustodian {
        node: NODE,
        generation: 3,
        custody: f.quarantine,
    };
    {
        let mut live = LiveSpace::new(
            AddressSpace::new(HostPageTable::new()),
            crate::procspace::new_space_tlb(None).expect("the set allocates"),
            SharedSim(f.sim),
            f.frames,
            VirtAddr::new(0x4000_0000),
            8,
            VirtAddr::new(0x5000_0000),
            8,
            VirtAddr::new(0x6000_0000),
            8,
            VirtAddr::new(0x7000_0000),
            8,
            VirtAddr::new(0x8000_0000),
            8,
        )
        .expect("windows are valid");
        live.alloc_dma(PAGE_SIZE, 0, custodian)
            .expect("a driver carves");
    }
    assert_eq!(
        f.frames.free_frames(),
        before - 1,
        "the dead space's carve is held, not freed"
    );
    assert_eq!(f.quarantine.held_bytes(NODE), PAGE_SIZE as u64);
    assert_eq!(f.quarantine.release(NODE, 4), Ok(PAGE_SIZE as u64));
    assert_eq!(
        f.frames.free_frames(),
        before,
        "the successor's reset freed it"
    );
}
