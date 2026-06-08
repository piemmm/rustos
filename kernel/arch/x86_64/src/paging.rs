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
//! It implements the Arch HAL page-table surface
//! ([`rustos_arch_api::mmu::AddressSpace`] +
//! [`rustos_arch_api::tlb::TlbShootdown`]) `kernel/mem` drives. The
//! page-table *walk* (`map_page` / `translate` / `unmap`) recovers
//! intermediate tables through the low identity map and so is only valid
//! on the bare-metal target; like [`AddressSpace::activate`] it is proven
//! by the `memory_isolation` QEMU vertical, not a host conformance test
//! (the host build of those methods is `unreachable!`). The bit math is
//! a strict subset so promotion does not require interface creep
//! (`AGENTS.md` §2.4).

use core::cell::UnsafeCell;
use core::sync::atomic::{AtomicUsize, Ordering};

use rustos_arch_api::frames::{PageTableFrames, TableFrame};
use rustos_arch_api::mmu::{AddressSpace as MmuAddressSpace, BlockSplit, MapError, PageFlags};
use rustos_arch_api::tlb::TlbShootdown;

/// Size of a single x86_64 page-table page.
pub const PAGE_SIZE: usize = 4096;

/// Number of 64-bit entries in a page-table page (PML4 / PDPT / PD / PT).
pub const ENTRIES_PER_TABLE: usize = 512;

/// Base virtual address of the -2 GiB higher-half kernel window.
///
/// A kernel symbol linked at `KERNEL_VMA_BASE + p` is loaded at physical
/// `p` (`kernel/arch/x86_64/linker.ld`; `boot.s` SAFETY-INVARIANT 9). Used
/// to turn a higher-half kernel virtual address back into the physical
/// address the MMU needs in a page-table entry or CR3. Must equal the
/// `KERNEL_VMA_BASE` in `linker.ld` and the literal in `boot.s`.
pub const KERNEL_VMA_BASE: u64 = 0xFFFF_FFFF_8000_0000;

