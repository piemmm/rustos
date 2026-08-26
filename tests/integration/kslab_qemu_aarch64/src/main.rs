//! `plans/FIX-KHEAP.md` slab-tier QEMU integration test: the kernel heap
//! serves a page-sized allocation out of **exactly one physical frame**,
//! addressed through the live direct map, and gives its pages back.
//!
//! ## Why this exists
//!
//! The slab tier's whole claim is arithmetic that only holds if the frame it
//! draws is genuinely usable memory: one frame per page-sized object, no
//! header, no remap-window slot, and the frame returned when the page drains.
//! Host tests prove the bookkeeping over a simulated physical map; this proves
//! it against a real frame allocator and a real MMU, where a mistranslated or
//! non-invertible direct map would silently strand every page.
//!
//! ## What this test asserts
//!
//! 1. Before a page supply is installed, the slab carves its pages out of the
//!    bootstrap `.bss` region, so the kernel has a working heap from the first
//!    allocation.
//! 2. With the production `kernel/mem::FramePages` supply installed, a `PAGE_SIZE`
//!    allocation costs exactly one frame, starts at a page boundary (so it
//!    carries no header), and comes from the frame pool rather than the
//!    bootstrap region.
//! 3. That page is genuinely writable end to end under the live MMU: the whole
//!    page is scribbled and read back.
//! 4. A drained page is kept back once — the next allocation reuses it without
//!    touching the frame allocator — and the one after that is returned, so an
//!    idle class holds a single page.
//! 5. Every size class round-trips: each is aligned to its own width, and a
//!    freed object is handed straight back out.
//! 6. A request above the granule still comes from the byte-granular tier.
//!
//! ## How it differs from a production kernel
//!
//! It links the arch port, `kernel/mem`, and the allocator, and supplies its
//! own `kernel_main` and a page-only heap source — the shape a port with a
//! direct map but no remap window presents. The QEMU-exit shortcut lives in
//! this dedicated bin, never behind a Cargo feature on a production crate.

#![cfg_attr(itest_aarch64, no_std)]
#![cfg_attr(itest_aarch64, no_main)]
#![deny(missing_docs)]

#[cfg(itest_aarch64)]
extern crate alloc;

// --- Freestanding test bin (`aarch64-unknown-none`) ----------------

#[cfg(itest_aarch64)]
mod kernel {
    use core::alloc::{GlobalAlloc, Layout};
    use core::num::NonZeroU16;
    use core::panic::PanicInfo;
    use core::ptr::NonNull;

    use alloc::boxed::Box;

    use tairix_abi::PAGE_SIZE;
    use tairix_arch_aarch64::paging::{AddressSpace, PageTablePool};
    use tairix_arch_aarch64::{exceptions, handle_panic_via_serial, qemu_exit, SERIAL_SINK};
    use tairix_arch_api::mmu::AddressSpace as _;
    use tairix_itest_finisher::fail_point;
    use tairix_kalloc::{FreeListAllocator, HeapSource};
    use tairix_kernel_mem::{
        BootMemoryMap, DirectPhysMap, FrameAllocator, FramePages, MemoryRegion, PhysAddr, PhysMap,
        RegionKind,
    };
    use tairix_log::{log, Event, Field, FieldValue, Level};

    /// Gigabytes the space identity-maps (device MMIO in GiB 0, RAM in GiB 1),
    /// which is also the extent the direct map inverts over.
    const IDENTITY_GIB: usize = 2;

    /// Stable audit-event ids for the QEMU transcript.
    const TEST_START: tairix_log::EventId = tairix_log::EventId(4370);
    const TEST_PASS: tairix_log::EventId = tairix_log::EventId(4371);
    const TEST_FAIL: tairix_log::EventId = tairix_log::EventId(4372);
    /// Failure finisher code. One site: every check reports through `fail`.
    const FAIL_CHECK: NonZeroU16 = fail_point!(2);

    /// Bootstrap heap region, the `.bss` arena every freestanding image
    /// starts on. 2 MiB is ample for this vertical's own allocations and for
    /// the pages the slab carves out of it before the supply is installed.
    const HEAP_BYTES: usize = 2 * 1024 * 1024;

    /// Page-aligned backing store for the bootstrap heap.
    #[repr(C, align(4096))]
    struct HeapStore([u8; HEAP_BYTES]);

    static mut HEAP: HeapStore = HeapStore([0; HEAP_BYTES]);

    /// The allocator under test, registered exactly as a real image does.
    ///
    /// SAFETY: the page-aligned `HEAP` static outlives the binary and the
    /// allocator is its only consumer.
    #[global_allocator]
    static ALLOCATOR: FreeListAllocator =
        unsafe { FreeListAllocator::new(core::ptr::addr_of!(HEAP) as *mut u8, HEAP_BYTES) };

    /// Frames the test's physical pool holds (1 MiB), plenty for a handful of
    /// slab pages per class.
    const POOL_PAGES: usize = 256;

    /// Page-aligned physical pool the frame allocator is built over. Identity
    /// mapped, so its virtual address is its physical one.
    #[repr(C, align(4096))]
    struct FramePool([u8; PAGE_SIZE * POOL_PAGES]);

