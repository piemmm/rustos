use super::DmaQuarantine;
use crate::devres::DmaQuarantineFacility;
use tairix_kernel_mem::{
    BootMemoryMap, DmaBlock, DmaCustody, FrameAllocator, MemoryClass, MemoryRegion, PhysAddr,
    PhysMap, RegionKind, SimPhysMap, PAGE_SIZE,
};
use tairix_sync::Once;

const BASE: u64 = 16 * PAGE_SIZE as u64;
const PAGES: usize = 64;
const NODE: u32 = 9;

/// The pool, its direct map and a quarantine over both, in cells of the
/// caller's own so concurrently-running tests never share a budget.
fn fixture(
    frames: &'static Once<FrameAllocator>,
    sim: &'static Once<SimPhysMap>,
    quarantine: &'static Once<DmaQuarantine>,
) -> (
    &'static DmaQuarantine,
    &'static FrameAllocator,
    &'static SimPhysMap,
) {
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
    let quarantine = quarantine
        .call_once_infallible(|| DmaQuarantine::new(frames, sim))
        .expect("a fresh cell");
    (quarantine, frames, sim)
}

macro_rules! quarantine {
    () => {{
        static FRAMES: Once<FrameAllocator> = Once::new();
        static SIM: Once<SimPhysMap> = Once::new();
        static QUARANTINE: Once<DmaQuarantine> = Once::new();
        fixture(&FRAMES, &SIM, &QUARANTINE)
    }};
}

/// Carve a block the way a driver's space does and fill it, so a scrub is
/// observable.
fn carve(frames: &FrameAllocator, sim: &SimPhysMap, order: u32) -> DmaBlock {
    let frame = frames
        .alloc_order(MemoryClass::Dma, order)
        .expect("a free block");
    let block = DmaBlock { frame, order };
    fill(sim, block, 0x5A);
    block
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
    let (quarantine, frames, sim) = quarantine!();
    let before = frames.free_frames();
    quarantine.bind(NODE).expect("binds");
    let block = carve(frames, sim, 1);
    quarantine.hold(NODE, 4, block);
    quarantine.unbind(NODE);
    assert_eq!(
        frames.free_frames(),
        before - 2,
        "a held block stays allocated"
    );
    assert_eq!(quarantine.held_bytes(NODE), 2 * PAGE_SIZE as u64);

    assert_eq!(
        quarantine.release(NODE, 4),
        Ok(0),
        "its own generation frees nothing"
    );
    assert_eq!(frames.free_frames(), before - 2);

    assert_eq!(quarantine.release(NODE, 5), Ok(2 * PAGE_SIZE as u64));
    assert_eq!(frames.free_frames(), before, "the reset freed it");
    assert!(is_zero(sim, block), "freed only once scrubbed");
    assert_eq!(quarantine.held_bytes(NODE), 0);
    assert_eq!(quarantine.tracked_nodes(), 0, "an idle record is dropped");
}

#[test]
fn a_release_frees_only_earlier_generations() {
    let (quarantine, frames, sim) = quarantine!();
    for generation in [3, 6, 8] {
        quarantine.bind(NODE).expect("binds");
        quarantine.hold(NODE, generation, carve(frames, sim, 0));
        quarantine.unbind(NODE);
    }
    assert_eq!(quarantine.release(NODE, 6), Ok(PAGE_SIZE as u64));
    assert_eq!(
        quarantine.held_bytes(NODE),
        2 * PAGE_SIZE as u64,
        "the releaser's own generation and a later one stay"
    );
    assert_eq!(
        quarantine.release(NODE, 5),
        Ok(0),
        "a stale, lower release cannot lower the bound"
    );
    assert_eq!(quarantine.release(NODE, 9), Ok(2 * PAGE_SIZE as u64));
}

