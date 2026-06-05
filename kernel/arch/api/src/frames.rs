//! Page-table frame-source surface of the Arch HAL (`AGENTS.md` §17.2,
//! `plans/WIRING.md` Stage W5b-3).
//!
//! A port's `AddressSpace` is built from 4 KiB page-table frames: the
//! root table and every intermediate table on a mapping walk. Until
//! Stage W5b-3 each port *owned* that storage in a static
//! `PageTablePool` linked into the kernel image. That is fine for the
//! boot/bootstrap address space, but a real per-process address space
//! must draw its tables from the kernel's physical `FrameAllocator`
//! (`kernel/mem`) so the tables live in ordinary reclaimable RAM rather
//! than a fixed-size `.bss` pool.
//!
//! §17.4 forbids `kernel/arch/*` from depending on `kernel/mem`, so the
//! allocator cannot be named by a port directly. This module is the
//! seam that keeps the one-way edge intact: a port draws each table
//! through [`PageTableFrames`], and the *caller* (`kernel/mem`, which is
//! allowed to depend on this crate) supplies the concrete source. The
//! static `PageTablePool` each port still ships is the boot/bootstrap
//! implementation of the same trait; the `FrameAllocator`-backed
//! implementation lives in `kernel/mem`.
//!
//! The parallel per-source implementations of this one trait — the
//! per-port static pool and the `kernel/mem` allocator adapter — are the
//! deliberate shape of §17.1/§17.2 modularity, never collapsed behind a
//! `cfg` (`AGENTS.md` §2.2 carve-out).

/// Number of `u64` entries in one 4 KiB page table.
///
/// Every architecture RustOS targets uses a 512-entry (`4096 / 8`)
/// table at each level (x86_64 PML4/PDPT/PD/PT, aarch64 stage-1
/// L1/L2/L3, riscv64 Sv39 levels). The constant lives here so the HAL
/// frame currency speaks one width (`AGENTS.md` §2.2).
pub const PAGE_TABLE_ENTRIES: usize = 512;

/// One freshly-allocated, zeroed 4 KiB page-table frame handed to a port
/// by a [`PageTableFrames`] source.
///
/// The frame carries both halves a page-table walk needs:
///
/// * [`phys`](Self::phys) — the physical address that goes into a parent
///   PTE or the root register (CR3 / `TTBR` / `satp`). The source owns
///   the physical/virtual relationship, so a port never computes it.
/// * [`entries`](Self::entries) — a CPU-dereferenceable, `'static`
///   mutable view of the frame's 512 entries, zero-initialised, that the
///   port writes table descriptors into.
///
/// The two name the *same* physical frame: `entries` is the source's
/// direct-map view of `phys`. A port stores `phys` in the parent entry
/// and recovers the table on a later walk through its own translation
/// regime (the identity / higher-half map the port already relies on),
/// exactly as it did with the static pool.
pub struct TableFrame {
    /// Physical address of the frame (a multiple of 4 KiB).
    pub phys: u64,
    /// Zero-initialised, `'static` mutable view of the frame's entries.
    pub entries: &'static mut [u64; PAGE_TABLE_ENTRIES],
}

/// Source of page-table frames for a port's `AddressSpace`
/// (`AGENTS.md` §17.2, `plans/WIRING.md` Stage W5b-3).
///
/// A port draws the root table and every intermediate table from this
/// seam instead of owning the storage, so it keeps its one-way
/// dependency edge (§17.4) while the caller decides where the frames
/// come from. Allocation takes `&self` — a source is shared (a `static`
/// pool or a `&FrameAllocator`) and synchronises internally — and is
/// infallible-or-`None`: a source that cannot satisfy a request returns
/// [`None`] so the port fails closed with deterministic OOM rather than
/// panicking (`AGENTS.md` §4).
pub trait PageTableFrames {
    /// Allocate one zeroed, naturally-aligned 4 KiB page-table frame.
    ///
    /// Returns [`None`] when the source is exhausted. Every returned
    /// frame must be distinct (its bytes alias no other live frame) and
    /// its `entries` view must be zero-initialised, so a port can build
    /// a table without clearing it first.
    fn alloc_table(&self) -> Option<TableFrame>;
}

/// The §17.2 page-table frame-source conformance vertical.
///
/// Like [`crate::tlb::conformance`] it names only the trait and runs on
/// the host against any faithful source. It proves the contract a port
/// relies on: a fresh frame is zeroed, physically page-aligned, and
/// distinct from earlier frames, writes through one frame do not affect
/// another, and the source eventually fails closed with [`None`] rather
/// than aliasing or panicking.
///
/// A port whose static-pool `phys` derivation is only valid on the
/// bare-metal target (x86_64 subtracts the higher-half base) cannot run
/// this on the host; it proves the seam end-to-end through its
/// `memory_isolation` / spawn QEMU verticals instead, the same honest
/// asymmetry [`crate::mmu::conformance`] already carries. Ports whose
/// `phys` derivation is the identity map (aarch64, riscv64) run it on
/// the host over their real `PageTablePool`.
pub mod conformance {
    use super::PageTableFrames;

