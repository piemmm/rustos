//! Sv39 page-table primitives for the riscv64 memory-isolation test.
//!
//! This module is the riscv64 analogue of `kernel/arch/x86_64::paging`.
//! It operates one level *below* `kernel/mem`'s `PageTableOps`: it
//! supplies the architectural mechanism the memory-isolation QEMU
//! vertical needs — two Sv39 page-table hierarchies that disagree about
//! a single virtual address, so the MMU faults a process that reaches
//! for another's frame (`AGENTS.md` §4, "memory isolation is enforced
//! by hardware").
//!
//! # Sv39
//!
//! Sv39 (RISC-V privileged spec §4.4.1) is a three-level, 39-bit
//! virtual-address scheme: VA = `VPN[2] (9) | VPN[1] (9) | VPN[0] (9) |
//! offset (12)`. Each page-table entry packs the next-level PPN
//! (physical page number) into bits `[53:10]` along with the
//! permission/valid bits in `[9:0]`. The `satp` CSR selects Sv39 with
//! mode `8` in its top four bits and carries the root table's PPN.
//!
//! The bit-twiddling that encodes a PPN into a PTE, extracts the
//! per-level VPN index from a VA, and assembles the `satp` value is
//! pure arithmetic and is host-unit-tested below; the `&mut`-recovering
//! table walk and the `satp` write are gated to the freestanding
//! riscv64 target.

use core::cell::UnsafeCell;
use core::sync::atomic::{AtomicUsize, Ordering};

/// Size of a single page (and of a page-table page).
pub const PAGE_SIZE: usize = 4096;

/// Number of 64-bit entries in an Sv39 page-table page.
pub const ENTRIES_PER_TABLE: usize = 512;

/// Number of paging levels in Sv39.
pub const SV39_LEVELS: usize = 3;

/// `satp` MODE field value selecting Sv39 (privileged spec table 4.3).
pub const SATP_MODE_SV39: u64 = 8;

/// Bit position of the `satp` MODE field on RV64.
pub const SATP_MODE_SHIFT: u64 = 60;

/// Page-table entry permission/valid bits (privileged spec §4.4.1).
pub mod flags {
    /// Entry is valid.
    pub const VALID: u64 = 1 << 0;
    /// Readable.
    pub const READ: u64 = 1 << 1;
    /// Writable.
    pub const WRITE: u64 = 1 << 2;
    /// Executable.
    pub const EXEC: u64 = 1 << 3;
    /// Accessible from user mode.
    pub const USER: u64 = 1 << 4;
    /// Accessed (set this eagerly so platforms without HW A/D updates
    /// do not fault on first touch).
    pub const ACCESSED: u64 = 1 << 6;
    /// Dirty (set eagerly alongside [`WRITE`], same rationale).
    pub const DIRTY: u64 = 1 << 7;
}

/// `true` iff a PTE is a *leaf* — valid and carrying at least one of
/// R/W/X. A valid entry with R=W=X=0 is a pointer to the next level.
#[must_use]
pub const fn pte_is_leaf(pte: u64) -> bool {
    (pte & flags::VALID) != 0 && (pte & (flags::READ | flags::WRITE | flags::EXEC)) != 0
}

/// Encode a physical address into the PPN field of a PTE.
///
/// Sv39 stores `paddr >> 12` in PTE bits `[53:10]`. The low 12 bits of
/// `paddr` (the page offset) are dropped — callers pass page-aligned
/// addresses.
#[must_use]
pub const fn pte_from_phys(paddr: u64, flags: u64) -> u64 {
    ((paddr >> 12) << 10) | flags
}

/// Recover the physical address a PTE points at (its PPN shifted back
/// into place). Inverse of [`pte_from_phys`] modulo the flag bits.
#[must_use]
pub const fn phys_from_pte(pte: u64) -> u64 {
    ((pte >> 10) & 0x0FFF_FFFF_FFFF) << 12
}

/// Extract the 9-bit VPN index for paging `level` (0 = leaf, 2 = root)
/// from a virtual address.
#[must_use]
pub const fn vpn_index(vaddr: u64, level: usize) -> usize {
    ((vaddr >> (12 + 9 * level)) & 0x1FF) as usize
}

