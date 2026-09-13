//! Page-table frame-source surface of the Arch HAL (
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
//! the charter forbids `kernel/arch/*` from depending on `kernel/mem`, so the
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
//! deliberate shape of modularity, never collapsed behind a
//! `cfg` (carve-out).

use tairix_sync::Once;

/// Number of `u64` entries in one 4 KiB page table.
///
/// Every architecture TAIRiX targets uses a 512-entry (`4096 / 8`)
/// table at each level (x86_64 PML4/PDPT/PD/PT, aarch64 stage-1
/// L1/L2/L3, riscv64 Sv39 levels). The constant lives here so the HAL
/// frame currency speaks one width.
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
/// and recovers the table on a later walk by asking the source for it
/// again ([`PageTableFrames::table_at`]), so the physical/virtual
/// relationship stays the source's alone.
pub struct TableFrame {
    /// Physical address of the frame (a multiple of 4 KiB).
    pub phys: u64,
    /// Zero-initialised, `'static` mutable view of the frame's entries.
    pub entries: &'static mut [u64; PAGE_TABLE_ENTRIES],
}

/// Source of page-table frames for a port's `AddressSpace`
/// (`plans/WIRING.md` Stage W5b-3).
///
/// A port draws the root table and every intermediate table from this
/// seam instead of owning the storage, so it keeps its one-way
/// dependency edge while the caller decides where the frames
/// come from. Allocation takes `&self` — a source is shared (a `static`
/// pool or a `&FrameAllocator`) and synchronises internally — and is
/// infallible-or-`None`: a source that cannot satisfy a request returns
/// [`None`] so the port fails closed with deterministic OOM rather than
/// panicking.
///
/// The shared-and-internally-synchronised contract is expressed as a
/// [`Sync`] supertrait: a source is reached concurrently through a
/// `&'static dyn PageTableFrames` (every CPU's spawn path shares the one
/// kernel source), so it must be safe to share across threads. Making it
/// `Sync` also makes a `&'static dyn PageTableFrames` [`Send`], which lets
/// a port's `AddressSpace` (which retains the source) be the `Send`
/// `LiveUserSpace` a task carries across CPUs (`plans/PI.md` 5d-0-ii (b′)).
/// Every implementor (each port's `PageTablePool`, the kernel
/// `FrameTableSource`) is already `Sync`.
pub trait PageTableFrames: Sync {
    /// Allocate one zeroed, naturally-aligned 4 KiB page-table frame.
    ///
    /// Returns [`None`] when the source is exhausted. Every returned
    /// frame must be distinct (its bytes alias no other live frame) and
    /// its `entries` view must be zero-initialised, so a port can build
    /// a table without clearing it first.
    fn alloc_table(&self) -> Option<TableFrame>;

    /// Recover the CPU-dereferenceable view of the table frame at
    /// physical address `phys`, or [`None`] when this source cannot
    /// reach it.
    ///
    /// The inverse of the `phys`/`entries` pairing
    /// [`Self::alloc_table`] hands out, and the only way a port turns a
    /// parent entry's output address back into a table it can read or
    /// write: a walk resolves each level through this, never by
    /// dereferencing `phys` itself. Keeping the derivation here is what
    /// lets a source place its frames wherever its own map puts them —
    /// a higher-half direct map, a slot in a static pool — while the
    /// port stays ignorant of the relationship.
    ///
    /// [`None`] is the fail-closed answer for a `phys` this source did
    /// not hand out: a corrupt or hostile descriptor's arbitrary
    /// address makes the walk report "not mapped" instead of
    /// dereferencing whatever the integer happens to name. The returned
    /// pointer is valid for the frame's whole 512 entries and aliases
    /// the `entries` view of the same `alloc_table`, so a caller must
    /// hold exclusive access before minting a `&mut` from it.
    fn table_at(&self, phys: u64) -> Option<*mut [u64; PAGE_TABLE_ENTRIES]>;

