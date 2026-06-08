//! AArch64 stage-1 page-table primitives for the memory-isolation test.
//!
//! This module is the aarch64 analogue of `kernel/arch/{x86_64,riscv64}::paging`.
//! It implements the Arch HAL page-table surface
//! ([`rustos_arch_api::mmu::AddressSpace`] +
//! [`rustos_arch_api::tlb::TlbShootdown`]) `kernel/mem` drives: it
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

use rustos_arch_api::frames::{PageTableFrames, TableFrame};
use rustos_arch_api::mmu::{AddressSpace as MmuAddressSpace, BlockSplit, MapError, PageFlags};
use rustos_arch_api::tlb::TlbShootdown;

/// Size of a single page (and of a page-table page).
pub const PAGE_SIZE: usize = 4096;

/// Size of an L2 block descriptor's translation (2 MiB).
///
/// The granularity [`AddressSpace::split_block`] re-expresses a coarse
/// block down to before reaching 4 KiB leaves, and the alignment the
/// guard-page arena ([`AddressSpace::prepare_guard_arena`]) is laid out
/// at so each guard page becomes its own L3 leaf without disturbing a
/// neighbour.
pub const BLOCK_2MIB: u64 = 2 * 1024 * 1024;

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
    /// Access permission `0b01` (bits `[7:6]`): read/write at EL1 **and**
    /// EL0. Used for an EL0 data mapping (e.g. a user stack).
    pub const AP_RW_EL0: u64 = 0b01 << 6;
    /// Access permission `0b11` (bits `[7:6]`): read-only at EL1 **and**
    /// EL0. Used for an EL0 code mapping (executable but not writable).
    pub const AP_RO_EL0: u64 = 0b11 << 6;
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

/// Lower attributes for an **EL0-executable** Normal-memory page leaf:
/// read-only at EL1 and EL0 (`AP_RO_EL0`), privileged-execute-never
/// (`PXN`, so EL1 cannot run user code) but *unprivileged*-executable
/// (`UXN` clear). The output is a page descriptor (`TABLE_OR_PAGE`); EL0
/// code is always mapped at 4 KiB granularity.
#[must_use]
pub const fn el0_code_leaf_attrs() -> u64 {
    attrs::VALID
        | attrs::TABLE_OR_PAGE
        | attrs::AF
        | attrs::SH_INNER
        | attrs::AP_RO_EL0
        | attrs::ATTR_IDX_NORMAL
        | attrs::PXN
}

/// Lower attributes for an **EL0 read-only, non-executable** Normal-memory
/// page leaf: read-only at EL1 and EL0 (`AP_RO_EL0`) and execute-never at
/// both ELs (`PXN | UXN`). Used for a read-only EL0 data page — an `rxe`
/// `ReadOnly` segment (`.rodata`) or the kernel-written process startup
/// block — where [`el0_code_leaf_attrs`] would wrongly leave the page
/// EL0-executable. The output is a page descriptor (`TABLE_OR_PAGE`).
#[must_use]
pub const fn el0_rodata_leaf_attrs() -> u64 {
    attrs::VALID
        | attrs::TABLE_OR_PAGE
        | attrs::AF
        | attrs::SH_INNER
        | attrs::AP_RO_EL0
        | attrs::ATTR_IDX_NORMAL
        | attrs::PXN
        | attrs::UXN
}