    static mut FRAME_POOL: FramePool = FramePool([0; PAGE_SIZE * POOL_PAGES]);

    /// Page-table pool backing the identity address space (lives in `.bss`).
    static POOL: PageTablePool = PageTablePool::new();

    /// A heap source that supplies **pages only** — the shape a port with a
    /// direct map but no kernel remap window presents. Its byte-granular tier
    /// then stays on the bootstrap region, which is what this vertical wants:
    /// every frame the pool loses is one the slab took.
    struct PageOnlySource {
        pages: FramePages,
    }

    impl HeapSource for PageOnlySource {
        fn grow(&self, _min_len: usize) -> Option<(*mut u8, usize)> {
            None
        }

        fn shrink(&self, _base: *mut u8, _len: usize) {}

        fn alloc_page(&self) -> Option<NonNull<u8>> {
            self.pages.alloc()
        }

        fn free_page(&self, page: NonNull<u8>) {
            self.pages.free(page);
        }
    }

    /// Forward to the shared aarch64 panic bridge (parks the CPU; the run then
    /// times out and the harness reports the failure).
    #[panic_handler]
    fn tairix_kslab_aarch64_panic(info: &PanicInfo<'_>) -> ! {
        handle_panic_via_serial(info)
    }

    /// Log a failed check and report it to QEMU. Never returns.
    fn fail(what: &str) -> ! {
        log(
            &SERIAL_SINK,
            &Event {
                level: Level::Error,
                id: TEST_FAIL,
                message: "aarch64 kslab test: check failed",
                fields: &[Field {
                    key: "check",
                    value: FieldValue::Str(what),
                }],
            },
        );
        qemu_exit::exit_failure(FAIL_CHECK)
    }

    /// Assert `cond`, reporting `what` to QEMU when it does not hold.
    fn check(cond: bool, what: &str) {
        if !cond {
            fail(what);
        }
    }

    /// Allocate `layout` through the global allocator, failing the run rather
    /// than handing back null.
    fn alloc(layout: Layout, what: &str) -> *mut u8 {
        // SAFETY: every layout here has a non-zero size.
        let p = unsafe { ALLOCATOR.alloc(layout) };
        if p.is_null() {
            fail(what);
        }
        p
    }

    /// Write `byte` over `len` bytes at `p` and read every one of them back.
    fn scribble(p: *mut u8, len: usize, byte: u8, what: &str) {
        for i in 0..len {
            // SAFETY: `p` owns `len` writable bytes per the alloc contract.
            unsafe { core::ptr::write_volatile(p.add(i), byte) };
        }
        for i in 0..len {
            // SAFETY: as above; the bytes were just written.
            if unsafe { core::ptr::read_volatile(p.add(i)) } != byte {
                fail(what);
            }
        }
    }