/// Assemble the `satp` value selecting Sv39 with `root_phys` as the
/// root table (ASID 0).
#[must_use]
pub const fn satp_sv39(root_phys: u64) -> u64 {
    (SATP_MODE_SV39 << SATP_MODE_SHIFT) | (root_phys >> 12)
}

/// One page-table page: 512 × u64, naturally aligned.
#[repr(C, align(4096))]
struct Table([u64; ENTRIES_PER_TABLE]);

impl Table {
    const fn new() -> Self {
        Self([0; ENTRIES_PER_TABLE])
    }
}

/// Maximum number of page-table pages the memory-isolation test needs:
/// two [`AddressSpace`]s, each a 3-level walk for the gigapage identity
/// map plus one extra 4 KiB mapping, with spares.
const POOL_SIZE: usize = 16;

/// A statically-allocated pool of zero-initialised page-table pages.
///
/// Allocation is monotonic — frames are never freed — which matches the
/// set-up → run → exit lifecycle of the isolation test. A real
/// allocator lives in `kernel/mem` and is wired in by a later stage.
pub struct PageTablePool {
    storage: [UnsafeCell<Table>; POOL_SIZE],
    used: AtomicUsize,
}

// SAFETY: the pool exposes `&self` allocation but every allocated frame
// is handed out exactly once (monotonic `AtomicUsize`), so distinct
// allocations never alias.
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
        #[allow(clippy::declare_interior_mutable_const)]
        const ZERO: UnsafeCell<Table> = UnsafeCell::new(Table::new());
        #[allow(clippy::large_stack_arrays)]
        let storage = [ZERO; POOL_SIZE];
        Self {
            storage,
            used: AtomicUsize::new(0),
        }
    }

    /// Allocate a fresh, zero-initialised table page.
    ///
    /// Returns `None` when the pool is exhausted — callers handle it as
    /// a closed-fail (`AGENTS.md` §4: deterministic OOM, never panic).
    pub fn alloc(&self) -> Option<&'static mut [u64; ENTRIES_PER_TABLE]> {
        let idx = self.used.fetch_add(1, Ordering::SeqCst);
        if idx >= POOL_SIZE {
            self.used.store(POOL_SIZE, Ordering::SeqCst);
            return None;
        }
        // SAFETY: monotonic allocator + atomic fetch_add means this index
        // is owned by *this* call uniquely; the returned `&'static mut`
        // never aliases another.
        let cell = &self.storage[idx];
        let table_ref: &'static mut Table = unsafe { &mut *cell.get() };
        Some(&mut table_ref.0)
    }
}

/// An Sv39 address space built on a freshly-allocated root table.
///
/// The constructor identity-maps the low `gigabytes` GiB of physical
/// memory with 1 GiB leaf entries (R|W|X) so the kernel's own
/// code/stack/data and the `virt` board's MMIO remain reachable
/// whichever [`AddressSpace`] is active. [`Self::map_4k`] adds the
/// finer-grained mappings the memory-isolation test diverges on.
pub struct AddressSpace {
    root_phys: u64,
    root: &'static mut [u64; ENTRIES_PER_TABLE],
}

impl AddressSpace {
    /// Build a new address space identity-mapping `[0, gigabytes GiB)`
    /// with 1 GiB leaf pages.
    ///
    /// `gigabytes` must be `1..=512` (the number of root-table slots in
    /// Sv39). On the QEMU `virt` board four gigapages cover the MMIO
    /// window and the 2 GiB RAM base at `0x8000_0000`.
    ///
    /// # Errors
    ///
    /// Returns `None` if `gigabytes` is out of range or the page-table
    /// pool is exhausted.
    pub fn new_identity_gigapages(pool: &'static PageTablePool, gigabytes: usize) -> Option<Self> {
        if gigabytes == 0 || gigabytes > ENTRIES_PER_TABLE {
            return None;
        }
        let root = pool.alloc()?;
        let root_phys = phys_of(root);
        let leaf = flags::VALID
            | flags::READ
            | flags::WRITE
            | flags::EXEC
            | flags::ACCESSED
            | flags::DIRTY;
        for (i, slot) in root.iter_mut().take(gigabytes).enumerate() {
            let paddr = (i as u64) << 30;
            *slot = pte_from_phys(paddr, leaf);
        }
        Some(Self { root_phys, root })
    }