/// Page-table entry flags actually used here.
pub mod flags {
    /// Entry is present.
    pub const PRESENT: u64 = 1 << 0;
    /// Writable.
    pub const WRITABLE: u64 = 1 << 1;
    /// User-accessible (CPL 3 may reach the page). Must be set on the
    /// leaf **and** on every intermediate entry on the walk, otherwise
    /// the CPU denies the ring-3 access (Intel SDM Vol 3A §4.6).
    pub const USER: u64 = 1 << 2;
    /// Page Size (1 for huge pages at PD or PDPT level).
    pub const HUGE: u64 = 1 << 7;
    /// No-Execute (bit 63): an instruction fetch from the page faults.
    /// Honoured only while `IA32_EFER.NXE` is set; with NXE clear the bit
    /// is reserved and would fault the walk, so callers that set it must
    /// have enabled NXE first. Used to mark writable user data and
    /// read-only user data non-executable (W^X, `AGENTS.md` §19.2).
    pub const NO_EXECUTE: u64 = 1 << 63;
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
/// two [`AddressSpace`]s, each carrying the low 32 MiB identity map
/// (PML4 + PDPT + PD), the higher-half kernel window (PDPT + PD), and up
/// to one extra fine-grained mapping (PDPT + PD + PT): 2 × 8 + spares.
const POOL_SIZE: usize = 24;

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

impl PageTableFrames for PageTablePool {
    fn alloc_table(&self) -> Option<TableFrame> {
        let entries = self.alloc()?;
        // The static pool is a higher-half kernel image; `phys_of`
        // recovers the physical address the MMU needs (`AGENTS.md`
        // §17.2 / `plans/WIRING.md` W5b-3 — the bootstrap frame source).
        let phys = phys_of(entries);
        Some(TableFrame { phys, entries })
    }
}

/// An address space built on a freshly-allocated PML4.
///
/// The constructor identity-maps the first 32 MiB with 2 MiB huge pages
/// (so low physical memory, including the boot stack, stays reachable)
/// **and** mirrors the boot trampoline's higher-half kernel window
/// (`boot.s` SAFETY-INVARIANT 9) so the higher-half-linked kernel
/// code/stack/data remain reachable regardless of which `AddressSpace`
/// is currently active. The [`Self::map_4k`] method adds finer-grained
/// mappings used by the memory-isolation test.
pub struct AddressSpace {
    pml4_phys: u64,
    pml4: &'static mut [u64; ENTRIES_PER_TABLE],
    /// The frame source the page-table walk allocates intermediate
    /// tables from, retained so the [`rustos_arch_api::mmu::AddressSpace`]
    /// HAL impl can install mappings without the caller re-supplying it.
    /// The static [`PageTablePool`] is the boot/bootstrap source; a real
    /// per-process space is built over the `kernel/mem` frame-allocator
    /// source (`plans/WIRING.md` W5b-3).
    frames: &'static dyn PageTableFrames,
}

impl AddressSpace {
    /// Build a new address space identity-mapping `[0, 32 MiB)`.
    ///
    /// # Errors
    ///
    /// Returns `None` if the frame source is exhausted.
    pub fn new_identity_first_32mib(frames: &'static dyn PageTableFrames) -> Option<Self> {
        let TableFrame {
            phys: pml4_phys,
            entries: pml4,
        } = frames.alloc_table()?;
        let TableFrame {
            phys: pdpt_phys,
            entries: pdpt,
        } = frames.alloc_table()?;
        let TableFrame {
            phys: pd_phys,
            entries: pd,
        } = frames.alloc_table()?;

        pml4[0] = pdpt_phys | flags::PRESENT | flags::WRITABLE;
        pdpt[0] = pd_phys | flags::PRESENT | flags::WRITABLE;
        // 16 × 2 MiB = 32 MiB identity-mapped.
        for (i, slot) in pd.iter_mut().take(16).enumerate() {
            *slot = ((i as u64) << 21) | flags::PRESENT | flags::WRITABLE | flags::HUGE;
        }

        // Mirror the boot trampoline's higher-half kernel window so the
        // higher-half-linked kernel code/stack/data stay reachable after a
        // CR3 switch to this space (`boot.s` SAFETY-INVARIANT 9). Map the
        // -2 GiB window at KERNEL_VMA_BASE onto physical [0, 1 GiB) with
        // 2 MiB huge pages — the same first-GiB identity PD the trampoline
        // reuses, covering the whole kernel image.
        let TableFrame {
            phys: pdpt_high_phys,
            entries: pdpt_high,
        } = frames.alloc_table()?;
        let TableFrame {
            phys: pd_high_phys,
            entries: pd_high,
        } = frames.alloc_table()?;
        let hi_i4 = ((KERNEL_VMA_BASE >> 39) & 0x1FF) as usize;
        let hi_i3 = ((KERNEL_VMA_BASE >> 30) & 0x1FF) as usize;
        pml4[hi_i4] = pdpt_high_phys | flags::PRESENT | flags::WRITABLE;
        pdpt_high[hi_i3] = pd_high_phys | flags::PRESENT | flags::WRITABLE;
        for (i, slot) in pd_high.iter_mut().enumerate() {
            *slot = ((i as u64) << 21) | flags::PRESENT | flags::WRITABLE | flags::HUGE;
        }

        Some(Self {
            pml4_phys,
            pml4,
            frames,
        })
    }

