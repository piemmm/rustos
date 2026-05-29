//! Page-table primitives for the Stage-2 memory-isolation test.
//!
//! The test in `tests/integration/memory_isolation` needs two
//! page-table hierarchies that disagree about a single virtual address:
//! a *victim* address space in which the address resolves to a known
//! frame, and an *attacker* address space in which it does not. The CPU
//! must fault the attacker on access. That is the architectural
//! guarantee `AGENTS.md` §4 ("Memory isolation is enforced by hardware")
//! requires — and the test verifies — *at the page-table layer*, before
//! any of the orchestration in `kernel/mem`'s `AddressSpace` is added.
//!
//! This module deliberately operates one level *below* `kernel/mem`:
//!
//! * It does not allocate from `lib/collections::FrameAllocator`. Instead
//!   it uses a tiny, in-`.bss` page-frame pool. The kernel-side trait
//!   plumbing is unrelated to the architectural property under test, and
//!   pulling it in would require Stage-3a's full physical-frame
//!   allocator (not in scope, see crate docs).
//! * It exposes only the operations the test needs: build a PML4 that
//!   identity-maps the first 32 MiB of physical memory, add an extra
//!   4 KiB mapping, switch CR3.
//!
//! When Stage 3a lands the proper allocator-backed page-table type, this
//! module becomes the `unsafe`-correctness bedrock for `kernel/mem`'s
//! `PageTableOps` impl. The current API is intentionally a strict subset
//! so that promotion does not require interface creep (`AGENTS.md` §2.4).

use core::cell::UnsafeCell;
use core::sync::atomic::{AtomicUsize, Ordering};

/// Size of a single x86_64 page-table page.
pub const PAGE_SIZE: usize = 4096;

/// Number of 64-bit entries in a page-table page (PML4 / PDPT / PD / PT).
pub const ENTRIES_PER_TABLE: usize = 512;

/// Page-table entry flags actually used here.
pub mod flags {
    /// Entry is present.
    pub const PRESENT: u64 = 1 << 0;
    /// Writable.
    pub const WRITABLE: u64 = 1 << 1;
    /// Page Size (1 for huge pages at PD or PDPT level).
    pub const HUGE: u64 = 1 << 7;
}

/// One page-table page: 512 × u64, naturally aligned.
#[repr(C, align(4096))]
struct Table([u64; ENTRIES_PER_TABLE]);

impl Table {
    const fn new() -> Self {
        Self([0; ENTRIES_PER_TABLE])
    }
}

/// Maximum number of page-table pages the Stage-2 tests need. Sized for
/// two [`AddressSpace`]s, each with up to 4 levels deep on one extra
/// mapping plus the initial 32 MiB identity map: 2 × (PML4 + PDPT + PD)
/// + spares.
const POOL_SIZE: usize = 16;

/// A statically-allocated pool of zero-initialised page-table pages.
///
/// Allocation is monotonic — frames are never freed. That matches the
/// lifecycle of the Stage-2 tests (set up → run → exit). A real
/// allocator lives in `kernel/mem` and is wired in by Stage 3a.
pub struct PageTablePool {
    storage: [UnsafeCell<Table>; POOL_SIZE],
    used: AtomicUsize,
}

// SAFETY: the pool exposes `&self` allocation but every allocated frame
// is handed out exactly once (monotonic `AtomicUsize`), so distinct
// allocations never alias. Callers receive `&'static mut [u64; 512]`s
// whose lifetimes are disjoint by construction.
unsafe impl Sync for PageTablePool {}

impl Default for PageTablePool {
    fn default() -> Self {
        Self::new()
    }
}