    /// Return the table frame at physical address `phys` to this source.
    ///
    /// `phys` **must** be the `phys` of a [`TableFrame`] this source
    /// handed out through [`Self::alloc_table`] and that the caller will
    /// never touch again — the port calls this from its address-space
    /// teardown once the frame holds no live translation, and the
    /// teardown walk only yields frames the hierarchy itself drew, which
    /// upholds the contract. A source whose backing supports reuse (the
    /// kernel `FrameAllocator`-backed production source) makes the frame
    /// allocatable again, so a dead process's page tables return to the
    /// system; a fixed bump source (the per-port boot `PageTablePool`,
    /// whose storage is permanent kernel image `.bss` and whose spaces
    /// are never torn down) retires the frame without reuse. Never
    /// panics; a double free of a recycled frame is refused by the
    /// backing allocator, but a source cannot always distinguish a
    /// *foreign* `phys` from a reserved frame, so passing one is a
    /// caller bug, not a checked error.
    fn free_table(&self, phys: u64);
}

/// The frame source a walk of a CPU's **active** translation root
/// recovers its tables through, published once by the boot wiring.
///
/// A fault-time walk — the software access-flag fix-up the aarch64 and
/// riscv64 ports perform in exception context — holds no `AddressSpace`,
/// so it cannot ask that space's own source for a table. The kernel's
/// production source reaches every table frame in RAM through its direct
/// map, the ports' static pools included, so one publication serves
/// every root a CPU can be running on. Before it is published (on the
/// host, and during boot before the frame allocator exists) such a walk
/// has no way to reach a table and fails closed.
static ACTIVE_FRAMES: Once<&'static dyn PageTableFrames> = Once::new();

/// Publish the source free-standing walks of the active root draw their
/// tables from. The first publication wins; a later one changes nothing.
pub fn publish_active_frames(frames: &'static dyn PageTableFrames) {
    let _ = ACTIVE_FRAMES.call_once_infallible(|| frames);
}

/// The published [`publish_active_frames`] source, or [`None`] while none
/// has been published.
#[must_use]
pub fn active_frames() -> Option<&'static dyn PageTableFrames> {
    ACTIVE_FRAMES.get().ok().flatten().copied()
}

/// Index of the slot a static table pool handed `phys` out of, given the
/// physical address of the pool's first slot and its capacity — the one
/// derivation every port's `PageTablePool` shares, so none re-derives it.
///
/// A pool's slots are contiguous, naturally-aligned 4 KiB tables, so the
/// index is the byte offset divided by the table size. Resolving `phys`
/// to an *index* rather than to a pointer is what lets a pool answer
/// [`PageTableFrames::table_at`] from the slot itself: a pointer rebuilt
/// from the integer would carry no provenance for the storage it names.
/// [`None`] for a `phys` below the pool, misaligned to a slot boundary,
/// or past its capacity — a foreign address names no slot, so the walk
/// asking for it fails closed.
#[must_use]
pub fn pool_slot_of(base_phys: u64, capacity: usize, phys: u64) -> Option<usize> {
    const STRIDE: u64 = core::mem::size_of::<[u64; PAGE_TABLE_ENTRIES]>() as u64;
    let offset = phys.checked_sub(base_phys)?;
    if offset % STRIDE != 0 {
        return None;
    }
    let index = usize::try_from(offset / STRIDE).ok()?;
    (index < capacity).then_some(index)
}

/// Reclaim every table frame of a page-table hierarchy, post-order,
/// returning each to `frames` — the one teardown walk every port's
/// `AddressSpace` reuses instead of re-deriving its own (the descriptor
/// predicate is the only genuinely per-ISA part, so it is the closure).
///
/// The walk starts at the table at `root_phys` (depth `0`) and descends
/// through every entry `child_of` classifies as a pointer to a child
/// table, freeing children before parents and the root last, so no frame
/// is released while a live descriptor still points at it. Block/leaf
/// descriptors are never descended into or freed — only *table* frames
/// are reclaimed; leaf frames (user RAM, MMIO) belong to their own
/// owners. A `phys` `frames` cannot reach ([`PageTableFrames::table_at`]
/// answering [`None`]) is neither descended into *nor freed*: a source
/// that never handed the address out cannot own the frame, so freeing it
/// would hand an unrelated frame back to the allocator on the strength of
/// a corrupt descriptor. The walk continues past it; at worst a table
/// whose parent entry was clobbered leaks, which is the safe side of that
/// trade.
///
/// * `child_of(entry, depth)` — `Some(child_phys)` when `entry`, read
///   from a table at `depth`, is a valid pointer to a child table;
///   `None` for invalid entries and block/page leaves. Returning `None`
///   at the deepest level is the caller's responsibility (a leaf-level
///   descriptor must never classify as a table).
///
/// Recursion depth is bounded by the architecture's table depth (at most
/// four levels on every TAIRiX target).
///
/// # Safety
///
/// The caller must guarantee, exactly as its own mapping walk does, that
/// every `phys` reachable through `child_of` (including `root_phys`)
/// names a live table frame of *this* hierarchy, that the hierarchy is
/// not the active translation of any CPU, and that no other reference to
/// those tables is live during the walk.
pub unsafe fn reclaim_hierarchy<C>(root_phys: u64, frames: &dyn PageTableFrames, child_of: &C)
where
    C: Fn(u64, usize) -> Option<u64>,
{
    // SAFETY: forwarded caller contract (see above).
    unsafe { reclaim_at(root_phys, 0, frames, child_of) }
}