    /// `true` if `vaddr` already resolves to a live leaf (4 KiB page or
    /// 2 MiB huge page) in this hierarchy.
    ///
    /// A read-only four-level walk used by the
    /// [`rustos_arch_api::mmu::AddressSpace`] HAL impl to report
    /// [`rustos_arch_api::mmu::MapError::AlreadyMapped`] rather than
    /// silently clobber an existing mapping (`map_4k_inner` overwrites a
    /// PT leaf without checking, so the HAL layer must guard it here). It
    /// recovers intermediate tables from the low physical address each
    /// entry holds, exactly as [`ensure_child`] does, so it is only valid
    /// on the bare-metal target where the low identity map is live.
    #[cfg(all(target_arch = "x86_64", target_os = "none"))]
    fn leaf_present(&self, vaddr: u64) -> bool {
        let i4 = ((vaddr >> 39) & 0x1FF) as usize;
        let i3 = ((vaddr >> 30) & 0x1FF) as usize;
        let i2 = ((vaddr >> 21) & 0x1FF) as usize;
        let i1 = ((vaddr >> 12) & 0x1FF) as usize;
        const ADDR_MASK: u64 = 0x000F_FFFF_FFFF_F000;
        let e4 = self.pml4[i4];
        if e4 & flags::PRESENT == 0 {
            return false;
        }
        // SAFETY: a present entry holds a low physical table address
        // `ensure_child` wrote; the low 32 MiB is identity-mapped, so the
        // address dereferences directly on the bare-metal target.
        let pdpt = unsafe { &*((e4 & ADDR_MASK) as *const [u64; ENTRIES_PER_TABLE]) };
        let e3 = pdpt[i3];
        if e3 & flags::PRESENT == 0 {
            return false;
        }
        if e3 & flags::HUGE != 0 {
            return true;
        }
        // SAFETY: as above — a present non-huge PDPT entry's address is a
        // live identity-mapped PD.
        let pd = unsafe { &*((e3 & ADDR_MASK) as *const [u64; ENTRIES_PER_TABLE]) };
        let e2 = pd[i2];
        if e2 & flags::PRESENT == 0 {
            return false;
        }
        if e2 & flags::HUGE != 0 {
            return true;
        }
        // SAFETY: as above — a present non-huge PD entry's address is a
        // live identity-mapped PT.
        let pt = unsafe { &*((e2 & ADDR_MASK) as *const [u64; ENTRIES_PER_TABLE]) };
        pt[i1] & flags::PRESENT != 0
    }

    /// Map `paddr` at `vaddr` with a 4 KiB page granularity.
    ///
    /// `vaddr` and `paddr` must be 4 KiB-aligned. Returns `None` on
    /// page-table-pool exhaustion. Aborts the existing 2 MiB huge-page
    /// covering `vaddr` if necessary (the Stage-2 tests stay outside
    /// the identity-mapped range so this is not exercised here).
    pub fn map_4k(
        &mut self,
        frames: &'static dyn PageTableFrames,
        vaddr: u64,
        paddr: u64,
        writable: bool,
    ) -> Option<()> {
        self.map_4k_inner(frames, vaddr, paddr, writable, false, false)
    }

    /// Map `paddr` at `vaddr` (4 KiB granularity) **user-accessible**:
    /// the leaf and every intermediate table entry on the walk get the
    /// [`flags::USER`] bit, so a ring-3 (CPL 3) program may reach the
    /// page. `writable` selects [`flags::WRITABLE`] on the leaf; an
    /// executable ring-3 page is mapped with `writable = false` (W^X,
    /// `AGENTS.md` §19.2).
    ///
    /// `vaddr` and `paddr` must be 4 KiB-aligned. Returns `None` on
    /// page-table-pool exhaustion or if the walk hits an existing huge
    /// page.
    pub fn map_4k_user(
        &mut self,
        frames: &'static dyn PageTableFrames,
        vaddr: u64,
        paddr: u64,
        writable: bool,
    ) -> Option<()> {
        self.map_4k_inner(frames, vaddr, paddr, writable, true, false)
    }

