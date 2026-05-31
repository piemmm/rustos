//! AArch64 stage-1 page-table primitives for the memory-isolation test.
//!
//! This module is the aarch64 analogue of `kernel/arch/{x86_64,riscv64}::paging`.
//! It operates one level *below* `kernel/mem`'s `PageTableOps`: it
//! supplies the architectural mechanism the memory-isolation QEMU
//! vertical needs — two stage-1 translation hierarchies that disagree
//! about a single virtual address, so the MMU faults a process that
//! reaches for another's frame (`AGENTS.md` §4, "memory isolation is
//! enforced by hardware").
//!
//! # Translation scheme
//!
//! 4 KiB granule, three levels (start at L1) covering a 39-bit VA — the
//! aarch64 mirror of riscv64's Sv39, selected by `TCR_EL1.T0SZ = 25`:
//! VA = `L1 (9) | L2 (9) | L3 (9) | offset (12)`. An L1 block descriptor
//! maps 1 GiB, an L2 block 2 MiB, an L3 page 4 KiB (ARM ARM D5.3).
//!
//! Descriptor low bits (ARM ARM D5.3.1): a *table* or *page* descriptor
//! is `0b11`, a *block* descriptor is `0b01`. The lower attributes carry
//! the `MAIR_EL1` attribute index, access permission, shareability, and
//! the access flag.
//!
//! The bit-twiddling that encodes an output address into a descriptor,
//! extracts the per-level table index from a VA, and assembles the
//! attribute words is pure arithmetic and is host-unit-tested below; the
//! `&mut`-recovering table walk and the `TTBR0_EL1`/`SCTLR_EL1` write are
//! gated to the freestanding aarch64 target.

use core::cell::UnsafeCell;
use core::sync::atomic::{AtomicUsize, Ordering};

/// Size of a single page (and of a page-table page).
pub const PAGE_SIZE: usize = 4096;

/// Number of 64-bit entries in a stage-1 page-table page.
pub const ENTRIES_PER_TABLE: usize = 512;

/// Number of paging levels (L1 → L2 → L3).
pub const LEVELS: usize = 3;

/// Descriptor low-bit encodings and lower-attribute fields (ARM ARM
/// D5.3.1 / D5.3.3).
pub mod attrs {
    /// Valid bit (bit 0). Set on every live descriptor.
    pub const VALID: u64 = 1 << 0;
    /// Bit 1: set for a table (at L1/L2) or page (at L3) descriptor,
    /// clear for a block descriptor. With [`VALID`] this gives `0b11`
    /// for table/page and `0b01` for block.
    pub const TABLE_OR_PAGE: u64 = 1 << 1;
    /// Access flag (bit 10). Set eagerly so a platform without hardware
    /// AF management does not fault on first touch.
    pub const AF: u64 = 1 << 10;
    /// Inner-shareable (bits `[9:8] = 0b11`).
    pub const SH_INNER: u64 = 0b11 << 8;
    /// Access permission `0b00` (bits `[7:6]`): read/write at EL1, no EL0
    /// access. The kernel-only mapping the isolation test uses.
    pub const AP_RW_EL1: u64 = 0b00 << 6;
    /// Privileged execute-never (bit 53).
    pub const PXN: u64 = 1 << 53;
    /// Unprivileged execute-never (bit 54).
    pub const UXN: u64 = 1 << 54;

    /// `MAIR_EL1` attribute index for Normal write-back memory (index 0).
    pub const ATTR_IDX_NORMAL: u64 = 0 << 2;
    /// `MAIR_EL1` attribute index for Device-nGnRE memory (index 1).
    pub const ATTR_IDX_DEVICE: u64 = 1 << 2;
}

/// `MAIR_EL1` value pairing attribute index 0 = Normal write-back
/// read/write-allocate and index 1 = Device-nGnRE (ARM ARM D13.2.95).
pub const MAIR_VALUE: u64 = 0xFF | (0x04 << 8);

/// `TCR_EL1` value for a 39-bit TTBR0 region, 4 KiB granule, inner/outer
/// write-back cacheable, inner-shareable walks, with the upper (TTBR1)
/// half disabled. `T0SZ = 25` ⇒ 39-bit VA (three levels from L1).
pub const TCR_VALUE: u64 = {
    let t0sz: u64 = 25;
    let irgn0: u64 = 0b01 << 8;
    let orgn0: u64 = 0b01 << 10;
    let sh0: u64 = 0b11 << 12;
    let tg0: u64 = 0b00 << 14; // 4 KiB granule for TTBR0
    let epd1: u64 = 1 << 23; // disable TTBR1 walks
    let ips: u64 = 0b010 << 32; // 40-bit (1 TiB) physical address size
    t0sz | irgn0 | orgn0 | sh0 | tg0 | epd1 | ips
};