/// Recursive post-order step of [`reclaim_hierarchy`].
///
/// # Safety
///
/// As [`reclaim_hierarchy`]; `table_phys` names a live table of the
/// hierarchy at `depth`.
unsafe fn reclaim_at<C>(table_phys: u64, depth: usize, frames: &dyn PageTableFrames, child_of: &C)
where
    C: Fn(u64, usize) -> Option<u64>,
{
    // An address this source never handed out names no frame it owns, so
    // it is left alone entirely rather than handed to `free_table`.
    let Some(table) = frames.table_at(table_phys) else {
        return;
    };
    // Four levels is the deepest hierarchy any TAIRiX target walks
    // (x86_64 PML4→PT); a `child_of` that classifies a leaf-level entry
    // as a table would otherwise walk leaf frame contents as
    // descriptors, so the depth is bounded here as well (fail closed).
    if depth < 4 {
        // SAFETY: the caller guarantees `table_phys` names a live table
        // this hierarchy owns, so the source's view of it is
        // dereferenceable and no other reference aliases it during the
        // walk.
        let entries = unsafe { &*table };
        for &entry in entries {
            if let Some(child_phys) = child_of(entry, depth) {
                // SAFETY: `child_of` classified `entry` as a valid child
                // table pointer of this hierarchy — the caller's contract
                // extends to it.
                unsafe {
                    reclaim_at(child_phys, depth + 1, frames, child_of);
                }
            }
        }
    }
    frames.free_table(table_phys);
}

