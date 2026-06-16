//! Sv39 page-table primitives for the riscv64 port.
//!
//! This module is the riscv64 analogue of `kernel/arch/x86_64::paging`.
//! It implements the Arch HAL page-table surface
//! ([`rustos_arch_api::mmu::AddressSpace`] +
//! [`rustos_arch_api::tlb::TlbShootdown`]) `kernel/mem` drives, and it
//! supplies the inherent [`AddressSpace::new_identity_gigapages`] /
//! `AddressSpace::switch` the production boot pipeline
//! (`rustos_kernel::riscv64::boot`, `plans/PI.md` RV-P2) uses to enable
//! the Sv39 identity MMU. The same primitives back the memory-isolation
//! QEMU vertical's two Sv39 hierarchies that disagree about a single
//! virtual address, so the MMU faults a process that reaches for
//! another's frame (`AGENTS.md` §4, "memory isolation is enforced by
//! hardware").
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

use rustos_arch_api::frames::{PageTableFrames, TableFrame};
use rustos_arch_api::mmu::{AddressSpace as MmuAddressSpace, BlockSplit, MapError, PageFlags};
use rustos_arch_api::tlb::TlbShootdown;

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

/// Bytes spanned by one Sv39 megapage (level-1) leaf — the coarse block
/// granularity [`AddressSpace::prepare_guard_arena`] walks (the
/// guard-page fault-form, `plans/PI.md` G2).
pub const BLOCK_2MIB: u64 = 2 * 1024 * 1024;

/// Mask of the PPN field within an Sv39 PTE (bits `[53:10]`). The
/// complement captures `VALID` plus every permission/attribute bit, so
/// [`shatter_pte_into`] can replace only the output address when
/// re-expressing a coarse leaf at finer granularity.
const PTE_PPN_MASK: u64 = 0x0FFF_FFFF_FFFF << 10;

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

impl PageTableFrames for PageTablePool {
    fn alloc_table(&self) -> Option<TableFrame> {
        let entries = self.alloc()?;
        // Sv39 runs identity-mapped for the kernel's own memory, so the
        // table's virtual address is its physical address (`AGENTS.md`
        // §17.2 / `plans/WIRING.md` W5b-3 — the bootstrap frame source).
        let phys = phys_of(entries);
        Some(TableFrame { phys, entries })
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
        Some(Self {
            root_phys,
            root,
            frames,
        })
    }

    /// `true` if `vaddr` already resolves to a leaf in this hierarchy.
    ///
    /// A read-only Sv39 walk used by the [`rustos_arch_api::mmu::AddressSpace`]
    /// HAL impl to report [`rustos_arch_api::mmu::MapError::AlreadyMapped`]
    /// rather than silently clobber an existing mapping. The walk
    /// dereferences present non-leaf entries through the identity map
    /// (phys == virt for every table the kernel owns), the same
    /// round-trip [`ensure_child`] relies on.
    fn leaf_present(&self, vaddr: u64) -> bool {
        let e2 = self.root[vpn_index(vaddr, 2)];
        if (e2 & flags::VALID) == 0 {
            return false;
        }
        if pte_is_leaf(e2) {
            return true;
        }
        // SAFETY: a present non-leaf entry holds a PPN `ensure_child`
        // wrote from `phys_of(&mut [u64; 512])`; identity mapping makes
        // the physical address dereferenceable directly.
        let l1 = unsafe { &*(phys_from_pte(e2) as *const [u64; ENTRIES_PER_TABLE]) };
        let e1 = l1[vpn_index(vaddr, 1)];
        if (e1 & flags::VALID) == 0 {
            return false;
        }
        if pte_is_leaf(e1) {
            return true;
        }
        // SAFETY: as above — a present non-leaf L1 entry's PPN is a valid
        // identity-mapped table address.
        let l0 = unsafe { &*(phys_from_pte(e1) as *const [u64; ENTRIES_PER_TABLE]) };
        (l0[vpn_index(vaddr, 0)] & flags::VALID) != 0
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
        frames: &'static dyn PageTableFrames,
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

        let l1 = ensure_child(self.root, i2, frames)?;
        let l0 = ensure_child(l1, i1, frames)?;
        if pte_is_leaf(l0[i0]) {
            return None;
        }
        l0[i0] = pte_from_phys(paddr, flags | flags::VALID | flags::ACCESSED | flags::DIRTY);
        Some(())
    }