impl PageTablePool {
    /// Construct an empty pool. `const`, so the pool lives in `.bss`.
    #[must_use]
    pub const fn new() -> Self {
        // Build the array of `UnsafeCell<Table>` via a const expression;
        // each cell is zero-initialised by `Table::new`. The `const`
        // initializer is consumed at array-literal expansion time and
        // never re-named, so the `declare_interior_mutable_const` lint
        // is suppressed with rationale per AGENTS.md §15.10. The
        // array itself is `POOL_SIZE * sizeof::<Table>() = 64 KiB`
        // and is materialised straight into the returned `Self`,
        // which lives in `.bss` via `static` storage at every call
        // site — there is no real stack temporary despite the
        // `large_stack_arrays` lint's heuristic.
        #[allow(clippy::declare_interior_mutable_const)]
        const ZERO: UnsafeCell<Table> = UnsafeCell::new(Table::new());
        // `[ZERO; POOL_SIZE]` evaluates the const into each slot;
        // semantically the value is materialised straight into the
        // returned `Self`, which lives in `.bss` via `static` storage
        // at every call site — there is no real stack temporary
        // despite the `large_stack_arrays` lint's heuristic.
        #[allow(clippy::large_stack_arrays)]
        let storage = [ZERO; POOL_SIZE];
        Self {
            storage,
            used: AtomicUsize::new(0),
        }
    }

    /// Allocate a fresh, zero-initialised table page.
    ///
    /// Returns `None` when the pool is exhausted — callers must handle it
    /// as a closed-fail (`AGENTS.md` §4: deterministic OOM, never panic).
    pub fn alloc(&self) -> Option<&'static mut [u64; ENTRIES_PER_TABLE]> {
        let idx = self.used.fetch_add(1, Ordering::SeqCst);
        if idx >= POOL_SIZE {
            // Roll back so subsequent allocations also fail closed rather
            // than overflowing `usize`.
            self.used.store(POOL_SIZE, Ordering::SeqCst);
            return None;
        }
        // SAFETY: monotonic allocator + atomic fetch_add means this index
        // is owned by *this* call uniquely. We cast the `UnsafeCell`
        // pointer to a `&'static mut` once and never alias it.
        let cell = &self.storage[idx];
        let raw = cell.get();
        let table_ref: &'static mut Table = unsafe { &mut *raw };
        Some(&mut table_ref.0)
    }
}

/// An address space built on a freshly-allocated PML4.
///
/// The constructor identity-maps the first 32 MiB with 1 GiB / 2 MiB
/// huge pages so the kernel's own code/stack/data remain reachable
/// regardless of which `AddressSpace` is currently active. The
/// [`Self::map_4k`] method adds finer-grained mappings used by the
/// memory-isolation test.
pub struct AddressSpace {
    pml4_phys: u64,
    pml4: &'static mut [u64; ENTRIES_PER_TABLE],
}

impl AddressSpace {
    /// Build a new address space identity-mapping `[0, 32 MiB)`.
    ///
    /// # Errors
    ///
    /// Returns `None` if the page-table pool is exhausted.
    pub fn new_identity_first_32mib(pool: &'static PageTablePool) -> Option<Self> {
        let pml4 = pool.alloc()?;
        let pdpt = pool.alloc()?;
        let pd = pool.alloc()?;

        let pml4_phys = phys_of(pml4);
        let pdpt_phys = phys_of(pdpt);
        let pd_phys = phys_of(pd);

        pml4[0] = pdpt_phys | flags::PRESENT | flags::WRITABLE;
        pdpt[0] = pd_phys | flags::PRESENT | flags::WRITABLE;
        // 16 × 2 MiB = 32 MiB identity-mapped.
        for (i, slot) in pd.iter_mut().take(16).enumerate() {
            *slot = ((i as u64) << 21) | flags::PRESENT | flags::WRITABLE | flags::HUGE;
        }

        Some(Self { pml4_phys, pml4 })
    }

    /// Map `paddr` at `vaddr` with a 4 KiB page granularity.
    ///
    /// `vaddr` and `paddr` must be 4 KiB-aligned. Returns `None` on
    /// page-table-pool exhaustion. Aborts the existing 2 MiB huge-page
    /// covering `vaddr` if necessary (the Stage-2 tests stay outside
    /// the identity-mapped range so this is not exercised here).
    pub fn map_4k(
        &mut self,
        pool: &'static PageTablePool,
        vaddr: u64,
        paddr: u64,
        writable: bool,
    ) -> Option<()> {
        assert_eq!(vaddr & 0xFFF, 0, "vaddr must be page-aligned");
        assert_eq!(paddr & 0xFFF, 0, "paddr must be page-aligned");

        let mut flags_ = flags::PRESENT;
        if writable {
            flags_ |= flags::WRITABLE;
        }

        let i4 = ((vaddr >> 39) & 0x1FF) as usize;
        let i3 = ((vaddr >> 30) & 0x1FF) as usize;
        let i2 = ((vaddr >> 21) & 0x1FF) as usize;
        let i1 = ((vaddr >> 12) & 0x1FF) as usize;

        let pdpt = ensure_child(self.pml4, i4, pool)?;
        let pd = ensure_child(pdpt, i3, pool)?;

        // Refuse to silently shatter an existing huge page — the test
        // explicitly uses VAs outside the bootstrap identity range so
        // this path returns `None` if anyone hits it.
        if (pd[i2] & flags::HUGE) != 0 {
            return None;
        }
        let pt = ensure_child(pd, i2, pool)?;
        pt[i1] = paddr | flags_;
        Some(())
    }