    /// Map `paddr` at `vaddr` (4 KiB granularity) **user-accessible** with
    /// explicit W^X leaf permissions: `writable` selects [`flags::WRITABLE`]
    /// and `executable` selects whether the page is instruction-fetchable.
    /// A non-executable leaf gets the [`flags::NO_EXECUTE`] bit, so a
    /// writable data page is mapped non-executable (`RW`) and a read-only
    /// data page non-executable (`R`) — the `AGENTS.md` §19.2 W^X contract a
    /// process image's `RW`/`R` segments and its stack need (a code segment
    /// is mapped `executable = true`, `writable = false`, i.e. `RX`).
    ///
    /// The caller must have enabled `IA32_EFER.NXE` before mapping any
    /// non-executable page (otherwise bit 63 is reserved and the walk
    /// faults). `vaddr` and `paddr` must be 4 KiB-aligned. Returns `None` on
    /// page-table-pool exhaustion or if the walk hits an existing huge page.
    pub fn map_4k_user_wx(
        &mut self,
        frames: &'static dyn PageTableFrames,
        vaddr: u64,
        paddr: u64,
        writable: bool,
        executable: bool,
    ) -> Option<()> {
        self.map_4k_inner(frames, vaddr, paddr, writable, true, !executable)
    }

    /// Shared 4 KiB mapping walk for [`Self::map_4k`] and
    /// [`Self::map_4k_user`] (`AGENTS.md` §2.2 — one definition).
    ///
    /// When `user` is set, [`flags::USER`] is OR-ed into the leaf and
    /// into each intermediate entry on the walk; a kernel mapping leaves
    /// every level without the bit, so ring 3 cannot reach it.
    fn map_4k_inner(
        &mut self,
        frames: &'static dyn PageTableFrames,
        vaddr: u64,
        paddr: u64,
        writable: bool,
        user: bool,
        no_execute: bool,
    ) -> Option<()> {
        assert_eq!(vaddr & 0xFFF, 0, "vaddr must be page-aligned");
        assert_eq!(paddr & 0xFFF, 0, "paddr must be page-aligned");

        let mut flags_ = flags::PRESENT;
        if writable {
            flags_ |= flags::WRITABLE;
        }
        if user {
            flags_ |= flags::USER;
        }
        if no_execute {
            flags_ |= flags::NO_EXECUTE;
        }

        let i4 = ((vaddr >> 39) & 0x1FF) as usize;
        let i3 = ((vaddr >> 30) & 0x1FF) as usize;
        let i2 = ((vaddr >> 21) & 0x1FF) as usize;
        let i1 = ((vaddr >> 12) & 0x1FF) as usize;

        let pdpt = ensure_child(self.pml4, i4, frames)?;
        if user {
            self.pml4[i4] |= flags::USER;
        }
        let pd = ensure_child(pdpt, i3, frames)?;
        if user {
            pdpt[i3] |= flags::USER;
        }

        // Refuse to silently shatter an existing huge page — the test
        // explicitly uses VAs outside the bootstrap identity range so
        // this path returns `None` if anyone hits it.
        if (pd[i2] & flags::HUGE) != 0 {
            return None;
        }
        let pt = ensure_child(pd, i2, frames)?;
        if user {
            pd[i2] |= flags::USER;
        }
        pt[i1] = paddr | flags_;
        Some(())
    }

    /// Switch the active page table to this address space.
    ///
    /// # Safety
    ///
    /// Caller must guarantee that the new PML4 also maps the currently
    /// executing instruction's `rip` and the current stack — otherwise
    /// the CPU will fault on the very next memory access.
    /// [`Self::new_identity_first_32mib`] upholds that by mapping both the
    /// low 32 MiB (boot stack / low physical) and the higher-half kernel
    /// window (where the higher-half-linked code/stack/data live).
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

impl MmuAddressSpace for AddressSpace {
    fn map_page(&mut self, vaddr: u64, paddr: u64, flags: PageFlags) -> Result<(), MapError> {
        if (vaddr & (PAGE_SIZE as u64 - 1)) != 0 || (paddr & (PAGE_SIZE as u64 - 1)) != 0 {
            return Err(MapError::Misaligned);
        }
        if flags.is_write_exec() {
            return Err(MapError::InvalidFlags);
        }
        // The four-level walk is only valid on the bare-metal target (it
        // recovers tables through the low identity map). `map_page` is
        // therefore proven by the `memory_isolation` QEMU vertical, not a
        // host conformance test; on the host it is unreachable.
        #[cfg(all(target_arch = "x86_64", target_os = "none"))]
        {
            if self.leaf_present(vaddr) {
                return Err(MapError::AlreadyMapped);
            }
            let frames = self.frames;
            let writable = flags.contains(PageFlags::WRITE);
            let result = if flags.contains(PageFlags::USER) {
                let executable = flags.contains(PageFlags::EXEC);
                self.map_4k_user_wx(frames, vaddr, paddr, writable, executable)
            } else {
                self.map_4k(frames, vaddr, paddr, writable)
            };
            // Alignment and prior-mapping are ruled out, so the only
            // remaining failure is page-table-pool exhaustion.
            result.ok_or(MapError::PoolExhausted)
        }
        #[cfg(not(all(target_arch = "x86_64", target_os = "none")))]
        {
            // `self.frames` is read only by the bare-metal walk above;
            // touch it here so the host build does not flag it unused.
            let _ = (vaddr, paddr, flags, self.frames);
            unreachable!("the x86_64 page-table walk is only valid on the bare-metal target")
        }
    }