    /// Re-express the coarse leaf(s) covering `vaddr` at 4 KiB
    /// granularity, preserving the mapped output address and every
    /// permission bit, so the single 4 KiB page containing `vaddr` can
    /// then be torn down with [`MmuAddressSpace::unmap`] (+ a
    /// [`TlbShootdown::flush_page`]) without disturbing its neighbours.
    ///
    /// This is the foundation of the riscv64 kthread guard page
    /// (`plans/PI.md` G1, the sibling of the aarch64 block split): a guard
    /// page that falls inside a region the boot path mapped with a coarse
    /// 1 GiB gigapage / 2 MiB megapage *leaf* cannot be unmapped while it
    /// is part of that leaf, because the leaf has no per-4 KiB entry to
    /// clear. Splitting re-expresses the same translation as a table of
    /// finer leaves — a gigapage (level 2) becomes a table of 512 × 2 MiB
    /// megapages, then the megapage (level 1) covering `vaddr` becomes a
    /// table of 512 × 4 KiB pages — leaving every address translating
    /// identically but now at 4 KiB granularity.
    ///
    /// The split is **break-before-make-free for the running region**: it
    /// only ever *adds* table levels that reproduce the existing
    /// translation, never invalidating a live address, so it is safe to
    /// run against the active translation regime. It is idempotent — a
    /// level that is already a table pointer is left untouched — so
    /// re-splitting an already-fine region succeeds without allocating.
    /// The split itself changes no translation result and so needs no TLB
    /// maintenance; the caller flushes after a subsequent
    /// [`MmuAddressSpace::unmap`].
    ///
    /// # Errors
    ///
    /// Returns [`MapError::Misaligned`] if `vaddr` is not 4 KiB-aligned,
    /// [`MapError::NotMapped`] if `vaddr` has no live mapping at the level
    /// being split, or [`MapError::PoolExhausted`] if the page-table pool
    /// cannot supply a replacement table. On [`MapError::PoolExhausted`]
    /// any level already split stays split (still a faithful identity
    /// re-expression of the same translation), so the address space is
    /// never left describing a *different* mapping (`AGENTS.md` §2.9).
    pub fn split_block(&mut self, vaddr: u64) -> Result<(), MapError> {
        if (vaddr & (PAGE_SIZE as u64 - 1)) != 0 {
            return Err(MapError::Misaligned);
        }
        let frames = self.frames;
        let i2 = vpn_index(vaddr, 2);

        // --- Level 2: a 1 GiB gigapage leaf becomes a table of 512 × 2 MiB
        // megapage leaves.
        let e2 = self.root[i2];
        if (e2 & flags::VALID) == 0 {
            return Err(MapError::NotMapped);
        }
        if pte_is_leaf(e2) {
            let TableFrame { phys, entries } =
                frames.alloc_table().ok_or(MapError::PoolExhausted)?;
            // 2 MiB sub-entries (shift 21) are still *leaves*, one finer level.
            shatter_pte_into(entries, e2, 21);
            self.root[i2] = pte_from_phys(phys, flags::VALID);
        }

        // The root slot now holds a table pointer; recover the L1 table.
        // SAFETY: the entry is a present, non-leaf table pointer (just
        // installed above, or pre-existing); its PPN is an identity-mapped
        // table page (the round-trip `ensure_child`/`translate` rely on),
        // and `&mut self` makes the borrow exclusive.
        let l1 = unsafe { &mut *(phys_from_pte(self.root[i2]) as *mut [u64; ENTRIES_PER_TABLE]) };
        let i1 = vpn_index(vaddr, 1);
        let e1 = l1[i1];
        if (e1 & flags::VALID) == 0 {
            return Err(MapError::NotMapped);
        }
        if pte_is_leaf(e1) {
            let TableFrame { phys, entries } =
                frames.alloc_table().ok_or(MapError::PoolExhausted)?;
            // 4 KiB sub-entries (shift 12) are level-0 page leaves.
            shatter_pte_into(entries, e1, 12);
            l1[i1] = pte_from_phys(phys, flags::VALID);
        }
        // L1 now resolves `vaddr` through a 4 KiB page leaf.
        Ok(())
    }