    /// Map `paddr` at `vaddr` with 4 KiB granularity.
    ///
    /// `vaddr` and `paddr` must be page-aligned. Returns `None` on
    /// page-table-pool exhaustion or if the walk meets an existing leaf
    /// (gigapage / megapage) it would have to shatter — the isolation
    /// test maps outside the identity-mapped gigapages so that path is
    /// not exercised.
    pub fn map_4k(
        &mut self,
        pool: &'static PageTablePool,
        vaddr: u64,
        paddr: u64,
        flags: u64,
    ) -> Option<()> {
        if (vaddr & (PAGE_SIZE as u64 - 1)) != 0 || (paddr & (PAGE_SIZE as u64 - 1)) != 0 {
            return None;
        }
        let i2 = vpn_index(vaddr, 2);
        let i1 = vpn_index(vaddr, 1);
        let i0 = vpn_index(vaddr, 0);

        let l1 = ensure_child(self.root, i2, pool)?;
        let l0 = ensure_child(l1, i1, pool)?;
        if pte_is_leaf(l0[i0]) {
            return None;
        }
        l0[i0] = pte_from_phys(paddr, flags | flags::VALID | flags::ACCESSED | flags::DIRTY);
        Some(())
    }

    /// Switch the active page table to this address space (write `satp`
    /// and flush the TLB).
    ///
    /// # Safety
    ///
    /// The caller must guarantee that this address space also maps the
    /// currently-executing `pc` and the current stack — otherwise the
    /// hart faults on the next fetch/access. [`Self::new_identity_gigapages`]
    /// upholds that by identity-mapping the kernel's gigapages.
    #[cfg(all(target_arch = "riscv64", target_os = "none"))]
    pub unsafe fn switch(&self) {
        let satp = satp_sv39(self.root_phys);
        // SAFETY: the caller asserts the new mappings cover `pc` and
        // `sp`. Writing `satp` then `sfence.vma` is the documented Sv39
        // activation sequence; `sfence.vma x0, x0` flushes all TLB
        // entries so stale translations cannot survive the switch.
        unsafe {
            core::arch::asm!(
                "csrw satp, {satp}",
                "sfence.vma",
                satp = in(reg) satp,
                options(nostack, preserves_flags),
            );
        }
    }

    /// Physical address of the root table (the PPN that goes into
    /// `satp`). Exposed so tests can observe it.
    #[must_use]
    pub fn root_phys(&self) -> u64 {
        self.root_phys
    }
}

// `&mut [u64; 512]` in, `&'static mut [u64; 512]` out: the returned
// reference points at a freshly-alloc'd table from `pool` or at a
// sibling recovered through the identity map, never a borrow of
// `parent` — exactly the shape `mut_from_ref` flags.
#[allow(clippy::mut_from_ref)]
fn ensure_child(
    parent: &mut [u64; ENTRIES_PER_TABLE],
    idx: usize,
    pool: &'static PageTablePool,
) -> Option<&'static mut [u64; ENTRIES_PER_TABLE]> {
    let entry = parent[idx];
    if (entry & flags::VALID) != 0 {
        if pte_is_leaf(entry) {
            // A leaf where we expected a table pointer: refuse rather
            // than shatter a large page silently.
            return None;
        }
        let phys = phys_from_pte(entry);
        // SAFETY: every non-leaf valid entry was inserted below with a
        // PPN derived from `phys_of(&mut [u64; 512])`, so the round-trip
        // is valid; identity mapping means the physical address is also
        // the address we dereference.
        let child: &'static mut [u64; ENTRIES_PER_TABLE] =
            unsafe { &mut *(phys as *mut [u64; ENTRIES_PER_TABLE]) };
        Some(child)
    } else {
        let child = pool.alloc()?;
        // Non-leaf (table pointer): valid set, R/W/X clear.
        parent[idx] = pte_from_phys(phys_of(child), flags::VALID);
        Some(child)
    }
}

fn phys_of(table: &[u64; ENTRIES_PER_TABLE]) -> u64 {
    // Identity-mapped: virtual == physical for everything the kernel
    // owns, because the boot trampoline runs with `satp = 0` (bare) and
    // the gigapage identity map preserves it.
    table.as_ptr() as u64
}

#[cfg(test)]
#[path = "paging_tests.rs"]
mod tests;