    fn translate(&self, vaddr: u64) -> Option<(u64, PageFlags)> {
        // The four-level walk is only valid on the bare-metal target (it
        // recovers tables through the low identity map), exactly like
        // `map_page`; on the host it is unreachable.
        #[cfg(all(target_arch = "x86_64", target_os = "none"))]
        {
            const ADDR_MASK: u64 = 0x000F_FFFF_FFFF_F000;
            let i4 = ((vaddr >> 39) & 0x1FF) as usize;
            let i3 = ((vaddr >> 30) & 0x1FF) as usize;
            let i2 = ((vaddr >> 21) & 0x1FF) as usize;
            let i1 = ((vaddr >> 12) & 0x1FF) as usize;
            let e4 = self.pml4[i4];
            if e4 & flags::PRESENT == 0 {
                return None;
            }
            // SAFETY: a present entry holds a low identity-mapped table
            // address `ensure_child` wrote (the same round-trip
            // `leaf_present` relies on).
            let pdpt = unsafe { &*((e4 & ADDR_MASK) as *const [u64; ENTRIES_PER_TABLE]) };
            let e3 = pdpt[i3];
            if e3 & flags::PRESENT == 0 {
                return None;
            }
            if e3 & flags::HUGE != 0 {
                return Some((
                    resolved_page(e3 & ADDR_MASK, vaddr, 30),
                    page_flags_from_pte(e3),
                ));
            }
            // SAFETY: as above — a present non-huge PDPT entry's address
            // is a live identity-mapped PD.
            let pd = unsafe { &*((e3 & ADDR_MASK) as *const [u64; ENTRIES_PER_TABLE]) };
            let e2 = pd[i2];
            if e2 & flags::PRESENT == 0 {
                return None;
            }
            if e2 & flags::HUGE != 0 {
                return Some((
                    resolved_page(e2 & ADDR_MASK, vaddr, 21),
                    page_flags_from_pte(e2),
                ));
            }
            // SAFETY: as above — a present non-huge PD entry's address is
            // a live identity-mapped PT.
            let pt = unsafe { &*((e2 & ADDR_MASK) as *const [u64; ENTRIES_PER_TABLE]) };
            let e1 = pt[i1];
            if e1 & flags::PRESENT == 0 {
                return None;
            }
            Some((e1 & ADDR_MASK, page_flags_from_pte(e1)))
        }
        #[cfg(not(all(target_arch = "x86_64", target_os = "none")))]
        {
            let _ = vaddr;
            unreachable!("the x86_64 page-table walk is only valid on the bare-metal target")
        }
    }