/// `SCTLR_EL1.M` (bit 0): enable stage-1 address translation.
pub const SCTLR_M: u64 = 1 << 0;

/// Physical-address mask of a descriptor's output-address field
/// (bits `[47:12]`).
const ADDR_MASK: u64 = 0x0000_FFFF_FFFF_F000;

/// `true` iff a descriptor is a *block* leaf — valid, with bit 1 clear.
#[must_use]
pub const fn is_block(desc: u64) -> bool {
    (desc & attrs::VALID) != 0 && (desc & attrs::TABLE_OR_PAGE) == 0
}

/// Encode an output physical address plus lower attributes into a block
/// or page descriptor. `paddr` must be page/block-aligned.
#[must_use]
pub const fn descriptor(paddr: u64, lower_attrs: u64) -> u64 {
    (paddr & ADDR_MASK) | lower_attrs
}

/// Encode a next-level table pointer into a table descriptor (`0b11`).
#[must_use]
pub const fn table_descriptor(paddr: u64) -> u64 {
    (paddr & ADDR_MASK) | attrs::VALID | attrs::TABLE_OR_PAGE
}

/// Recover the output physical address a descriptor points at.
#[must_use]
pub const fn phys_from_descriptor(desc: u64) -> u64 {
    desc & ADDR_MASK
}

/// Extract the 9-bit table index for paging `level` (1 = top, 3 = leaf)
/// from a virtual address.
#[must_use]
pub const fn table_index(vaddr: u64, level: usize) -> usize {
    // L1 indexes bits [38:30], L2 [29:21], L3 [20:12].
    let shift = 12 + 9 * (LEVELS - level);
    ((vaddr >> shift) & 0x1FF) as usize
}

/// Lower attributes for a kernel Normal-memory leaf (AF, inner
/// shareable, EL1 RW, MAIR index 0), valid block/page.
///
/// The identity-mapped RAM gigapage must remain *privileged-executable*:
/// it backs the kernel's own `.text`, so after the MMU is enabled the
/// next instruction fetch runs from it. `UXN` is set (EL0 must not
/// execute kernel pages) but `PXN` is left clear.
#[must_use]
pub const fn normal_leaf_attrs(block: bool) -> u64 {
    let base = attrs::VALID
        | attrs::AF
        | attrs::SH_INNER
        | attrs::AP_RW_EL1
        | attrs::ATTR_IDX_NORMAL
        | attrs::UXN;
    if block {
        base
    } else {
        base | attrs::TABLE_OR_PAGE
    }
}