    /// Re-express every coarse leaf covering the arena
    /// `[base, base + len)` at 4 KiB granularity, so any single page in
    /// the arena (e.g. a kthread kernel-stack guard page) can later be
    /// torn down with [`MmuAddressSpace::unmap`] (+ a
    /// [`TlbShootdown::flush_page`]) without disturbing the block the
    /// running hart executes on (`plans/PI.md` guard-page fault-form,
    /// stage G2).
    ///
    /// This is [`Self::split_block`] applied to every 2 MiB block the
    /// arena spans: a guard-page arena that the boot path laid down inside
    /// the coarse identity gigapages has no per-4 KiB leaf to clear, so the
    /// whole arena is split up-front, at boot, while it holds no running
    /// context. Because `split_block` only ever *adds* table levels that
    /// reproduce the existing translation, preparing the arena changes no
    /// address's mapping and needs no TLB maintenance — it is safe against
    /// the active translation regime and is idempotent.
    ///
    /// `base` and `len` are taken in bytes; `base` must be 4 KiB-aligned
    /// (the arena is laid out 2 MiB-aligned, which satisfies this). The
    /// arena is walked from the 2 MiB block containing `base` through the
    /// block containing its last byte.
    ///
    /// # Errors
    ///
    /// Returns [`MapError::Misaligned`] if `len` is zero, `base` is not
    /// 4 KiB-aligned, or `base + len` wraps; [`MapError::NotMapped`] if any
    /// covering block has no live mapping; or [`MapError::PoolExhausted`]
    /// if the page-table pool cannot supply a replacement table. On a
    /// mid-arena failure the blocks already split stay split (a faithful
    /// re-expression of the same translation), so the space never describes
    /// a *different* mapping (`AGENTS.md` §2.9 — fail closed, never
    /// corrupt).
    pub fn prepare_guard_arena(&mut self, base: u64, len: u64) -> Result<(), MapError> {
        if len == 0 || (base & (PAGE_SIZE as u64 - 1)) != 0 {
            return Err(MapError::Misaligned);
        }
        // The last byte the arena occupies; `len != 0`, so `len - 1` does
        // not underflow. A `base + len` that wraps `u64` is rejected as a
        // fail-closed `Misaligned`, never silently truncated.
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

    /// Map the 1 GiB region at `paddr` to `vaddr` with a single root-level
    /// gigapage leaf.
    ///
    /// `vaddr` and `paddr` must be 1 GiB-aligned. This installs one leaf
    /// directly in the root table (no child tables), so it costs no pool
    /// frames — the cheap way to alias a whole gigabyte of physical memory at
    /// a high virtual address with different permissions (e.g. the `USER`
    /// bit) than the identity map carries. Returns `None` on a misaligned
    /// address or if the target root slot is already occupied (a leaf or a
    /// table pointer) — it refuses to overwrite an existing mapping rather
    /// than silently clobber it.
    ///
    /// **TEST-ONLY SCAFFOLDING.** This exists solely to let the (in-progress)
    /// crt0 QEMU round-trip vertical alias the kernel's RAM at a high `BIAS`
    /// with the `USER` bit so it can `sret` into U-mode. It is gated to test
    /// builds and the `test-harness` feature so it is **never** compiled into
    /// a production kernel image; remove the gate (and this note) only when a
    /// real U-mode loader in `kernel/mem` makes it a supported primitive.
    #[cfg(any(test, feature = "test-harness"))]
    pub fn map_gigapage(&mut self, vaddr: u64, paddr: u64, flags: u64) -> Option<()> {
        const GIB: u64 = 1 << 30;
        if (vaddr & (GIB - 1)) != 0 || (paddr & (GIB - 1)) != 0 {
            return None;
        }
        let i2 = vpn_index(vaddr, 2);
        if (self.root[i2] & flags::VALID) != 0 {
            return None;
        }
        self.root[i2] = pte_from_phys(paddr, flags | flags::VALID | flags::ACCESSED | flags::DIRTY);
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

/// Translate the architecture-neutral [`PageFlags`] into the Sv39
/// permission bits (`AGENTS.md` §2.2 — one neutral vocabulary, decoded
/// once at the HAL boundary). The `VALID`/`ACCESSED`/`DIRTY` bits are
/// added by [`AddressSpace::map_4k`]; riscv64 has no page-table Device
/// attribute (memory type is PMA-driven), so [`PageFlags::DEVICE`] only
/// affects the absent caching attribute and maps to the same R/W/X here.
fn sv39_flags(flags: PageFlags) -> u64 {
    let mut bits = 0;
    if flags.contains(PageFlags::READ) {
        bits |= flags::READ;
    }
    if flags.contains(PageFlags::WRITE) {
        bits |= flags::WRITE;
    }
    if flags.contains(PageFlags::EXEC) {
        bits |= flags::EXEC;
    }
    if flags.contains(PageFlags::USER) {
        bits |= flags::USER;
    }
    bits
}

/// Decode an Sv39 leaf PTE's permission bits back into the neutral
/// [`PageFlags`] (the inverse of [`sv39_flags`]). riscv64 has no
/// page-table Device attribute, so [`PageFlags::DEVICE`] is not
/// recoverable from a leaf and is never reported.
fn page_flags_from_sv39(pte: u64) -> PageFlags {
    let mut out = PageFlags::empty();
    if pte & flags::READ != 0 {
        out = out | PageFlags::READ;
    }
    if pte & flags::WRITE != 0 {
        out = out | PageFlags::WRITE;
    }
    if pte & flags::EXEC != 0 {
        out = out | PageFlags::EXEC;
    }
    if pte & flags::USER != 0 {
        out = out | PageFlags::USER;
    }
    out
}

/// 4 KiB-aligned physical address `vaddr` resolves to under a leaf whose
/// region starts at `leaf_base` and spans `1 << region_shift` bytes
/// (30 = gigapage, 21 = megapage, 12 = 4 KiB). The page offset is
/// dropped so the result is always page-aligned (the HAL `translate`
/// contract reports the 4 KiB page base).
fn resolved_page(leaf_base: u64, vaddr: u64, region_shift: u32) -> u64 {
    let region_mask = (1u64 << region_shift) - 1;
    (leaf_base + (vaddr & region_mask)) & !((PAGE_SIZE as u64) - 1)
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
        self.map_4k(frames, vaddr, paddr, sv39_flags(flags))
            .ok_or(MapError::PoolExhausted)
    }

    fn translate(&self, vaddr: u64) -> Option<(u64, PageFlags)> {
        let e2 = self.root[vpn_index(vaddr, 2)];
        if (e2 & flags::VALID) == 0 {
            return None;
        }
        if pte_is_leaf(e2) {
            return Some((
                resolved_page(phys_from_pte(e2), vaddr, 30),
                page_flags_from_sv39(e2),
            ));
        }
        // SAFETY: a present non-leaf entry holds a PPN `ensure_child`
        // wrote from `phys_of(&[u64; 512])`; identity mapping makes that
        // physical address directly dereferenceable (the same round-trip
        // `leaf_present` relies on).
        let l1 = unsafe { &*(phys_from_pte(e2) as *const [u64; ENTRIES_PER_TABLE]) };
        let e1 = l1[vpn_index(vaddr, 1)];
        if (e1 & flags::VALID) == 0 {
            return None;
        }
        if pte_is_leaf(e1) {
            return Some((
                resolved_page(phys_from_pte(e1), vaddr, 21),
                page_flags_from_sv39(e1),
            ));
        }
        // SAFETY: as above — a present non-leaf L1 entry's PPN is a valid
        // identity-mapped table address.
        let l0 = unsafe { &*(phys_from_pte(e1) as *const [u64; ENTRIES_PER_TABLE]) };
        let e0 = l0[vpn_index(vaddr, 0)];
        if (e0 & flags::VALID) == 0 || !pte_is_leaf(e0) {
            return None;
        }
        Some((phys_from_pte(e0), page_flags_from_sv39(e0)))
    }

    fn unmap(&mut self, vaddr: u64) -> Result<u64, MapError> {
        if (vaddr & (PAGE_SIZE as u64 - 1)) != 0 {
            return Err(MapError::Misaligned);
        }
        // Navigate to the 4 KiB leaf without allocating. A missing level
        // or a large-page leaf encountered on the way means there is no
        // 4 KiB leaf to tear down here — fail closed (the per-page unmap
        // path never shatters a gigapage/megapage).
        let e2 = self.root[vpn_index(vaddr, 2)];
        if (e2 & flags::VALID) == 0 || pte_is_leaf(e2) {
            return Err(MapError::NotMapped);
        }
        // SAFETY: a present non-leaf entry's PPN is an identity-mapped
        // table address (see `translate`); `&mut self` makes the
        // exclusive borrow sound.
        let l1 = unsafe { &mut *(phys_from_pte(e2) as *mut [u64; ENTRIES_PER_TABLE]) };
        let e1 = l1[vpn_index(vaddr, 1)];
        if (e1 & flags::VALID) == 0 || pte_is_leaf(e1) {
            return Err(MapError::NotMapped);
        }
        // SAFETY: as above — a present non-leaf L1 entry's PPN is a valid
        // identity-mapped table address.
        let l0 = unsafe { &mut *(phys_from_pte(e1) as *mut [u64; ENTRIES_PER_TABLE]) };
        let i0 = vpn_index(vaddr, 0);
        let e0 = l0[i0];
        if (e0 & flags::VALID) == 0 || !pte_is_leaf(e0) {
            return Err(MapError::NotMapped);
        }
        let paddr = phys_from_pte(e0);
        l0[i0] = 0;
        Ok(paddr)
    }

    fn root_phys(&self) -> u64 {
        self.root_phys
    }

    fn block_split_support(&self) -> BlockSplit {
        // Sv39 re-expresses a 1 GiB gigapage / 2 MiB megapage leaf as a
        // table of finer leaves (`plans/PI.md` G1/G2 — the riscv64
        // guard-page fault-form foundation, host- and `-M virt`-proven),
        // the sibling of the aarch64 block split.
        BlockSplit::Supported
    }

    fn split_block(&mut self, vaddr: u64) -> Result<(), MapError> {
        // The HAL view of the inherent, fully-tested
        // `AddressSpace::split_block` (G1): one body, reached either
        // directly by the arch boot path / verticals or through the HAL
        // trait here (`AGENTS.md` §2.2). Inherent methods take precedence
        // over a same-named trait method, so this forwards to the inherent
        // body rather than recursing into itself.
        self.split_block(vaddr)
    }

    fn prepare_guard_arena(&mut self, base: u64, len: u64) -> Result<(), MapError> {
        // The HAL view of the inherent, fully-tested
        // `AddressSpace::prepare_guard_arena` (G2): one body, reached
        // either directly or through the HAL trait here (`AGENTS.md` §2.2).
        // As with `split_block`, inherent-method resolution forwards to the
        // inherent body rather than recursing.
        self.prepare_guard_arena(base, len)
    }

    unsafe fn activate(&self) {
        #[cfg(all(target_arch = "riscv64", target_os = "none"))]
        {
            // SAFETY: forwards to the gated `satp` activation primitive;
            // the caller upholds the `MmuAddressSpace::activate` contract
            // (this space maps the current `pc`/`sp`/MMIO), which is
            // exactly `AddressSpace::switch`'s contract.
            unsafe { self.switch() };
        }
        #[cfg(not(all(target_arch = "riscv64", target_os = "none")))]
        {
            unreachable!("Sv39 activation is only meaningful on the riscv64 bare-metal target")
        }
    }
}

impl TlbShootdown for AddressSpace {
    fn flush_page(&mut self, vaddr: u64) {
        invalidate_page_local(vaddr);
    }
}

/// Invalidate the *calling* hart's cached Sv39 translation for the 4 KiB
/// page containing `vaddr`.
///
/// This is the single instruction sequence shared by both the local
/// per-page flush ([`TlbShootdown::flush_page`]) and the local half of
/// the cross-CPU shootdown
/// ([`rustos_arch_api::CrossCpuTlbShootdown::shootdown_page`] on
/// [`crate::kernel_arch::RiscvArch`]) — one implementation, not two
/// (`AGENTS.md` §2.2). Unlike aarch64 there is no broadcast variant: the
/// cross-CPU path reaches *other* harts through the SBI RFENCE firmware
/// call (`crate::sbi::remote_sfence_vma`).
pub(crate) fn invalidate_page_local(vaddr: u64) {
    #[cfg(all(target_arch = "riscv64", target_os = "none"))]
    {
        // SAFETY: `sfence.vma {addr}, zero` is the documented Sv39
        // single-page TLB invalidation; it touches no memory and only
        // discards the cached translation for `vaddr`. No Rust
        // spelling exists.
        unsafe {
            core::arch::asm!(
                "sfence.vma {addr}, zero",
                addr = in(reg) vaddr,
                options(nostack, preserves_flags),
            );
        }
    }
    #[cfg(not(all(target_arch = "riscv64", target_os = "none")))]
    {
        // The host has no TLB to invalidate; a flush is vacuous.
        let _ = vaddr;
    }
}

/// Reactivate `root_phys` as the active Sv39 translation root (write
/// `satp`) on a hart whose paging is already on.
///
/// This is the RV-X1 user-kthread `pre_resume` primitive (`plans/PI.md`
/// §X), the riscv64 sibling of the `aarch64`/`x86_64` `activate_user_root`:
/// immediately before the kernel `sret`s back into a user task's U-mode,
/// that task's own page-table root must be installed so its translations —
/// and only its — are in force, keeping sibling processes hardware-isolated
/// (`AGENTS.md` §4). It takes only the `u64` root, so the per-task hook
/// that calls it captures a plain word and stays `Send`.
///
/// Unlike [`AddressSpace::switch`] this is a free function over a raw
/// `root_phys` rather than an owned [`AddressSpace`]: the per-task hook
/// holds only the captured root word, not the (`!Send`) space. The `satp`
/// write + `sfence.vma` sequence is identical — Sv39 has a single
/// translation regime, so reprogramming the root reprograms everything.
///
/// # Safety
///
/// Paging must already be enabled, and the root table at `root_phys` must
/// map the currently-executing kernel `pc`, `sp`, and the MMIO the code
/// touches identically to the outgoing root — every RustOS user space
/// identity-maps the low kernel window, so this holds for any task root,
/// but a `root_phys` that does not faults the hart on its next access.
#[cfg(all(target_arch = "riscv64", target_os = "none"))]
pub unsafe fn activate_user_root(root_phys: u64) {
    let satp = satp_sv39(root_phys);
    // SAFETY: writing `satp` swaps the Sv39 translation root; `sfence.vma`
    // (with both operands `x0`) flushes the stale entries so the new root
    // is in force before the next access. No memory is touched and no Rust
    // spelling exists for `satp`. The caller's contract guarantees the new
    // root covers the running kernel context.
    unsafe {
        core::arch::asm!(
            "csrw satp, {satp}",
            "sfence.vma",
            satp = in(reg) satp,
            options(nostack, preserves_flags),
        );
    }
}

/// Host substitute: reprogramming `satp` is meaningful only on the
/// bare-metal riscv64 target. Never linked into a kernel image and never
/// reached on the host (the QEMU verticals exercise the real switch).
///
/// # Safety
///
/// Carries the same contract as the bare-metal definition above (paging
/// enabled; `root_phys` maps the running kernel context), so the two `cfg`
/// arms present one `unsafe` API. The host body is inert.
#[cfg(not(all(target_arch = "riscv64", target_os = "none")))]
pub unsafe fn activate_user_root(root_phys: u64) {
    let _ = root_phys;
}

/// Populate the freshly-allocated table `child` with 512 PTEs that
/// reproduce the leaf `block` at the next finer granularity, preserving
/// every permission/attribute bit.
///
/// `sub_shift` is the base-2 log of each sub-entry's coverage (21 for the
/// 2 MiB megapages a gigapage shatters into, 12 for the 4 KiB pages a
/// megapage shatters into). Sv39 leaves carry the same R/W/X/U/A/D
/// encoding at every level, so only the PPN changes per sub-entry —
/// `block & !PTE_PPN_MASK` captures `VALID` plus every permission bit, so
/// the finer leaves map the same memory with identical permissions
/// (`AGENTS.md` §2.2 — one attribute vocabulary, never re-derived).
fn shatter_pte_into(child: &mut [u64; ENTRIES_PER_TABLE], block: u64, sub_shift: u32) {
    let base = phys_from_pte(block);
    let attr_bits = block & !PTE_PPN_MASK;
    let sub_size = 1u64 << sub_shift;
    for (i, slot) in child.iter_mut().enumerate() {
        let sub_pa = base + (i as u64) * sub_size;
        *slot = pte_from_phys(sub_pa, attr_bits);
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
    if (entry & flags::VALID) != 0 {
        if pte_is_leaf(entry) {
            // A leaf where we expected a table pointer: refuse rather
            // than shatter a large page silently.
            return None;
        }
        let phys = phys_from_pte(entry);
        // SAFETY: every non-leaf valid entry was inserted below with a
        // PPN derived from a `TableFrame`, so the round-trip is valid;
        // identity mapping means the physical address is also the address
        // we dereference.
        let child: &'static mut [u64; ENTRIES_PER_TABLE] =
            unsafe { &mut *(phys as *mut [u64; ENTRIES_PER_TABLE]) };
        Some(child)
    } else {
        let TableFrame { phys, entries } = frames.alloc_table()?;
        // Non-leaf (table pointer): valid set, R/W/X clear.
        parent[idx] = pte_from_phys(phys, flags::VALID);
        Some(entries)
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