    fn unmap(&mut self, vaddr: u64) -> Result<u64, MapError> {
        if (vaddr & (PAGE_SIZE as u64 - 1)) != 0 {
            return Err(MapError::Misaligned);
        }
        #[cfg(all(target_arch = "x86_64", target_os = "none"))]
        {
            const ADDR_MASK: u64 = 0x000F_FFFF_FFFF_F000;
            let i4 = ((vaddr >> 39) & 0x1FF) as usize;
            let i3 = ((vaddr >> 30) & 0x1FF) as usize;
            let i2 = ((vaddr >> 21) & 0x1FF) as usize;
            let i1 = ((vaddr >> 12) & 0x1FF) as usize;
            // Navigate to the 4 KiB PT leaf without allocating. A missing
            // level or a huge-page leaf means there is no 4 KiB leaf to
            // tear down here — fail closed (per-page unmap never shatters
            // a huge page).
            let e4 = self.pml4[i4];
            if e4 & flags::PRESENT == 0 {
                return Err(MapError::NotMapped);
            }
            // SAFETY: present entry → identity-mapped PDPT (see `translate`).
            let pdpt = unsafe { &*((e4 & ADDR_MASK) as *const [u64; ENTRIES_PER_TABLE]) };
            let e3 = pdpt[i3];
            if e3 & flags::PRESENT == 0 || e3 & flags::HUGE != 0 {
                return Err(MapError::NotMapped);
            }
            // SAFETY: present non-huge PDPT entry → identity-mapped PD.
            let pd = unsafe { &*((e3 & ADDR_MASK) as *const [u64; ENTRIES_PER_TABLE]) };
            let e2 = pd[i2];
            if e2 & flags::PRESENT == 0 || e2 & flags::HUGE != 0 {
                return Err(MapError::NotMapped);
            }
            // SAFETY: present non-huge PD entry → identity-mapped PT, and
            // `&mut self` makes the exclusive borrow of the leaf sound.
            let pt = unsafe { &mut *((e2 & ADDR_MASK) as *mut [u64; ENTRIES_PER_TABLE]) };
            let e1 = pt[i1];
            if e1 & flags::PRESENT == 0 {
                return Err(MapError::NotMapped);
            }
            pt[i1] = 0;
            Ok(e1 & ADDR_MASK)
        }
        #[cfg(not(all(target_arch = "x86_64", target_os = "none")))]
        {
            let _ = vaddr;
            unreachable!("the x86_64 page-table walk is only valid on the bare-metal target")
        }
    }

    fn root_phys(&self) -> u64 {
        self.pml4_phys
    }

    fn block_split_support(&self) -> BlockSplit {
        // The four-level walk has 1 GiB / 2 MiB huge leaves a split *could*
        // re-express at 4 KiB granularity (pure page-table work, no silicon
        // dependency), but the primitive has not been written for x86_64
        // yet: it lands with this port's own guard-page fault-form. aarch64
        // has it (`plans/PI.md` G1/G2). Honest `Pending`, never a pretend
        // no-op (`AGENTS.md` §2.17).
        BlockSplit::Pending(
            "x86_64 huge-page split lands with the x86_64 guard-page fault-form (plans/PI.md G3)",
        )
    }