/// Lower attributes for a kernel Device-memory leaf (MAIR index 1,
/// otherwise as [`normal_leaf_attrs`]). Device memory must not be
/// inner-shareable cacheable; the attribute index selects the
/// Device-nGnRE memory type from `MAIR_EL1`.
#[must_use]
pub const fn device_leaf_attrs(block: bool) -> u64 {
    let base = attrs::VALID
        | attrs::AF
        | attrs::AP_RW_EL1
        | attrs::ATTR_IDX_DEVICE
        | attrs::PXN
        | attrs::UXN;
    if block {
        base
    } else {
        base | attrs::TABLE_OR_PAGE
    }
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
/// two [`AddressSpace`]s, each a root plus a 3-level walk for the extra
/// 4 KiB mapping, with spares.
const POOL_SIZE: usize = 16;

/// A statically-allocated pool of zero-initialised page-table pages.
///
/// Allocation is monotonic — frames are never freed — which matches the
/// set-up → run → exit lifecycle of the isolation test. A real allocator
/// lives in `kernel/mem` and is wired in by a later stage.
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

/// A stage-1 address space built on a freshly-allocated L1 root table.
///
/// The constructor identity-maps the low `gigabytes` GiB of physical
/// memory with 1 GiB L1 block descriptors so the kernel's own
/// code/stack/data and the `virt` board's MMIO remain reachable
/// whichever [`AddressSpace`] is active. The first gigabyte (which holds
/// the PL011 UART and the GIC) is mapped Device; the rest Normal.
/// [`Self::map_4k`] adds the finer-grained mappings the
/// memory-isolation test diverges on.
pub struct AddressSpace {
    root_phys: u64,
    root: &'static mut [u64; ENTRIES_PER_TABLE],
}

impl AddressSpace {
    /// Build a new address space identity-mapping `[0, gigabytes GiB)`
    /// with 1 GiB L1 block descriptors.
    ///
    /// `gigabytes` must be `1..=512` (the number of L1 slots). On the
    /// QEMU `virt` board two gigapages cover the device MMIO window and
    /// the RAM base at `0x4000_0000`.
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
        for (i, slot) in root.iter_mut().take(gigabytes).enumerate() {
            let paddr = (i as u64) << 30;
            // GiB 0 holds device MMIO (UART, GIC); the rest is RAM.
            let leaf = if i == 0 {
                device_leaf_attrs(true)
            } else {
                normal_leaf_attrs(true)
            };
            *slot = descriptor(paddr, leaf);
        }
        let root_phys = phys_of(root);
        Some(Self { root_phys, root })
    }

    /// Map `paddr` at `vaddr` with 4 KiB granularity as Normal memory.
    ///
    /// `vaddr` and `paddr` must be page-aligned. Returns `None` on
    /// page-table-pool exhaustion or if the walk meets an existing block
    /// it would have to shatter — the isolation test maps outside the
    /// identity-mapped gigapages so that path is not exercised.
    pub fn map_4k(&mut self, pool: &'static PageTablePool, vaddr: u64, paddr: u64) -> Option<()> {
        if (vaddr & (PAGE_SIZE as u64 - 1)) != 0 || (paddr & (PAGE_SIZE as u64 - 1)) != 0 {
            return None;
        }
        let i1 = table_index(vaddr, 1);
        let i2 = table_index(vaddr, 2);
        let i3 = table_index(vaddr, 3);

        let l2 = ensure_child(self.root, i1, pool)?;
        let l3 = ensure_child(l2, i2, pool)?;
        if (l3[i3] & attrs::VALID) != 0 {
            return None;
        }
        l3[i3] = descriptor(paddr, normal_leaf_attrs(false));
        Some(())
    }

    /// Physical address of the L1 root table (the value programmed into
    /// `TTBR0_EL1`). Exposed so tests can observe it.
    #[must_use]
    pub fn root_phys(&self) -> u64 {
        self.root_phys
    }

    /// Activate this address space: program `MAIR_EL1`, `TCR_EL1`,
    /// `TTBR0_EL1`, and enable the MMU (`SCTLR_EL1.M`), then synchronise.
    ///
    /// # Safety
    ///
    /// The caller must guarantee that this address space identity-maps
    /// the currently-executing `pc`, the current stack, and every MMIO
    /// region the code touches before the next `switch` — otherwise the
    /// CPU faults on the next fetch/access.
    /// [`Self::new_identity_gigapages`] upholds that by identity-mapping
    /// the kernel's gigapages (RAM Normal, MMIO Device).
    #[cfg(all(target_arch = "aarch64", target_os = "none"))]
    pub unsafe fn switch(&self) {
        // SAFETY: the caller asserts the new mappings cover `pc`, `sp`,
        // and MMIO. Programming MAIR/TCR/TTBR0 then setting `SCTLR_EL1.M`
        // is the documented stage-1 enable sequence; the `tlbi vmalle1`
        // + `dsb`/`isb` flush stale translations and ensure the new
        // system-register state is in force before the next access.
        unsafe {
            core::arch::asm!(
                "msr MAIR_EL1, {mair}",
                "msr TCR_EL1, {tcr}",
                "msr TTBR0_EL1, {ttbr}",
                "dsb ish",
                "tlbi vmalle1",
                "dsb ish",
                "isb",
                "mrs {tmp}, SCTLR_EL1",
                "orr {tmp}, {tmp}, {m}",
                "msr SCTLR_EL1, {tmp}",
                "isb",
                mair = in(reg) MAIR_VALUE,
                tcr = in(reg) TCR_VALUE,
                ttbr = in(reg) self.root_phys,
                m = in(reg) SCTLR_M,
                tmp = out(reg) _,
                options(nostack, preserves_flags),
            );
        }
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
    if (entry & attrs::VALID) != 0 {
        if is_block(entry) {
            // A block where we expected a table pointer: refuse rather
            // than shatter a large mapping silently.
            return None;
        }
        let phys = phys_from_descriptor(entry);
        // SAFETY: every non-block valid entry was inserted below with an
        // output address derived from `phys_of(&mut [u64; 512])`, so the
        // round-trip is valid; identity mapping means the physical
        // address is also the address we dereference.
        let child: &'static mut [u64; ENTRIES_PER_TABLE] =
            unsafe { &mut *(phys as *mut [u64; ENTRIES_PER_TABLE]) };
        Some(child)
    } else {
        let child = pool.alloc()?;
        parent[idx] = table_descriptor(phys_of(child));
        Some(child)
    }
}

fn phys_of(table: &[u64; ENTRIES_PER_TABLE]) -> u64 {
    // Identity-mapped: virtual == physical for everything the kernel
    // owns, because the boot trampoline runs with the MMU off and the
    // gigapage identity map preserves it.
    table.as_ptr() as u64
}

#[cfg(test)]
#[path = "paging_tests.rs"]
mod tests;