#[test]
fn a_block_surrendered_after_its_successors_reset_is_freed_on_arrival() {
    // The dead driver's space is dropped only after its successor has
    // started, reset the device and released: the late block is safe to free.
    let (quarantine, frames, sim) = quarantine!();
    let before = frames.free_frames();
    quarantine
        .bind(NODE)
        .expect("the dead driver's space is still bound");
    assert_eq!(quarantine.release(NODE, 2), Ok(0), "nothing held yet");
    let block = carve(frames, sim, 0);
    quarantine.hold(NODE, 1, block);
    quarantine.unbind(NODE);
    assert_eq!(frames.free_frames(), before, "freed as it arrived");
    assert!(is_zero(sim, block));
    assert_eq!(quarantine.tracked_nodes(), 0);
}

#[test]
fn retiring_a_node_bounds_at_its_high_water_so_a_reused_id_is_unaffected() {
    let (quarantine, frames, sim) = quarantine!();
    let before = frames.free_frames();
    // The removed device's driver (generation 7) has not surrendered yet.
    quarantine.bind(NODE).expect("binds");
    assert_eq!(quarantine.retire(NODE, 7), Ok(0));

    // A new device reuses the id; its driver is admitted above the mark.
    quarantine.bind(NODE).expect("the new driver binds");

    let late = carve(frames, sim, 0);
    quarantine.hold(NODE, 7, late);
    quarantine.unbind(NODE);
    assert!(
        is_zero(sim, late),
        "the gone device's memory frees on arrival"
    );

    let live = carve(frames, sim, 0);
    quarantine.hold(NODE, 8, live);
    quarantine.unbind(NODE);
    assert_eq!(
        frames.free_frames(),
        before - 1,
        "the reused id's driver keeps its protection"
    );
    assert_eq!(quarantine.held_bytes(NODE), PAGE_SIZE as u64);
    assert_eq!(quarantine.release(NODE, 9), Ok(PAGE_SIZE as u64));
    assert_eq!(frames.free_frames(), before);
}

#[test]
fn a_record_lives_while_any_space_is_bound() {
    let (quarantine, _frames, _sim) = quarantine!();
    quarantine.bind(NODE).expect("first space");
    quarantine.bind(NODE).expect("second space");
    quarantine.unbind(NODE);
    assert_eq!(quarantine.tracked_nodes(), 1, "one space still bound");
    quarantine.unbind(NODE);
    assert_eq!(quarantine.tracked_nodes(), 0);
}

#[test]
fn a_block_for_an_unbound_node_stays_allocated_for_good() {
    let (quarantine, frames, sim) = quarantine!();
    let before = frames.free_frames();
    quarantine.hold(NODE, 1, carve(frames, sim, 0));
    assert_eq!(frames.free_frames(), before - 1, "never returned");
    assert_eq!(
        quarantine.release(NODE, u64::MAX),
        Ok(0),
        "nor reachable later"
    );
    assert_eq!(frames.free_frames(), before - 1);
}

#[test]
fn quieting_an_unknown_node_frees_nothing() {
    let (quarantine, _frames, _sim) = quarantine!();
    assert_eq!(quarantine.release(NODE, 3), Ok(0));
    assert_eq!(quarantine.retire(NODE, 3), Ok(0));
    assert_eq!(quarantine.tracked_nodes(), 0, "quieting records nothing");
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

    let (quarantine, frames, sim) = quarantine!();
    let before = frames.free_frames();
    let custodian = DmaCustodian {
        node: NODE,
        generation: 3,
        custody: quarantine,
    };
    {
        let mut live = LiveSpace::new(
            AddressSpace::new(HostPageTable::new()),
            SharedSim(sim),
            frames,
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
        frames.free_frames(),
        before - 1,
        "the dead space's carve is held, not freed"
    );
    assert_eq!(quarantine.held_bytes(NODE), PAGE_SIZE as u64);
    assert_eq!(quarantine.release(NODE, 4), Ok(PAGE_SIZE as u64));
    assert_eq!(
        frames.free_frames(),
        before,
        "the successor's reset freed it"
    );
    assert_eq!(quarantine.tracked_nodes(), 0);
}