    unsafe fn activate(&self) {
        #[cfg(all(target_arch = "x86_64", target_os = "none"))]
        {
            // SAFETY: forwards to the gated `CR3` load primitive; the
            // caller upholds the `MmuAddressSpace::activate` contract (this
            // space maps the current `rip`/`rsp`), which is exactly
            // `AddressSpace::switch`'s contract.
            unsafe { self.switch() };
        }
        #[cfg(not(all(target_arch = "x86_64", target_os = "none")))]
        {
            unreachable!("CR3 activation is only meaningful on the x86_64 bare-metal target")
        }
    }
}

impl TlbShootdown for AddressSpace {
    fn flush_page(&mut self, vaddr: u64) {
        #[cfg(all(target_arch = "x86_64", target_os = "none"))]
        {
            // SAFETY: `invlpg` invalidates the calling CPU's TLB entry for
            // the page containing the operand address; it touches no
            // memory and only discards a cached translation. No Rust
            // spelling exists.
            unsafe {
                core::arch::asm!(
                    "invlpg [{addr}]",
                    addr = in(reg) vaddr,
                    options(nostack, preserves_flags),
                );
            }
        }
        #[cfg(not(all(target_arch = "x86_64", target_os = "none")))]
        {
            // The host has no TLB to invalidate; a flush is vacuous.
            let _ = vaddr;
        }
    }
}

/// Decode an x86_64 leaf PTE's permission bits back into the neutral
/// [`PageFlags`]. Present implies readable; `WRITABLE`/`USER` map
/// directly; executability is the inverse of the `NO_EXECUTE` bit. Only
/// compiled on the bare-metal target where the page-table walk runs.
#[cfg(all(target_arch = "x86_64", target_os = "none"))]
fn page_flags_from_pte(pte: u64) -> PageFlags {
    let mut out = PageFlags::READ;
    if pte & flags::WRITABLE != 0 {
        out = out | PageFlags::WRITE;
    }
    if pte & flags::USER != 0 {
        out = out | PageFlags::USER;
    }
    if pte & flags::NO_EXECUTE == 0 {
        out = out | PageFlags::EXEC;
    }
    out
}

/// 4 KiB-aligned physical address `vaddr` resolves to under a leaf whose
/// region starts at `leaf_base` and spans `1 << region_shift` bytes
/// (30 = 1 GiB PDPT leaf, 21 = 2 MiB PD leaf, 12 = 4 KiB PT leaf). Only
/// compiled on the bare-metal target.
#[cfg(all(target_arch = "x86_64", target_os = "none"))]
fn resolved_page(leaf_base: u64, vaddr: u64, region_shift: u32) -> u64 {
    let region_mask = (1u64 << region_shift) - 1;
    (leaf_base + (vaddr & region_mask)) & !((PAGE_SIZE as u64) - 1)
}

// `&mut [u64; 512]` in, `&'static mut [u64; 512]` out: the returned
// reference does not borrow from `parent` (it points at a freshly
// alloc'd table from `frames`, or at a sibling table recovered through
// the identity map). `mut_from_ref` / `mut_from_immut` clippy lint
// flags this shape because the function does not return a borrow of
// `parent`'s lifetime — which is exactly the documented contract.
#[allow(clippy::mut_from_ref)]
fn ensure_child(
    parent: &mut [u64; ENTRIES_PER_TABLE],
    idx: usize,
    frames: &'static dyn PageTableFrames,
) -> Option<&'static mut [u64; ENTRIES_PER_TABLE]> {
    let entry = parent[idx];
    if entry & flags::PRESENT != 0 {
        // Existing child — recover the `&mut` from the physical address.
        // Identity mapping makes phys = virt here.
        let phys = entry & 0x000F_FFFF_FFFF_F000;
        // SAFETY: every entry that has PRESENT set was inserted below (or
        // by `new_identity_first_32mib`) with a physical address that came
        // from a `TableFrame`, so the round-trip is valid; identity
        // mapping means we can dereference the physical address directly.
        let child: &'static mut [u64; ENTRIES_PER_TABLE] =
            unsafe { &mut *(phys as usize as *mut [u64; ENTRIES_PER_TABLE]) };
        Some(child)
    } else {
        let TableFrame { phys, entries } = frames.alloc_table()?;
        parent[idx] = phys | flags::PRESENT | flags::WRITABLE;
        Some(entries)
    }
}

fn phys_of(table: &[u64; ENTRIES_PER_TABLE]) -> u64 {
    // The page-table pool is a higher-half kernel static (linked at
    // KERNEL_VMA_BASE + phys; see linker.ld / boot.s SAFETY-INVARIANT 9).
    // Convert its virtual address back to the physical address the MMU
    // needs in a page-table entry or CR3. The subtraction cannot wrap: on
    // the bare-metal target every kernel static lives at or above
    // KERNEL_VMA_BASE.
    (table.as_ptr() as u64) - KERNEL_VMA_BASE
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
        // Intel SDM Vol 3A §4.5 paging-structure flag bit positions.
        assert_eq!(flags::PRESENT, 1 << 0);
        assert_eq!(flags::WRITABLE, 1 << 1);
        assert_eq!(flags::USER, 1 << 2);
        assert_eq!(flags::HUGE, 1 << 7);
        assert_eq!(flags::NO_EXECUTE, 1 << 63);
    }
}