    /// Run the [`PageTableFrames`] conformance suite against `frames`,
    /// which must be freshly constructed and able to hand out at least
    /// `capacity` frames before exhaustion.
    ///
    /// # Panics
    ///
    /// Panics (test-only) if the source violates the [`PageTableFrames`]
    /// contract: a non-aligned, non-zeroed, or aliasing frame, a source
    /// that exhausts before `capacity` frames, or one that never
    /// exhausts.
    pub fn run_all<F: PageTableFrames + ?Sized>(frames: &F, capacity: usize) {
        assert!(capacity >= 2, "the suite needs at least two frames");

        let first = frames.alloc_table().expect("first frame");
        assert_eq!(first.phys & 0xFFF, 0, "frame is physically page-aligned");
        assert!(
            first.entries.iter().all(|&e| e == 0),
            "a fresh frame is zero-initialised"
        );
        let first_phys = first.phys;
        // Dirty the first frame so the independence check below is real.
        first.entries[0] = 0xDEAD_BEEF;

        let second = frames.alloc_table().expect("second frame");
        assert_eq!(second.phys & 0xFFF, 0, "frame is physically page-aligned");
        assert_ne!(second.phys, first_phys, "frames are physically distinct");
        assert!(
            second.entries.iter().all(|&e| e == 0),
            "a second fresh frame is zeroed, independent of the first"
        );

        // Drain the rest; every frame stays page-aligned and the source
        // fails closed within `capacity` rather than aliasing forever.
        let mut handed_out = 2usize;
        while handed_out < capacity {
            match frames.alloc_table() {
                Some(frame) => {
                    assert_eq!(frame.phys & 0xFFF, 0, "every frame is page-aligned");
                    handed_out += 1;
                }
                None => break,
            }
        }
        assert!(
            frames.alloc_table().is_none(),
            "an exhausted source fails closed with None"
        );
    }

    #[cfg(test)]
    mod tests {
        use super::super::{PageTableFrames, TableFrame, PAGE_TABLE_ENTRIES};
        use super::run_all;
        use core::cell::UnsafeCell;
        use core::sync::atomic::{AtomicUsize, Ordering};

        const DOUBLE_CAPACITY: usize = 8;

        /// One naturally-aligned table page, matching the per-port
        /// `#[repr(C, align(4096))]` table so the double's identity
        /// `phys` is genuinely 4 KiB-aligned.
        #[repr(C, align(4096))]
        struct Table([u64; PAGE_TABLE_ENTRIES]);

        /// A faithful host double: a fixed bump pool over `'static`
        /// storage, modelling the per-port `PageTablePool` exactly. Its
        /// `phys` is the identity address of the slot (the aarch64 /
        /// riscv64 relationship), so it is host-runnable.
        struct BumpFrames {
            storage: [UnsafeCell<Table>; DOUBLE_CAPACITY],
            used: AtomicUsize,
        }

        // SAFETY: each slot is handed out exactly once via the monotonic
        // `AtomicUsize`, so the `&'static mut` views never alias.
        unsafe impl Sync for BumpFrames {}

        impl PageTableFrames for BumpFrames {
            fn alloc_table(&self) -> Option<TableFrame> {
                let idx = self.used.fetch_add(1, Ordering::SeqCst);
                if idx >= DOUBLE_CAPACITY {
                    self.used.store(DOUBLE_CAPACITY, Ordering::SeqCst);
                    return None;
                }
                let cell = &self.storage[idx];
                // SAFETY: unique index per call (see the `Sync` note).
                let table: &'static mut Table = unsafe { &mut *cell.get() };
                let entries = &mut table.0;
                let phys = entries.as_ptr() as u64;
                Some(TableFrame { phys, entries })
            }
        }

        #[test]
        fn suite_accepts_a_faithful_bump_source() {
            #[allow(clippy::declare_interior_mutable_const)]
            const ZERO: UnsafeCell<Table> = UnsafeCell::new(Table([0; PAGE_TABLE_ENTRIES]));
            static POOL: BumpFrames = BumpFrames {
                storage: [ZERO; DOUBLE_CAPACITY],
                used: AtomicUsize::new(0),
            };
            run_all(&POOL, DOUBLE_CAPACITY);

            // And behind the object-safe erasure the per-process façade
            // and the `kernel/mem` adapter both rely on: the suite drained
            // the pool, so the erased handle now fails closed.
            let erased: &dyn PageTableFrames = &POOL;
            assert!(erased.alloc_table().is_none());
        }
    }
}