/// The page-table frame-source conformance vertical.
///
/// Like [`crate::tlb::conformance`] it names only the trait and runs on
/// the host against any faithful source. It proves the contract a port
/// relies on: a fresh frame is zeroed, physically page-aligned, and
/// distinct from earlier frames, writes through one frame do not affect
/// another, [`PageTableFrames::table_at`] recovers exactly the frame a
/// `phys` was handed out with and fails closed on an address the source
/// never handed out, and the source eventually fails closed with
/// [`None`] rather than aliasing or panicking.
///
/// Every port runs this on the host over its real `PageTablePool`,
/// including x86_64's, whose `phys` subtracts the higher-half base:
/// [`PageTableFrames::table_at`] is defined as the inverse of whatever
/// each source's `phys` derivation is, so the suite never needs to know
/// which relationship a source keeps.
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
        let second_phys = second.phys;

        // `table_at` is the walk's only way back to a table, so it must
        // name the very frame the `phys` was handed out with — the byte a
        // parent entry's output address resolves to is the byte the child
        // table was built in.
        let recovered = frames
            .table_at(first_phys)
            .expect("a phys this source handed out is recoverable");
        // SAFETY: `first_phys` was handed out by this source and the
        // `entries` view of it was dropped above, so this is the only live
        // reference to the frame.
        assert_eq!(
            unsafe { &*recovered }[0],
            0xDEAD_BEEF,
            "table_at recovers the frame the phys was handed out with"
        );
        let other = frames
            .table_at(second_phys)
            .expect("a phys this source handed out is recoverable");
        assert!(
            !core::ptr::eq(recovered, other),
            "distinct frames recover to distinct tables"
        );

        // Fail closed rather than dereference an address the source never
        // handed out: a corrupt or hostile parent entry carries an
        // arbitrary output address, and the walk must read it as "not
        // mapped".
        assert!(
            frames.table_at(first_phys | 0x8).is_none(),
            "an address off a table boundary names no table"
        );
        assert!(
            frames.table_at(u64::MAX - 0xFFF).is_none(),
            "an address this source never handed out names no table"
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
        use core::sync::atomic::{AtomicU64, AtomicUsize, Ordering};

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
            freed: [AtomicU64; DOUBLE_CAPACITY],
            freed_len: AtomicUsize,
        }

        // SAFETY: each slot is handed out exactly once via the monotonic
        // `AtomicUsize`, so the `&'static mut` views never alias; the
        // freed log is plain atomics.
        unsafe impl Sync for BumpFrames {}

        impl BumpFrames {
            const fn new() -> Self {
                // The array initialisers need a `const`, and copying it per
                // slot is the point: each element is its own cell.
                #[allow(clippy::declare_interior_mutable_const)]
                const ZERO: UnsafeCell<Table> = UnsafeCell::new(Table([0; PAGE_TABLE_ENTRIES]));
                #[allow(clippy::declare_interior_mutable_const)]
                const FREED: AtomicU64 = AtomicU64::new(0);
                // `const`, so the pool lands in the `static` it initialises
                // rather than a runtime stack frame, despite the
                // `large_stack_arrays` heuristic.
                #[allow(clippy::large_stack_arrays)]
                Self {
                    storage: [ZERO; DOUBLE_CAPACITY],
                    used: AtomicUsize::new(0),
                    freed: [FREED; DOUBLE_CAPACITY],
                    freed_len: AtomicUsize::new(0),
                }
            }

            /// Every `phys` handed back, in the order it was returned;
            /// slots past [`Self::freed_count`] stay zero.
            fn freed_order(&self) -> [u64; DOUBLE_CAPACITY] {
                let mut out = [0u64; DOUBLE_CAPACITY];
                for (slot, out) in self.freed.iter().zip(out.iter_mut()) {
                    *out = slot.load(Ordering::SeqCst);
                }
                out
            }

            fn freed_count(&self) -> usize {
                self.freed_len.load(Ordering::SeqCst)
            }
        }

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

            /// The slot the pool handed `phys` out of.
            ///
            /// Rebuilding the pointer from the integer instead would strip
            /// the storage's provenance, so the offset is taken from the
            /// storage base pointer — the same shape a port's pool uses,
            /// and the shape a direct-map source gets for free from its
            /// window base. A `phys` from elsewhere fails closed.
            fn table_at(&self, phys: u64) -> Option<*mut [u64; PAGE_TABLE_ENTRIES]> {
                let base = self.storage.as_ptr() as u64;
                let idx = super::super::pool_slot_of(base, DOUBLE_CAPACITY, phys)?;
                Some(self.storage[idx].get().cast())
            }

            fn free_table(&self, phys: u64) {
                // A bump pool retires a returned frame without reuse,
                // exactly like the per-port boot pools it models; log the
                // return so the suite can assert the discipline.
                let slot = self.freed_len.fetch_add(1, Ordering::SeqCst);
                assert!(slot < DOUBLE_CAPACITY, "more frees than the pool can hold");
                self.freed[slot].store(phys, Ordering::SeqCst);
            }
        }

        #[test]
        fn reclaim_hierarchy_frees_every_table_post_order_and_only_tables() {
            static POOL: BumpFrames = BumpFrames::new();

            // A synthetic three-level hierarchy over the identity-phys
            // double: bit 0 marks a table pointer, bit 1 a leaf — the
            // shape every port's descriptors reduce to for the walk.
            const TABLE: u64 = 1;
            const LEAF: u64 = 2;
            let root = POOL.alloc_table().expect("root");
            let root_phys = root.phys;
            let mid = POOL.alloc_table().expect("mid");
            let mid_phys = mid.phys;
            let deep = POOL.alloc_table().expect("deep");
            let deep_phys = deep.phys;
            root.entries[0] = mid_phys | TABLE;
            root.entries[1] = LEAF; // a block leaf: never descended, never freed
            mid.entries[7] = deep_phys | TABLE;
            deep.entries[3] = LEAF; // deepest level holds only leaves

            let child_of = |entry: u64, _depth: usize| -> Option<u64> {
                ((entry & TABLE) != 0).then_some(entry & !0xFFF)
            };
            // SAFETY: every phys reachable through `child_of` names a live
            // table of this test hierarchy (slots the pool handed out), the
            // hierarchy is no CPU's translation, and no other reference to
            // the tables is live during the walk.
            unsafe {
                super::super::reclaim_hierarchy(root_phys, &POOL, &child_of);
            }

            // Post-order: the deepest table first, the root last, each
            // exactly once, and no leaf frame ever freed.
            assert_eq!(POOL.freed_count(), 3);
            assert_eq!(POOL.freed_order()[..3], [deep_phys, mid_phys, root_phys]);
        }

        /// A hierarchy holding a descriptor the source cannot reach —
        /// what a corrupt or foreign parent entry looks like — still
        /// gives up every frame the source owns, never walks the bytes
        /// the unreachable address happens to name, and never hands that
        /// address to `free_table`: a source that did not hand it out
        /// does not own the frame, and freeing it on the strength of a
        /// clobbered descriptor would return an unrelated frame to the
        /// allocator.
        #[test]
        fn reclaim_hierarchy_leaves_an_unreachable_table_alone() {
            static POOL: BumpFrames = BumpFrames::new();
            const TABLE: u64 = 1;
            let root = POOL.alloc_table().expect("root");
            let root_phys = root.phys;
            // Two children: one the pool handed out, one it never did.
            let mid = POOL.alloc_table().expect("mid");
            let mid_phys = mid.phys;
            root.entries[0] = mid_phys | TABLE;
            root.entries[1] = 0x1_0000_0000 | TABLE;

            let child_of = |entry: u64, _depth: usize| -> Option<u64> {
                ((entry & TABLE) != 0).then_some(entry & !0xFFF)
            };
            // SAFETY: `mid_phys` and `root_phys` name live tables of this
            // test hierarchy; the foreign address is never dereferenced
            // (which is the property under test), the hierarchy is no
            // CPU's translation, and no other reference is live.
            unsafe {
                super::super::reclaim_hierarchy(root_phys, &POOL, &child_of);
            }
            assert_eq!(POOL.freed_count(), 2);
            assert_eq!(
                POOL.freed_order()[..2],
                [mid_phys, root_phys],
                "the unreachable child is neither walked nor freed"
            );
        }

        #[test]
        fn pool_slot_of_maps_a_handed_out_phys_to_its_slot() {
            use super::super::pool_slot_of;
            const BASE: u64 = 0x8020_0000;
            const STRIDE: u64 = 4096;
            assert_eq!(pool_slot_of(BASE, 4, BASE), Some(0));
            assert_eq!(pool_slot_of(BASE, 4, BASE + 3 * STRIDE), Some(3));
        }

        #[test]
        fn pool_slot_of_fails_closed_off_the_pool() {
            use super::super::pool_slot_of;
            const BASE: u64 = 0x8020_0000;
            const STRIDE: u64 = 4096;
            // Below the pool, past its capacity, and off a slot boundary:
            // a foreign address names no slot, so the walk asking for it
            // reads as "not mapped" rather than dereferencing it.
            assert_eq!(pool_slot_of(BASE, 4, BASE - STRIDE), None);
            assert_eq!(pool_slot_of(BASE, 4, BASE + 4 * STRIDE), None);
            assert_eq!(pool_slot_of(BASE, 4, BASE + 8), None);
            assert_eq!(pool_slot_of(BASE, 0, BASE), None);
        }

        #[test]
        fn suite_accepts_a_faithful_bump_source() {
            static POOL: BumpFrames = BumpFrames::new();
            run_all(&POOL, DOUBLE_CAPACITY);

            // And behind the object-safe erasure the per-process façade
            // and the `kernel/mem` adapter both rely on: the suite drained
            // the pool, so the erased handle now fails closed.
            let erased: &dyn PageTableFrames = &POOL;
            assert!(erased.alloc_table().is_none());
        }
    }
}