    /// Boot entry point — the symbol the arch crate's `boot.s` trampoline
    /// calls (via `tairix_arch_aarch64_main`).
    #[no_mangle]
    pub extern "C" fn kernel_main(_dtb: u64) -> ! {
        log(
            &SERIAL_SINK,
            &Event {
                level: Level::Info,
                id: TEST_START,
                message: "aarch64 kslab test: proving one frame per page-sized allocation",
                fields: &[],
            },
        );

        // Install the vectors and enable the MMU before touching a frame: the
        // slab writes its pages through the direct map, so every access below
        // is a translated one.
        // SAFETY: called once on the boot CPU before any fault can fire.
        unsafe { exceptions::init_vectors() };
        let space = AddressSpace::new_identity_gigapages(&POOL, IDENTITY_GIB)
            .unwrap_or_else(|| fail("identity map"));
        // SAFETY: the space identity-maps `pc`, `sp`, and MMIO per
        // `new_identity_gigapages`, so execution continues across the switch.
        unsafe { space.activate() };

        // The slab has no supply yet, so it carves its pages out of the
        // bootstrap region — the kernel has a heap from the first allocation.
        let heap_base = core::ptr::addr_of!(HEAP) as usize;
        let early = alloc(layout(64), "early allocation before any page supply");
        check(
            (early as usize).wrapping_sub(heap_base) < HEAP_BYTES,
            "an early page must come from the bootstrap region",
        );
        // SAFETY: `early` came from this allocator with the same layout.
        unsafe { ALLOCATOR.dealloc(early, layout(64)) };

        let (frames, source) = build_supply();
        ALLOCATOR.install_source(source);
        let pool_base = core::ptr::addr_of!(FRAME_POOL) as usize;

        // 1. A page-sized allocation costs exactly one frame, carries no
        //    header, and comes from the pool.
        let before = frames.free_frames();
        let page = alloc(layout(PAGE_SIZE), "page-sized allocation");
        check(
            frames.free_frames() == before - 1,
            "a page-sized allocation must cost exactly one frame",
        );
        check(
            page as usize % PAGE_SIZE == 0,
            "a page-sized object starts at a page boundary, so it has no header",
        );
        check(
            (page as usize).wrapping_sub(pool_base) < PAGE_SIZE * POOL_PAGES,
            "the page must come from the frame pool, not the bootstrap region",
        );

        // 2. The whole page is writable through the live direct map.
        scribble(page, PAGE_SIZE, 0x5A, "page not writable end to end");

        // 3. A drained page is kept back once and reused without a draw.
        // SAFETY: `page` came from this allocator with the same layout.
        unsafe { ALLOCATOR.dealloc(page, layout(PAGE_SIZE)) };
        check(
            frames.free_frames() == before - 1,
            "the drained page is kept back, so its frame is still held",
        );
        let again = alloc(layout(PAGE_SIZE), "reallocation of the retained page");
        check(
            again == page,
            "the retained page must serve the next request",
        );
        check(
            frames.free_frames() == before - 1,
            "reusing the retained page must not draw another frame",
        );

        // 4. A second live page draws a second frame; releasing both leaves
        //    exactly one held.
        let second = alloc(layout(PAGE_SIZE), "second page-sized allocation");
        check(
            frames.free_frames() == before - 2,
            "a second live page costs a second frame",
        );
        check(second != again, "two live pages must be distinct");
        // SAFETY: both came from this allocator with the same layout.
        unsafe {
            ALLOCATOR.dealloc(second, layout(PAGE_SIZE));
            ALLOCATOR.dealloc(again, layout(PAGE_SIZE));
        }
        check(
            frames.free_frames() == before - 1,
            "an idle class keeps one page and returns the rest",
        );

        // 5. Every width up to the granule round-trips — whichever tier the
        //    routing picks for it — honouring the alignment it asked for.
        let mut size = 1usize;
        while size <= PAGE_SIZE {
            let l = layout(size);
            let a = alloc(l, "size-class allocation");
            check(
                a as usize % l.align() == 0,
                "an allocation honours its requested alignment",
            );
            scribble(a, size, 0xC3, "size-class object not writable");
            // SAFETY: `a` came from this allocator with `l`.
            unsafe { ALLOCATOR.dealloc(a, l) };
            let b = alloc(l, "size-class reallocation");
            check(a == b, "a freed object must be handed back out");
            // SAFETY: `b` came from this allocator with `l`.
            unsafe { ALLOCATOR.dealloc(b, l) };
            size *= 2;
        }

        // Page-aligned requests are the slab's whatever their size, and an
        // object of a page-aligned class starts on a page boundary.
        let aligned = Layout::from_size_align(PAGE_SIZE, PAGE_SIZE)
            .unwrap_or_else(|_| fail("page-aligned layout"));
        let pa = alloc(aligned, "page-aligned allocation");
        check(
            pa as usize % PAGE_SIZE == 0,
            "a page-aligned request lands on a page boundary",
        );
        // SAFETY: `pa` came from this allocator with `aligned`.
        unsafe { ALLOCATOR.dealloc(pa, aligned) };

        // 6. Above the granule the byte-granular tier serves, out of the
        //    bootstrap region (this source grows no regions).
        let big = layout(4 * PAGE_SIZE);
        let block = alloc(big, "byte-granular allocation above the granule");
        check(
            (block as usize).wrapping_sub(heap_base) < HEAP_BYTES,
            "a byte-granular block comes from the bootstrap region",
        );
        scribble(
            block,
            4 * PAGE_SIZE,
            0x3C,
            "byte-granular block not writable",
        );
        // SAFETY: `block` came from this allocator with `big`.
        unsafe { ALLOCATOR.dealloc(block, big) };

        log(
            &SERIAL_SINK,
            &Event {
                level: Level::Info,
                id: TEST_PASS,
                message: "aarch64 kslab test: one frame per page, reused and returned",
                fields: &[],
            },
        );
        qemu_exit::exit_success()
    }

    /// A `size`-byte, word-aligned layout.
    fn layout(size: usize) -> Layout {
        Layout::from_size_align(size, 8).unwrap_or_else(|_| fail("layout"))
    }

    /// Build the frame allocator over [`FRAME_POOL`] and the production page
    /// supply over the identity direct map, both leaked `'static` as they are
    /// in a real boot.
    fn build_supply() -> (&'static FrameAllocator, &'static PageOnlySource) {
        let mut map = BootMemoryMap::new();
        map.push(MemoryRegion {
            kind: RegionKind::Usable,
            start: PhysAddr::new(core::ptr::addr_of!(FRAME_POOL) as u64),
            length: (PAGE_SIZE * POOL_PAGES) as u64,
        });
        let Ok(allocator) = FrameAllocator::new(&map) else {
            fail("frame allocator over the pool");
        };
        let frames: &'static FrameAllocator = Box::leak(Box::new(allocator));
        let physmap: &'static DirectPhysMap = Box::leak(Box::new(DirectPhysMap::identity(
            (IDENTITY_GIB as u64) << 30,
        )));
        let source: &'static PageOnlySource = Box::leak(Box::new(PageOnlySource {
            pages: FramePages::new(frames, physmap as &'static (dyn PhysMap + Sync)),
        }));
        check(frames.free_frames() > 0, "the pool holds usable frames");
        (frames, source)
    }
}

// --- Host stub -----------------------------------------------------
#[cfg(not(itest_aarch64))]
fn main() {}