/// Lower attributes for an **EL0-writable** Normal-memory page leaf:
/// read/write at EL1 and EL0 (`AP_RW_EL0`), execute-never at both ELs
/// (`PXN | UXN`). Used for an EL0 data page such as a user stack. The
/// output is a page descriptor (`TABLE_OR_PAGE`).
#[must_use]
pub const fn el0_data_leaf_attrs() -> u64 {
    attrs::VALID
        | attrs::TABLE_OR_PAGE
        | attrs::AF
        | attrs::SH_INNER
        | attrs::AP_RW_EL0
        | attrs::ATTR_IDX_NORMAL
        | attrs::PXN
        | attrs::UXN
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

impl PageTableFrames for PageTablePool {
    fn alloc_table(&self) -> Option<TableFrame> {
        let entries = self.alloc()?;
        // The kernel's own memory is identity-mapped (MMU-off boot then a
        // gigapage identity map), so a table's virtual address is its
        // physical address (`AGENTS.md` §17.2 / `plans/WIRING.md` W5b-3 —
        // the bootstrap frame source).
        let phys = phys_of(entries);
        Some(TableFrame { phys, entries })
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
    /// The frame source the page-table walk allocates intermediate
    /// tables from, retained so the [`rustos_arch_api::mmu::AddressSpace`]
    /// HAL impl can install mappings without the caller re-supplying it.
    /// The static [`PageTablePool`] is the boot/bootstrap source; a real
    /// per-process space is built over the `kernel/mem` frame-allocator
    /// source (`plans/WIRING.md` W5b-3).
    frames: &'static dyn PageTableFrames,
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
    pub fn new_identity_gigapages(
        frames: &'static dyn PageTableFrames,
        gigabytes: usize,
    ) -> Option<Self> {
        if gigabytes == 0 || gigabytes > ENTRIES_PER_TABLE {
            return None;
        }
        let TableFrame {
            phys: root_phys,
            entries: root,
        } = frames.alloc_table()?;
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
        Some(Self {
            root_phys,
            root,
            frames,
        })
    }

    /// `true` if `vaddr` already resolves to a live leaf (block or page)
    /// in this hierarchy.
    ///
    /// A read-only stage-1 walk used by the
    /// [`rustos_arch_api::mmu::AddressSpace`] HAL impl to report
    /// [`rustos_arch_api::mmu::MapError::AlreadyMapped`] rather than
    /// silently clobber an existing mapping. It dereferences present
    /// table descriptors through the identity map (phys == virt for every
    /// table the kernel owns), the same round-trip [`ensure_child`] uses.
    fn leaf_present(&self, vaddr: u64) -> bool {
        let e1 = self.root[table_index(vaddr, 1)];
        if (e1 & attrs::VALID) == 0 {
            return false;
        }
        if is_block(e1) {
            return true;
        }
        // SAFETY: a present table descriptor holds an output address
        // `ensure_child` wrote from `phys_of(&mut [u64; 512])`; identity
        // mapping makes it dereferenceable directly.
        let l2 = unsafe { &*(phys_from_descriptor(e1) as *const [u64; ENTRIES_PER_TABLE]) };
        let e2 = l2[table_index(vaddr, 2)];
        if (e2 & attrs::VALID) == 0 {
            return false;
        }
        if is_block(e2) {
            return true;
        }
        // SAFETY: as above — a present L2 table descriptor's output
        // address is a valid identity-mapped table.
        let l3 = unsafe { &*(phys_from_descriptor(e2) as *const [u64; ENTRIES_PER_TABLE]) };
        (l3[table_index(vaddr, 3)] & attrs::VALID) != 0
    }

    /// Map `paddr` at `vaddr` with 4 KiB granularity as Normal memory
    /// (kernel-only, EL1 RW, execute-never at EL0).
    ///
    /// `vaddr` and `paddr` must be page-aligned. Returns `None` on
    /// page-table-pool exhaustion or if the walk meets an existing block
    /// it would have to shatter — the isolation test maps outside the
    /// identity-mapped gigapages so that path is not exercised.
    pub fn map_4k(
        &mut self,
        frames: &'static dyn PageTableFrames,
        vaddr: u64,
        paddr: u64,
    ) -> Option<()> {
        self.map_4k_with_attrs(frames, vaddr, paddr, normal_leaf_attrs(false))
    }

    /// Map `paddr` at `vaddr` with 4 KiB granularity using the supplied
    /// page-leaf `leaf_attrs` (e.g. [`el0_code_leaf_attrs`] /
    /// [`el0_data_leaf_attrs`] for an EL0 user mapping). `map_4k` is this
    /// with the kernel-only [`normal_leaf_attrs`], so there is one walk
    /// implementation (`AGENTS.md` §2.2).
    ///
    /// `vaddr` and `paddr` must be page-aligned. Returns `None` on
    /// page-table-pool exhaustion or if the walk meets an existing block
    /// it would have to shatter.
    pub fn map_4k_with_attrs(
        &mut self,
        frames: &'static dyn PageTableFrames,
        vaddr: u64,
        paddr: u64,
        leaf_attrs: u64,
    ) -> Option<()> {
        if (vaddr & (PAGE_SIZE as u64 - 1)) != 0 || (paddr & (PAGE_SIZE as u64 - 1)) != 0 {
            return None;
        }
        let i1 = table_index(vaddr, 1);
        let i2 = table_index(vaddr, 2);
        let i3 = table_index(vaddr, 3);

        let l2 = ensure_child(self.root, i1, frames)?;
        let l3 = ensure_child(l2, i2, frames)?;
        if (l3[i3] & attrs::VALID) != 0 {
            return None;
        }
        l3[i3] = descriptor(paddr, leaf_attrs);
        Some(())
    }

    /// Split the coarse block descriptor(s) covering `vaddr` down to
    /// 4 KiB granularity, preserving the mapped output address and every
    /// attribute, so the single 4 KiB page containing `vaddr` can then be
    /// torn down with [`MmuAddressSpace::unmap`] without disturbing its
    /// neighbours.
    ///
    /// This is the foundation of the kthread guard page (`plans/PI.md`):
    /// a guard page that falls inside a region the boot path mapped with
    /// coarse 1 GiB / 2 MiB *block* descriptors cannot be unmapped while
    /// it is part of a block, because a block has no per-4 KiB leaf to
    /// clear. Splitting re-expresses the same translation as a table of
    /// finer descriptors — an L1 block (1 GiB) becomes a table of 512 ×
    /// 2 MiB blocks, then the 2 MiB block covering `vaddr` becomes a table
    /// of 512 × 4 KiB pages — leaving every address translating
    /// identically but now at 4 KiB granularity.
    ///
    /// The split is **break-before-make-free for the running region**: it
    /// only ever *adds* table levels that reproduce the existing
    /// translation, never invalidating a live address, so it is safe to
    /// run against the active translation regime (the resulting tables map
    /// the same physical frames with the same permissions). It is
    /// idempotent — a level that is already a table is left untouched — so
    /// re-splitting an already-fine region succeeds without allocating.
    /// The caller is responsible for any TLB maintenance after a
    /// subsequent [`MmuAddressSpace::unmap`]; the split itself changes no
    /// translation result and so needs none.
    ///
    /// # Errors
    ///
    /// Returns [`MapError::Misaligned`] if `vaddr` is not 4 KiB-aligned,
    /// [`MapError::NotMapped`] if `vaddr` has no live mapping at the level
    /// being split (nothing to shatter), or [`MapError::PoolExhausted`] if
    /// the page-table pool cannot supply a replacement table. On
    /// [`MapError::PoolExhausted`] any level already split stays split
    /// (still a faithful identity re-expression of the same translation),
    /// so the address space is never left describing a *different*
    /// mapping (`AGENTS.md` §2.9 — fail closed, never corrupt).
    pub fn split_block(&mut self, vaddr: u64) -> Result<(), MapError> {
        if (vaddr & (PAGE_SIZE as u64 - 1)) != 0 {
            return Err(MapError::Misaligned);
        }
        let frames = self.frames;
        let i1 = table_index(vaddr, 1);

        // --- Level 1: a 1 GiB block becomes a table of 512 × 2 MiB blocks.
        let e1 = self.root[i1];
        if (e1 & attrs::VALID) == 0 {
            return Err(MapError::NotMapped);
        }
        if is_block(e1) {
            let TableFrame { phys, entries } =
                frames.alloc_table().ok_or(MapError::PoolExhausted)?;
            // 2 MiB sub-entries (shift 21) are still *blocks*, not pages.
            shatter_block_into(entries, e1, 21, false);
            self.root[i1] = table_descriptor(phys);
        }

        // L1 now holds a table descriptor; recover the L2 table it points at.
        // SAFETY: the entry is a present, non-block table descriptor (just
        // installed above, or pre-existing); its output address is an
        // identity-mapped table page (the round-trip `ensure_child` relies
        // on), and `&mut self` makes the borrow exclusive.
        let l2 =
            unsafe { &mut *(phys_from_descriptor(self.root[i1]) as *mut [u64; ENTRIES_PER_TABLE]) };
        let i2 = table_index(vaddr, 2);
        let e2 = l2[i2];
        if (e2 & attrs::VALID) == 0 {
            return Err(MapError::NotMapped);
        }
        if is_block(e2) {
            let TableFrame { phys, entries } =
                frames.alloc_table().ok_or(MapError::PoolExhausted)?;
            // 4 KiB sub-entries (shift 12) are L3 *pages* (`TABLE_OR_PAGE`).
            shatter_block_into(entries, e2, 12, true);
            l2[i2] = table_descriptor(phys);
        }
        // L2 now resolves `vaddr` through a 4 KiB page leaf.
        Ok(())
    }

    /// Re-express every coarse block covering the arena
    /// `[base, base + len)` at 4 KiB granularity, so any single page in
    /// the arena can later be unmapped (e.g. a kthread kernel-stack guard
    /// page) without shattering the block the running CPU executes on
    /// (`plans/PI.md` guard-page fault-form, stage G2).
    ///
    /// This is [`Self::split_block`] applied to every 2 MiB block the
    /// arena spans: a guard-page arena that the boot path laid down inside
    /// the coarse identity gigapages has no per-4 KiB leaf to clear, so the
    /// whole arena is split up-front, at boot, while it holds no running
    /// context. Because `split_block` only ever *adds* table levels that
    /// reproduce the existing translation, preparing the arena changes no
    /// address's mapping and needs no TLB maintenance — it is safe against
    /// the active translation regime and is idempotent (a re-prepare of an
    /// already-fine arena allocates nothing).
    ///
    /// `base` and `len` are taken in bytes; `base` must be 4 KiB-aligned
    /// (the arena is laid out 2 MiB-aligned, which satisfies this). The
    /// arena is walked from the 2 MiB block containing `base` through the
    /// block containing its last byte, so an arena that is not itself a
    /// whole number of 2 MiB blocks still has every covering block split.
    ///
    /// # Errors
    ///
    /// Returns [`MapError::Misaligned`] if `len` is zero or `base` is not
    /// 4 KiB-aligned, [`MapError::NotMapped`] if any covering block has no
    /// live mapping, or [`MapError::PoolExhausted`] if the page-table pool
    /// cannot supply a replacement table. On a mid-arena failure the
    /// blocks already split stay split (a faithful re-expression of the
    /// same translation), so the space never describes a *different*
    /// mapping (`AGENTS.md` §2.9 — fail closed, never corrupt).
    pub fn prepare_guard_arena(&mut self, base: u64, len: u64) -> Result<(), MapError> {
        if len == 0 || (base & (PAGE_SIZE as u64 - 1)) != 0 {
            return Err(MapError::Misaligned);
        }
        // The last byte the arena occupies; `len != 0`, so `base + len`
        // does not underflow when computing it. A `base + len` that wraps
        // `u64` is rejected as a fail-closed `Misaligned` (a degenerate
        // arena), never silently truncated (`AGENTS.md` §2.9).
        let last = base.checked_add(len - 1).ok_or(MapError::Misaligned)?;
        let first_block = base & !(BLOCK_2MIB - 1);
        let last_block = last & !(BLOCK_2MIB - 1);
        let mut block = first_block;
        loop {
            self.split_block(block)?;
            if block == last_block {
                break;
            }
            block += BLOCK_2MIB;
        }
        Ok(())
    }

    /// Physical address of the L1 root table (the value programmed into
    /// `TTBR0_EL1`). Exposed so tests can observe it.
    #[must_use]
    pub fn root_phys(&self) -> u64 {
        self.root_phys
    }

    /// Translate the architecture-neutral [`PageFlags`] into a stage-1
    /// page-leaf attribute word (`AGENTS.md` §2.2 — one neutral
    /// vocabulary, decoded once at the HAL boundary). W^X is the default
    /// (`AGENTS.md` §19.2): an executable user page is mapped read-only
    /// ([`el0_code_leaf_attrs`]); a writable user page is execute-never
    /// ([`el0_data_leaf_attrs`]); a read-only user page is execute-never
    /// ([`el0_rodata_leaf_attrs`]). A kernel page uses the EL1 RW,
    /// EL0-execute-never [`normal_leaf_attrs`]; a Device page uses
    /// [`device_leaf_attrs`].
    fn leaf_attrs_for(flags: PageFlags) -> u64 {
        if flags.contains(PageFlags::DEVICE) {
            device_leaf_attrs(false)
        } else if flags.contains(PageFlags::USER) {
            if flags.contains(PageFlags::EXEC) {
                el0_code_leaf_attrs()
            } else if flags.contains(PageFlags::WRITE) {
                el0_data_leaf_attrs()
            } else {
                el0_rodata_leaf_attrs()
            }
        } else {
            normal_leaf_attrs(false)
        }
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

impl MmuAddressSpace for AddressSpace {
    fn map_page(&mut self, vaddr: u64, paddr: u64, flags: PageFlags) -> Result<(), MapError> {
        if (vaddr & (PAGE_SIZE as u64 - 1)) != 0 || (paddr & (PAGE_SIZE as u64 - 1)) != 0 {
            return Err(MapError::Misaligned);
        }
        if flags.is_write_exec() {
            return Err(MapError::InvalidFlags);
        }
        if self.leaf_present(vaddr) {
            return Err(MapError::AlreadyMapped);
        }
        let frames = self.frames;
        // Alignment and prior-mapping are already ruled out, so the only
        // remaining failure from the walk is frame-source exhaustion.
        self.map_4k_with_attrs(frames, vaddr, paddr, Self::leaf_attrs_for(flags))
            .ok_or(MapError::PoolExhausted)
    }

    fn translate(&self, vaddr: u64) -> Option<(u64, PageFlags)> {
        let e1 = self.root[table_index(vaddr, 1)];
        if (e1 & attrs::VALID) == 0 {
            return None;
        }
        if is_block(e1) {
            return Some((
                resolved_page(phys_from_descriptor(e1), vaddr, 30),
                page_flags_from_leaf(e1),
            ));
        }
        // SAFETY: a present table descriptor holds an output address
        // `ensure_child` wrote from `phys_of(&[u64; 512])`; identity
        // mapping makes that physical address directly dereferenceable
        // (the same round-trip `leaf_present` relies on).
        let l2 = unsafe { &*(phys_from_descriptor(e1) as *const [u64; ENTRIES_PER_TABLE]) };
        let e2 = l2[table_index(vaddr, 2)];
        if (e2 & attrs::VALID) == 0 {
            return None;
        }
        if is_block(e2) {
            return Some((
                resolved_page(phys_from_descriptor(e2), vaddr, 21),
                page_flags_from_leaf(e2),
            ));
        }
        // SAFETY: as above — a present L2 table descriptor's output
        // address is a valid identity-mapped table.
        let l3 = unsafe { &*(phys_from_descriptor(e2) as *const [u64; ENTRIES_PER_TABLE]) };
        let e3 = l3[table_index(vaddr, 3)];
        if (e3 & attrs::VALID) == 0 {
            return None;
        }
        Some((phys_from_descriptor(e3), page_flags_from_leaf(e3)))
    }

    fn unmap(&mut self, vaddr: u64) -> Result<u64, MapError> {
        if (vaddr & (PAGE_SIZE as u64 - 1)) != 0 {
            return Err(MapError::Misaligned);
        }
        // Navigate to the 4 KiB page leaf without allocating. A missing
        // level or a block leaf encountered on the way means there is no
        // 4 KiB leaf to tear down here — fail closed (the per-page unmap
        // path never shatters a block).
        let e1 = self.root[table_index(vaddr, 1)];
        if (e1 & attrs::VALID) == 0 || is_block(e1) {
            return Err(MapError::NotMapped);
        }
        // SAFETY: a present table descriptor's output address is an
        // identity-mapped table (see `translate`); `&mut self` makes the
        // exclusive borrow sound.
        let l2 = unsafe { &mut *(phys_from_descriptor(e1) as *mut [u64; ENTRIES_PER_TABLE]) };
        let e2 = l2[table_index(vaddr, 2)];
        if (e2 & attrs::VALID) == 0 || is_block(e2) {
            return Err(MapError::NotMapped);
        }
        // SAFETY: as above — a present L2 table descriptor's output
        // address is a valid identity-mapped table.
        let l3 = unsafe { &mut *(phys_from_descriptor(e2) as *mut [u64; ENTRIES_PER_TABLE]) };
        let i3 = table_index(vaddr, 3);
        let e3 = l3[i3];
        if (e3 & attrs::VALID) == 0 {
            return Err(MapError::NotMapped);
        }
        let paddr = phys_from_descriptor(e3);
        l3[i3] = 0;
        Ok(paddr)
    }

    fn root_phys(&self) -> u64 {
        self.root_phys
    }

    fn block_split_support(&self) -> BlockSplit {
        // aarch64 re-expresses a 1 GiB / 2 MiB block as a table of finer
        // leaves (`plans/PI.md` G1/G2 — the guard-page fault-form
        // foundation, host- and `-M virt`-proven).
        BlockSplit::Supported
    }

    fn split_block(&mut self, vaddr: u64) -> Result<(), MapError> {
        // The HAL view of the inherent, fully-tested `AddressSpace::split_block`
        // (G1): one body, reached either directly by the arch boot path /
        // verticals or through the HAL trait here (`AGENTS.md` §2.2).
        // Method-call syntax resolves to the inherent method (inherent methods
        // take precedence over a same-named trait method), so this forwards to
        // the inherent body rather than recursing into itself.
        self.split_block(vaddr)
    }

    fn prepare_guard_arena(&mut self, base: u64, len: u64) -> Result<(), MapError> {
        // The HAL view of the inherent, fully-tested
        // `AddressSpace::prepare_guard_arena` (G2): one body, reached either
        // directly by the arch boot path / verticals or through the HAL trait
        // here (`AGENTS.md` §2.2). As with `split_block`, inherent-method
        // resolution forwards to the inherent body rather than recursing.
        self.prepare_guard_arena(base, len)
    }

    unsafe fn activate(&self) {
        #[cfg(all(target_arch = "aarch64", target_os = "none"))]
        {
            // SAFETY: forwards to the gated stage-1 enable primitive; the
            // caller upholds the `MmuAddressSpace::activate` contract (this
            // space maps the current `pc`/`sp`/MMIO), which is exactly
            // `AddressSpace::switch`'s contract.
            unsafe { self.switch() };
        }
        #[cfg(not(all(target_arch = "aarch64", target_os = "none")))]
        {
            unreachable!("stage-1 activation is only meaningful on the aarch64 bare-metal target")
        }
    }
}

impl TlbShootdown for AddressSpace {
    fn flush_page(&mut self, vaddr: u64) {
        invalidate_page_inner_shareable(vaddr);
    }
}

/// Invalidate, on every PE in the inner-shareable domain, the stage-1
/// EL1&0 TLB entries for the 4 KiB page containing `vaddr` (all ASIDs).
///
/// This is the single instruction sequence shared by both the *local*
/// per-page flush ([`TlbShootdown::flush_page`]) and the *cross-CPU*
/// shootdown ([`rustos_arch_api::CrossCpuTlbShootdown::shootdown_page`] on
/// [`crate::kernel_arch::Aarch64Arch`]): `tlbi vaae1is` is the
/// inner-shareable *broadcast* variant, so the "local" and "cross-CPU"
/// shootdowns are literally the same operation on aarch64 — there is one
/// implementation, not two (`AGENTS.md` §2.2).
pub(crate) fn invalidate_page_inner_shareable(vaddr: u64) {
    #[cfg(all(target_arch = "aarch64", target_os = "none"))]
    {
        // SAFETY: `tlbi vaae1is` invalidates the inner-shareable TLB
        // entries for the page named by its operand (VA[55:12], all
        // ASIDs); the `dsb`/`isb` barriers order the invalidation and
        // make it visible before the next translation. It touches no
        // memory and only discards a cached translation. No Rust
        // spelling exists.
        let va_page = vaddr >> 12;
        unsafe {
            core::arch::asm!(
                "dsb ishst",
                "tlbi vaae1is, {page}",
                "dsb ish",
                "isb",
                page = in(reg) va_page,
                options(nostack, preserves_flags),
            );
        }
    }
    #[cfg(not(all(target_arch = "aarch64", target_os = "none")))]
    {
        // The host has no TLB to invalidate; a flush is vacuous.
        let _ = vaddr;
    }
}

/// Reactivate `root_phys` as the active stage-1 EL1&0 translation root
/// (reprogram `TTBR0_EL1`) on a CPU whose MMU is already enabled.
///
/// This is the SP2b user-kthread `pre_resume` primitive (`plans/SPAWN.md`
/// SP2): immediately before the kernel `eret`s back into a user task's
/// EL0, that task's own page-table root must be installed so its
/// translations — and only its — are in force, keeping sibling processes
/// hardware-isolated (`AGENTS.md` §4). It takes only the `u64` root, so
/// the per-task hook that calls it captures a plain word and stays `Send`.
///
/// Unlike [`AddressSpace::switch`] this does **not** touch `MAIR_EL1` /
/// `TCR_EL1` / `SCTLR_EL1.M`: the MMU is already on with the boot
/// translation controls in force, and only the low (`TTBR0_EL1`)
/// translation regime changes between user spaces.
///
/// # Safety
///
/// The MMU must already be enabled, and the L1 table at `root_phys` must
/// map the currently-executing kernel `pc`, `sp`, and the MMIO the code
/// touches identically to the outgoing root — every RustOS user space
/// identity-maps the low kernel window, so this holds for any task root,
/// but a `root_phys` that does not faults the CPU on its next access.
#[cfg(all(target_arch = "aarch64", target_os = "none"))]
pub unsafe fn activate_user_root(root_phys: u64) {
    // SAFETY: writing `TTBR0_EL1` swaps the low translation regime; the
    // `dsb`/`tlbi vmalle1`/`dsb`/`isb` sequence flushes the stale EL1&0
    // entries and makes the new root in force before the next access. No
    // memory is touched and no Rust spelling exists for these system
    // registers. The caller's contract guarantees the new root covers the
    // running kernel context.
    unsafe {
        core::arch::asm!(
            "msr TTBR0_EL1, {ttbr}",
            "dsb ish",
            "tlbi vmalle1",
            "dsb ish",
            "isb",
            ttbr = in(reg) root_phys,
            options(nostack, preserves_flags),
        );
    }
}

/// Host substitute: reprogramming `TTBR0_EL1` is meaningful only on the
/// bare-metal aarch64 target. Never linked into a kernel image and never
/// reached on the host (the QEMU verticals exercise the real switch).
///
/// # Safety
///
/// Carries the same contract as the bare-metal definition above (MMU
/// enabled; `root_phys` maps the running kernel context), so the two
/// `cfg` arms present one `unsafe` API. The host body is inert.
#[cfg(not(all(target_arch = "aarch64", target_os = "none")))]
pub unsafe fn activate_user_root(root_phys: u64) {
    let _ = root_phys;
}

/// Populate the freshly-allocated table `child` with 512 descriptors that
/// reproduce the leaf `block` at the next finer granularity, preserving
/// every attribute bit.
///
/// `sub_shift` is the base-2 log of each sub-entry's coverage (21 for the
/// 2 MiB sub-blocks an L1 block shatters into, 12 for the 4 KiB pages an
/// L2 block shatters into); `page` selects an L3 *page* descriptor
/// (`TABLE_OR_PAGE` set) when shattering to 4 KiB, versus a finer *block*
/// (bit clear) at 2 MiB. Only the output address changes per sub-entry —
/// `block & !ADDR_MASK` captures `VALID` plus every lower (`[11:2]`) and
/// upper (`[63:48]`, incl. `PXN`/`UXN`) attribute bit, so the finer
/// descriptors map the same memory with identical permissions
/// (`AGENTS.md` §2.2 — one attribute vocabulary, never re-derived).
fn shatter_block_into(
    child: &mut [u64; ENTRIES_PER_TABLE],
    block: u64,
    sub_shift: u32,
    page: bool,
) {
    let base = phys_from_descriptor(block);
    let attr_bits = block & !ADDR_MASK;
    let sub_size = 1u64 << sub_shift;
    for (i, slot) in child.iter_mut().enumerate() {
        let sub_pa = base + (i as u64) * sub_size;
        let mut desc = (sub_pa & ADDR_MASK) | attr_bits;
        if page {
            desc |= attrs::TABLE_OR_PAGE;
        } else {
            desc &= !attrs::TABLE_OR_PAGE;
        }
        *slot = desc;
    }
}

// `&mut [u64; 512]` in, `&'static mut [u64; 512]` out: the returned
// reference points at a freshly-alloc'd table from `frames` or at a
// sibling recovered through the identity map, never a borrow of
// `parent` — exactly the shape `mut_from_ref` flags.
#[allow(clippy::mut_from_ref)]
fn ensure_child(
    parent: &mut [u64; ENTRIES_PER_TABLE],
    idx: usize,
    frames: &'static dyn PageTableFrames,
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
        // output address derived from a `TableFrame`, so the round-trip
        // is valid; identity mapping means the physical address is also
        // the address we dereference.
        let child: &'static mut [u64; ENTRIES_PER_TABLE] =
            unsafe { &mut *(phys as *mut [u64; ENTRIES_PER_TABLE]) };
        Some(child)
    } else {
        let TableFrame { phys, entries } = frames.alloc_table()?;
        parent[idx] = table_descriptor(phys);
        Some(entries)
    }
}

fn phys_of(table: &[u64; ENTRIES_PER_TABLE]) -> u64 {
    // Identity-mapped: virtual == physical for everything the kernel
    // owns, because the boot trampoline runs with the MMU off and the
    // gigapage identity map preserves it.
    table.as_ptr() as u64
}

/// Decode a stage-1 leaf (block or page) descriptor's attributes back
/// into the neutral [`PageFlags`] (the inverse of
/// [`AddressSpace::leaf_attrs_for`]). A valid leaf is always readable;
/// the AP field decides writability and EL0 reachability, the
/// execute-never bit for the leaf's privilege level decides
/// executability, and the `MAIR` attribute index decides Device.
fn page_flags_from_leaf(desc: u64) -> PageFlags {
    let mut out = PageFlags::READ;
    let ap = desc & (0b11 << 6);
    let user = ap == attrs::AP_RW_EL0 || ap == attrs::AP_RO_EL0;
    if ap == attrs::AP_RW_EL1 || ap == attrs::AP_RW_EL0 {
        out = out | PageFlags::WRITE;
    }
    if user {
        out = out | PageFlags::USER;
        if desc & attrs::UXN == 0 {
            out = out | PageFlags::EXEC;
        }
    } else if desc & attrs::PXN == 0 {
        out = out | PageFlags::EXEC;
    }
    if desc & attrs::ATTR_IDX_DEVICE != 0 {
        out = out | PageFlags::DEVICE;
    }
    out
}

/// 4 KiB-aligned physical address `vaddr` resolves to under a leaf whose
/// region starts at `leaf_base` and spans `1 << region_shift` bytes
/// (30 = L1 block, 21 = L2 block, 12 = L3 page). The page offset is
/// dropped so the result is page-aligned (the HAL `translate` contract
/// reports the 4 KiB page base).
fn resolved_page(leaf_base: u64, vaddr: u64, region_shift: u32) -> u64 {
    let region_mask = (1u64 << region_shift) - 1;
    (leaf_base + (vaddr & region_mask)) & !((PAGE_SIZE as u64) - 1)
}

#[cfg(test)]
#[path = "paging_tests.rs"]
mod tests;