    /// Switch the active page table to this address space.
    ///
    /// # Safety
    ///
    /// Caller must guarantee that the new PML4 also maps the currently
    /// executing instruction's `rip` and the current stack — otherwise
    /// the CPU will fault on the very next memory access. Both
    /// constructors here uphold that by identity-mapping the kernel's
    /// 0–32 MiB code/stack/data range.
    #[cfg(all(target_arch = "x86_64", target_os = "none"))]
    pub unsafe fn switch(&self) {
        // SAFETY: caller asserts the new mappings cover RIP and RSP; see
        // the `# Safety` paragraph above. `mov cr3, _` is otherwise a
        // pure architectural state change.
        unsafe {
            core::arch::asm!(
                "mov cr3, {p}",
                p = in(reg) self.pml4_phys,
                options(nostack, preserves_flags),
            );
        }
    }

    /// Physical address of this PML4 (i.e. the value that would go into
    /// CR3). Exposed so tests can observe it for assertions.
    #[must_use]
    pub fn pml4_phys(&self) -> u64 {
        self.pml4_phys
    }
}

// `&mut [u64; 512]` in, `&'static mut [u64; 512]` out: the returned
// reference does not borrow from `parent` (it points at a freshly
// alloc'd table from `pool`, or at a sibling table recovered through
// the identity map). `mut_from_ref` / `mut_from_immut` clippy lint
// flags this shape because the function does not return a borrow of
// `parent`'s lifetime — which is exactly the documented contract.
#[allow(clippy::mut_from_ref)]
fn ensure_child(
    parent: &mut [u64; ENTRIES_PER_TABLE],
    idx: usize,
    pool: &'static PageTablePool,
) -> Option<&'static mut [u64; ENTRIES_PER_TABLE]> {
    let entry = parent[idx];
    if entry & flags::PRESENT != 0 {
        // Existing child — recover the `&mut` from the physical address.
        // Identity mapping makes phys = virt here.
        let phys = entry & 0x000F_FFFF_FFFF_F000;
        // SAFETY: every entry that has PRESENT set was inserted by
        // `ensure_child`/`new_identity_first_32mib` with a physical
        // address that came from `phys_of(&mut [u64; 512])`, so the
        // round-trip is valid; identity mapping means we can dereference
        // the physical address directly.
        let child: &'static mut [u64; ENTRIES_PER_TABLE] =
            unsafe { &mut *(phys as usize as *mut [u64; ENTRIES_PER_TABLE]) };
        Some(child)
    } else {
        let child = pool.alloc()?;
        parent[idx] = phys_of(child) | flags::PRESENT | flags::WRITABLE;
        Some(child)
    }
}

fn phys_of(table: &[u64; ENTRIES_PER_TABLE]) -> u64 {
    // Identity-mapped: virtual == physical for everything the kernel
    // owns. The cast is justified because the boot trampoline established
    // an identity map covering all kernel-allocated memory.
    table.as_ptr() as u64
}

#[cfg(test)]
mod tests {
    // Page-table mechanics are tested end-to-end in the
    // `memory_isolation` QEMU integration test (the only environment in
    // which the CPU actually walks them). Host-side unit tests of the
    // bit manipulation would re-implement the CPU's MMU and add nothing
    // that the architectural test does not already prove.
    //
    // This stub exists so `cargo test -p rustos-arch-x86_64` runs cleanly
    // on the host target without emitting a "no tests in module" lint.
    #[test]
    fn page_constants_are_canonical() {
        use super::*;
        assert_eq!(PAGE_SIZE, 4096);
        assert_eq!(ENTRIES_PER_TABLE, 512);
    }
}
