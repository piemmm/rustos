//! Page-table primitives for the Stage-2 memory-isolation test.
//!
//! The test in `tests/integration/memory_isolation` needs two
//! page-table hierarchies that disagree about a single virtual address:
//! a *victim* address space in which the address resolves to a known
//! frame, and an *attacker* address space in which it does not. The CPU
//! must fault the attacker on access. That is the architectural
//! guarantee ("Memory isolation is enforced by hardware")
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
//! ([`tairix_arch_api::mmu::AddressSpace`] +
//! [`tairix_arch_api::tlb::TlbShootdown`]) `kernel/mem` drives. The
//! page-table *walk* (`map_page` / `translate` / `unmap`) recovers
//! intermediate tables through the low identity map and so is only valid
//! on the bare-metal target; like [`AddressSpace::activate`] it is proven
//! by the `memory_isolation` QEMU vertical, not a host conformance test
//! (the host build of those methods is `unreachable!`). The bit math is
//! a strict subset so promotion does not require interface creep.

use core::cell::UnsafeCell;
use core::sync::atomic::{AtomicU64, AtomicUsize, Ordering};

use tairix_arch_api::frames::{PageTableFrames, TableFrame};
use tairix_arch_api::mmu::{
    AddressSpace as MmuAddressSpace, BlockSplit, KernelWindow, MapError, PageFlags,
};
use tairix_arch_api::tlb::TlbShootdown;

/// Size of a single page (and of a page-table page): the one system granule.
pub use tairix_abi::PAGE_SIZE;

/// Number of 64-bit entries in a page-table page (PML4 / PDPT / PD / PT).
pub const ENTRIES_PER_TABLE: usize = 512;

/// Span of a single PD-level (2 MiB) huge-page block.
///
/// The unit [`AddressSpace::prepare_guard_arena`] walks an arena in: every
/// 2 MiB block the arena spans is re-expressed at 4 KiB granularity so a
/// single guard page inside it can be unmapped (`plans/PI.md` G2).
pub const BLOCK_2MIB: u64 = 2 * 1024 * 1024;

/// Gigabytes of physical memory the boot trampoline identity-maps before
/// any discovery has run (`boot.s` SAFETY-INVARIANT 4, whose static
/// `boot_pds` array holds exactly this many page directories). It is the
/// floor [`widen_boot_identity`] widens from, and the window every root
/// built before the widening carries.
pub const BOOT_IDENTITY_GIB: usize = 4;

/// Largest identity window the low PDPT can express: its 512 slots at
/// 1 GiB each. A wider window would need a second PML4 slot, which the
/// port's virtual layout gives to user space.
pub const MAX_IDENTITY_GIB: usize = ENTRIES_PER_TABLE;

/// Gigabytes of physical memory currently identity-mapped in every
/// translation root, published by [`widen_boot_identity`] once the boot
/// memory map is known. Read on every direct-map translate and by every
/// root constructor, so the whole system shares one window.
static IDENTITY_GIGAPAGES: AtomicUsize = AtomicUsize::new(BOOT_IDENTITY_GIB);

/// Gigabytes of physical memory the live identity window covers.
#[must_use]
pub fn configured_identity_gigapages() -> usize {
    IDENTITY_GIGAPAGES.load(Ordering::Acquire)
}

/// Exclusive top of the live identity window, in bytes — the direct
/// physical map's limit and the ceiling a kernel stack or arena block must
/// sit below to stay reachable under every root.
#[must_use]
pub fn configured_identity_bytes() -> u64 {
    (configured_identity_gigapages() as u64) << 30
}

/// `true` when the part maps 1 GiB pages at PDPT level (CPUID
/// `0x8000_0001` `EDX[26]`, AMD64 APM Vol. 3 / Intel SDM Vol. 2A).
///
/// With gigapages a whole identity window costs no page directories at
/// all, so a root's identity map is one table however much RAM the
/// machine has. The host build reports `false` (no CPU to ask), which
/// only selects the 2 MiB path the host never walks.
#[must_use]
pub fn gigapages_supported() -> bool {
    #[cfg(all(target_arch = "x86_64", target_os = "none"))]
    {
        // Leaf `0x8000_0000` reports the highest extended leaf, so the
        // feature leaf is read only once the part admits to having it.
        if core::arch::x86_64::__cpuid(0x8000_0000).eax < 0x8000_0001 {
            return false;
        }
        core::arch::x86_64::__cpuid(0x8000_0001).edx & (1 << 26) != 0
    }
    #[cfg(not(all(target_arch = "x86_64", target_os = "none")))]
    {
        false
    }
}

/// Page directories [`widen_boot_identity`] and the root constructors need
/// to identity-map `gib` gigabytes: none when the part has 1 GiB pages
/// (the PDPT holds the leaves itself), otherwise one per gigabyte.
#[must_use]
pub fn identity_directory_frames(gib: usize) -> usize {
    if gigapages_supported() {
        0
    } else {
        gib
    }
}

/// Base virtual address of the -2 GiB higher-half kernel window.
///
/// A kernel symbol linked at `KERNEL_VMA_BASE + p` is loaded at physical
/// `p` (`kernel/arch/x86_64/linker.ld`; `boot.s` SAFETY-INVARIANT 9). Used
/// to turn a higher-half kernel virtual address back into the physical
/// address the MMU needs in a page-table entry or CR3. Must equal the
/// `KERNEL_VMA_BASE` in `linker.ld` and the literal in `boot.s`.
pub const KERNEL_VMA_BASE: u64 = 0xFFFF_FFFF_8000_0000;

/// Physical-address field of a page-table entry (bits 51:12).
///
/// Masking an entry with this recovers the child table's — or the mapped
/// page's — physical address without the flag and attribute bits.
const ADDR_MASK: u64 = 0x000F_FFFF_FFFF_F000;

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
    /// Page-level write-through/PAT index bit.
    pub const WRITE_THROUGH: u64 = 1 << 3;
    /// Page-level cache-disable/PAT index bit.
    pub const CACHE_DISABLE: u64 = 1 << 4;
    /// Accessed: the CPU sets this on the leaf (and every intermediate
    /// entry it walks) the first time the page is read, written, or
    /// fetched, and never clears it itself (Intel SDM Vol 3A §4.8). This
    /// is the hardware referenced bit the page-replacement clock scan
    /// reads and clears to tell a genuinely cold page from a hot one
    /// before the compressed-memory tier reclaims it.
    pub const ACCESSED: u64 = 1 << 5;
    /// Page Size (1 for huge pages at PD or PDPT level).
    pub const HUGE: u64 = 1 << 7;
    /// No-Execute (bit 63): an instruction fetch from the page faults.
    /// Honoured only while `IA32_EFER.NXE` is set; with NXE clear the bit
    /// is reserved and would fault the walk, so callers that set it must
    /// have enabled NXE first. Used to mark writable user data and
    /// read-only user data non-executable (W^X).
    pub const NO_EXECUTE: u64 = 1 << 63;
}

/// Leaf permissions and memory attributes one 4 KiB mapping walk applies.
///
/// Grouped into a named value so the walk is steered by labelled fields
/// rather than a row of positional booleans at each call site.
#[derive(Clone, Copy)]
struct LeafPolicy {
    writable: bool,
    user: bool,
    no_execute: bool,
    memory_attrs: u64,
}

impl LeafPolicy {
    /// The page-table entry flag word this policy maps to.
    fn pte_flags(self) -> u64 {
        let mut bits = flags::PRESENT;
        if self.writable {
            bits |= flags::WRITABLE;
        }
        if self.user {
            bits |= flags::USER;
        }
        if self.no_execute {
            bits |= flags::NO_EXECUTE;
        }
        bits | self.memory_attrs
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

/// Page-table pages one live root costs at the boot identity floor: the
/// PML4, the low PDPT, a page directory per identity gigabyte (none where
/// the part has 1 GiB pages), the higher-half window's PDPT and PD, and one
/// fine-grained PDPT/PD/PT chain.
const PAGES_PER_LIVE_ROOT: usize = 2 + BOOT_IDENTITY_GIB + 2 + 3;

/// Pages a further fine-grained mapping costs (PDPT + PD + PT).
const PAGES_PER_FINE_CHAIN: usize = 3;

/// Maximum number of page-table pages a static pool hands out. Sized for the
/// two roots a bring-up path or a fixture builds at once, plus a further
/// fine-grained chain each. A window widened past the boot floor needs more,
/// and `alloc_table` then answers `None`, so the constructor fails closed
/// rather than returning a root with holes in its identity map.
const POOL_SIZE: usize = 2 * (PAGES_PER_LIVE_ROOT + PAGES_PER_FINE_CHAIN);

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
        // is suppressed with rationale. The
        // array itself is `POOL_SIZE * sizeof::<Table>()`
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
    /// as a closed-fail (: deterministic OOM, never panic).
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
        // recovers the physical address the MMU needs (`plans/WIRING.md` W5b-3 — the bootstrap frame source).
        let phys = phys_of(entries);
        Some(TableFrame { phys, entries })
    }

    fn free_table(&self, phys: u64) {
        // The boot pool is a bump allocator over permanent kernel-image
        // `.bss`: its storage is never reclaimable RAM and the boot space
        // built over it is never torn down, so a returned frame is retired
        // without reuse. Per-process spaces draw from the allocator-backed
        // `kernel/mem` source, whose `free_table` genuinely recycles.
        let _ = phys;
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
    /// tables from, retained so the [`tairix_arch_api::mmu::AddressSpace`]
    /// HAL impl can install mappings without the caller re-supplying it.
    /// The static [`PageTablePool`] is the boot/bootstrap source; a real
    /// per-process space is built over the `kernel/mem` frame-allocator
    /// source (`plans/WIRING.md` W5b-3).
    frames: &'static dyn PageTableFrames,
}

impl AddressSpace {
    /// Build a root carrying the live identity window plus the higher-half
    /// kernel window — the constructor for any space that will be **made
    /// live**.
    ///
    /// The extent is not a parameter, and deliberately so: kernel code runs
    /// with the current task's root active, so a root that maps less than
    /// [`configured_identity_gigapages`] leaves kernel memory above its own
    /// ceiling unreachable while it is loaded. That is silent until something
    /// the kernel touches happens to land high — a kernel-heap slab page
    /// drawn from a frame, an intermediate table the walk dereferences
    /// through its low physical address (see [`ensure_child`]), a frame handed
    /// to a process image — at which point the fault has no local cause. The
    /// window is read here so no caller can get it wrong. The leaves are
    /// 1 GiB pages where the part has them and 2 MiB pages otherwise.
    ///
    /// # Errors
    ///
    /// Returns `None` if the frame source is exhausted or the window
    /// overflows the 2 MiB-page count.
    pub fn new_identity_window(frames: &'static dyn PageTableFrames) -> Option<Self> {
        // 512 × 2 MiB = 1 GiB.
        Self::new_identity(frames, configured_identity_gigapages().checked_mul(512)?)
    }

    /// Build a root identity-mapping only `[0, 32 MiB)`, for a space that is
    /// **never made live**.
    ///
    /// The MMIO register-window maps use one purely as page-table
    /// bookkeeping — the device is reached through the direct physical map,
    /// never through this root — and their window base sits inside the live
    /// identity window, so a root carrying that window would collide with it.
    /// Making this space live would strand every kernel address above
    /// 32 MiB; use [`Self::new_identity_window`] for anything that runs.
    ///
    /// # Errors
    ///
    /// Returns `None` if the frame source is exhausted.
    pub fn new_bookkeeping_identity_32mib(frames: &'static dyn PageTableFrames) -> Option<Self> {
        // 16 × 2 MiB = 32 MiB.
        Self::new_identity(frames, 16)
    }

    /// Shared constructor backing [`Self::new_identity_window`] and
    /// [`Self::new_bookkeeping_identity_32mib`] (one definition).
    ///
    /// Identity-maps the first `pages_2mib` 2 MiB pages and mirrors the boot
    /// trampoline's higher-half kernel window. A whole-gigabyte span on a
    /// part with 1 GiB pages is laid down as PDPT leaves; otherwise one page
    /// directory is drawn per gigabyte.
    fn new_identity(frames: &'static dyn PageTableFrames, pages_2mib: usize) -> Option<Self> {
        // One PDPT addresses 512 GiB; a wider span has nowhere to put its
        // remaining directories, so refuse rather than index past the table.
        if pages_2mib > ENTRIES_PER_TABLE * ENTRIES_PER_TABLE {
            return None;
        }
        let TableFrame {
            phys: pml4_phys,
            entries: pml4,
        } = frames.alloc_table()?;
        let TableFrame {
            phys: pdpt_phys,
            entries: pdpt,
        } = frames.alloc_table()?;
        pml4[0] = pdpt_phys | flags::PRESENT | flags::WRITABLE;

        // A whole-gigabyte window on a part with 1 GiB pages needs no page
        // directories: the PDPT carries the leaves, so a root's identity map
        // costs one table however much RAM the machine has.
        if gigapages_supported() && pages_2mib.is_multiple_of(ENTRIES_PER_TABLE) {
            for (gib, slot) in pdpt
                .iter_mut()
                .take(pages_2mib / ENTRIES_PER_TABLE)
                .enumerate()
            {
                *slot = ((gib as u64) << 30) | flags::PRESENT | flags::WRITABLE | flags::HUGE;
            }
        } else {
            // Identity-map `pages_2mib` 2 MiB pages, one PD (512 entries =
            // 1 GiB) at a time, linking each into the low PDPT.
            let mut mapped = 0usize;
            let mut pdpt_idx = 0usize;
            while mapped < pages_2mib {
                let TableFrame {
                    phys: pd_phys,
                    entries: pd,
                } = frames.alloc_table()?;
                pdpt[pdpt_idx] = pd_phys | flags::PRESENT | flags::WRITABLE;
                for slot in pd.iter_mut() {
                    if mapped >= pages_2mib {
                        break;
                    }
                    *slot =
                        ((mapped as u64) << 21) | flags::PRESENT | flags::WRITABLE | flags::HUGE;
                    mapped += 1;
                }
                pdpt_idx += 1;
            }
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

        // Every root reaches the kernel remap window, so a kernel address
        // in it resolves whichever root is active. Done here rather than at
        // each call site so no future space can be built without it.
        install_kernel_window_slot(pml4);

        Some(Self {
            pml4_phys,
            pml4,
            frames,
        })
    }

    /// Build a root that maps **only** the kernel remap window — the handle
    /// the kernel-heap remap layer edits the window's shared sub-hierarchy
    /// through.
    ///
    /// The root is never loaded into `CR3`: because the window's PML4 entry
    /// points at a PDPT every other root shares, a leaf installed through
    /// this space is immediately visible under all of them. Keeping it
    /// separate means the remap layer draws its intermediate tables from the
    /// frame allocator rather than from the fixed boot pool, and cannot
    /// reach any address outside the window.
    ///
    /// # Errors
    ///
    /// Returns `None` if the frame source cannot supply the root table.
    pub fn new_kernel_window(frames: &'static dyn PageTableFrames) -> Option<Self> {
        let TableFrame {
            phys: pml4_phys,
            entries: pml4,
        } = frames.alloc_table()?;
        install_kernel_window_slot(pml4);
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
    /// [`tairix_arch_api::mmu::AddressSpace`] HAL impl to report
    /// [`tairix_arch_api::mmu::MapError::AlreadyMapped`] rather than
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
        self.map_4k_inner(
            frames,
            vaddr,
            paddr,
            LeafPolicy {
                writable,
                user: false,
                no_execute: false,
                memory_attrs: 0,
            },
        )
    }

    /// Map `paddr` at `vaddr` (4 KiB granularity) **user-accessible**:
    /// the leaf and every intermediate table entry on the walk get the
    /// [`flags::USER`] bit, so a ring-3 (CPL 3) program may reach the
    /// page. `writable` selects [`flags::WRITABLE`] on the leaf; an
    /// executable ring-3 page is mapped with `writable = false` (W^X).
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
        self.map_4k_inner(
            frames,
            vaddr,
            paddr,
            LeafPolicy {
                writable,
                user: true,
                no_execute: false,
                memory_attrs: 0,
            },
        )
    }

    /// Map `paddr` at `vaddr` (4 KiB granularity) **user-accessible** with
    /// explicit W^X leaf permissions: `writable` selects [`flags::WRITABLE`]
    /// and `executable` selects whether the page is instruction-fetchable.
    /// A non-executable leaf gets the [`flags::NO_EXECUTE`] bit, so a
    /// writable data page is mapped non-executable (`RW`) and a read-only
    /// data page non-executable (`R`) — the W^X contract a
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
        self.map_4k_inner(
            frames,
            vaddr,
            paddr,
            LeafPolicy {
                writable,
                user: true,
                no_execute: !executable,
                memory_attrs: 0,
            },
        )
    }

    /// Shared 4 KiB mapping walk for [`Self::map_4k`] and
    /// [`Self::map_4k_user`] (one definition).
    ///
    /// When `leaf.user` is set, [`flags::USER`] is OR-ed into the leaf and
    /// into each intermediate entry on the walk; a kernel mapping leaves
    /// every level without the bit, so ring 3 cannot reach it.
    fn map_4k_inner(
        &mut self,
        frames: &'static dyn PageTableFrames,
        vaddr: u64,
        paddr: u64,
        leaf: LeafPolicy,
    ) -> Option<()> {
        assert_eq!(vaddr & 0xFFF, 0, "vaddr must be page-aligned");
        assert_eq!(paddr & 0xFFF, 0, "paddr must be page-aligned");

        let flags_ = leaf.pte_flags();

        let i4 = ((vaddr >> 39) & 0x1FF) as usize;
        let i3 = ((vaddr >> 30) & 0x1FF) as usize;
        let i2 = ((vaddr >> 21) & 0x1FF) as usize;
        let i1 = ((vaddr >> 12) & 0x1FF) as usize;

        let pdpt = ensure_child(self.pml4, i4, frames)?;
        if leaf.user {
            self.pml4[i4] |= flags::USER;
        }
        let pd = ensure_child(pdpt, i3, frames)?;
        if leaf.user {
            pdpt[i3] |= flags::USER;
        }

        // Refuse to silently shatter an existing huge page — the test
        // explicitly uses VAs outside the bootstrap identity range so
        // this path returns `None` if anyone hits it.
        if (pd[i2] & flags::HUGE) != 0 {
            return None;
        }
        let pt = ensure_child(pd, i2, frames)?;
        if leaf.user {
            pd[i2] |= flags::USER;
        }
        pt[i1] = paddr | flags_;
        Some(())
    }

    /// Re-express the coarse huge-page leaf(s) covering `vaddr` at 4 KiB
    /// granularity, preserving the mapped output address and every
    /// permission bit, so the single 4 KiB page containing `vaddr` can
    /// then be torn down with [`MmuAddressSpace::unmap`] (+ a
    /// [`TlbShootdown::flush_page`]) without disturbing its neighbours.
    ///
    /// This is the x86_64 foundation of the kthread guard page
    /// (`plans/PI.md` G1, the sibling of the aarch64 / riscv64 block
    /// split): a guard page that falls inside a region the boot path
    /// mapped with a coarse 1 GiB (PDPTE) or 2 MiB (PDE) *huge page*
    /// cannot be unmapped while it is part of that leaf, because the huge
    /// leaf has no per-4 KiB entry to clear. Splitting re-expresses the
    /// same translation as a table of finer leaves — a 1 GiB huge page
    /// becomes a PD of 512 × 2 MiB huge pages, then the 2 MiB huge page
    /// covering `vaddr` becomes a PT of 512 × 4 KiB pages — leaving every
    /// address translating identically but now at 4 KiB granularity.
    ///
    /// Each new table pointer carries `PRESENT | WRITABLE` plus the huge
    /// leaf's own `USER` / `NO_EXECUTE` bits, so user-accessibility and
    /// non-executability of the re-expressed region are preserved exactly
    /// (the effective permission is the AND across the walk, and every
    /// shattered child carries the same leaf permissions). The 2 MiB → 4 KiB
    /// step **clears** the page-size bit on the PT leaves: at PT level bit 7
    /// ([`flags::HUGE`]) is the PAT attribute, not a page-size flag (Intel
    /// SDM Vol 3A §4.5), so leaving it set would change the memory type.
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
    /// Like the rest of the four-level walk this recovers intermediate
    /// tables through the low identity map, so it is only valid on the
    /// bare-metal target; it is proven by the
    /// `tests/integration/stack_guard_qemu_x86_64` QEMU vertical, not a
    /// host conformance test.
    ///
    /// # Errors
    ///
    /// Returns [`MapError::Misaligned`] if `vaddr` is not 4 KiB-aligned,
    /// [`MapError::NotMapped`] if `vaddr` has no live mapping at the level
    /// being split, or [`MapError::PoolExhausted`] if the page-table pool
    /// cannot supply a replacement table. On [`MapError::PoolExhausted`]
    /// any level already split stays split (still a faithful identity
    /// re-expression of the same translation), so the address space is
    /// never left describing a *different* mapping.
    #[cfg(all(target_arch = "x86_64", target_os = "none"))]
    pub fn split_block(&mut self, vaddr: u64) -> Result<(), MapError> {
        if (vaddr & (PAGE_SIZE as u64 - 1)) != 0 {
            return Err(MapError::Misaligned);
        }
        let frames = self.frames;
        let i4 = ((vaddr >> 39) & 0x1FF) as usize;
        let i3 = ((vaddr >> 30) & 0x1FF) as usize;
        let i2 = ((vaddr >> 21) & 0x1FF) as usize;

        // --- PML4 level: always a table pointer (x86_64 has no PML4-level
        // huge leaf), so a present entry resolves to a live PDPT.
        let e4 = self.pml4[i4];
        if e4 & flags::PRESENT == 0 {
            return Err(MapError::NotMapped);
        }
        // SAFETY: a present PML4 entry holds the low physical address of a
        // PDPT (the same round-trip `translate`/`ensure_child` rely on);
        // the low identity map makes that address dereferenceable, and
        // `&mut self` makes the borrow exclusive.
        let pdpt = unsafe { &mut *((e4 & ADDR_MASK) as *mut [u64; ENTRIES_PER_TABLE]) };

        // --- PDPT level (1 GiB): a 1 GiB huge leaf becomes a PD of 512 ×
        // 2 MiB huge leaves (the page-size bit stays set — PD entries use
        // it too).
        let e3 = pdpt[i3];
        if e3 & flags::PRESENT == 0 {
            return Err(MapError::NotMapped);
        }
        if e3 & flags::HUGE != 0 {
            let TableFrame { phys, entries } =
                frames.alloc_table().ok_or(MapError::PoolExhausted)?;
            shatter_huge_into(entries, e3, 21, true);
            pdpt[i3] =
                phys | flags::PRESENT | flags::WRITABLE | (e3 & (flags::USER | flags::NO_EXECUTE));
        }

        // The PDPT slot now holds a table pointer; recover the PD.
        // SAFETY: present non-huge PDPT entry → low identity-mapped PD (as
        // above); `&mut self` keeps the borrow exclusive.
        let pd = unsafe { &mut *((pdpt[i3] & ADDR_MASK) as *mut [u64; ENTRIES_PER_TABLE]) };

        // --- PD level (2 MiB): a 2 MiB huge leaf becomes a PT of 512 ×
        // 4 KiB page leaves (the page-size bit is CLEARED — at PT level
        // bit 7 is PAT, not page-size).
        let e2 = pd[i2];
        if e2 & flags::PRESENT == 0 {
            return Err(MapError::NotMapped);
        }
        if e2 & flags::HUGE != 0 {
            let TableFrame { phys, entries } =
                frames.alloc_table().ok_or(MapError::PoolExhausted)?;
            shatter_huge_into(entries, e2, 12, false);
            pd[i2] =
                phys | flags::PRESENT | flags::WRITABLE | (e2 & (flags::USER | flags::NO_EXECUTE));
        }
        // The PD now resolves `vaddr` through a 4 KiB page leaf.
        Ok(())
    }

    /// Re-express every coarse huge-page leaf covering the arena
    /// `[base, base + len)` at 4 KiB granularity, so any single page in
    /// the arena (e.g. a kthread kernel-stack guard page) can later be
    /// torn down with [`MmuAddressSpace::unmap`] (+ a
    /// [`TlbShootdown::flush_page`]) without disturbing the block the
    /// running CPU executes on (`plans/PI.md` guard-page fault-form, stage
    /// G2).
    ///
    /// This is [`Self::split_block`] applied to every 2 MiB block the arena
    /// spans: a guard-page arena that the boot path laid down inside the
    /// coarse identity huge pages has no per-4 KiB leaf to clear, so the
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
    /// a *different* mapping (fail closed, never
    /// corrupt).
    #[cfg(all(target_arch = "x86_64", target_os = "none"))]
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

    /// Switch the active page table to this address space.
    ///
    /// # Safety
    ///
    /// Caller must guarantee that the new PML4 also maps the currently
    /// executing instruction's `rip` and the current stack — otherwise
    /// the CPU will fault on the very next memory access.
    /// [`Self::new_identity_window`] upholds that by mapping both the live
    /// identity window (boot stack / low physical) and the higher-half kernel
    /// window (where the higher-half-linked code/stack/data live).
    #[cfg(all(target_arch = "x86_64", target_os = "none"))]
    pub unsafe fn switch(&self) {
        // The first fully-configured space activated on the metal is the
        // permanent boot space: publish its root, set-once, as the park
        // root teardown and the dispatcher's suspend path re-install so a
        // dead user root is never left active (see [`park_kernel_root`]).
        let _ = PARK_ROOT.compare_exchange(0, self.pml4_phys, Ordering::AcqRel, Ordering::Relaxed);
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

/// PML4 slot the kernel remap window claims.
///
/// Chosen from the port's VA layout: the boot trampoline uses slot 0 for
/// the low identity window and slot 511 for the higher-half kernel window
/// (`boot.s` SAFETY-INVARIANT 4 and 9), so slot 510 is the highest free
/// canonical slot. One PML4 slot is 512 GiB of address space, which costs
/// nothing until something is backed into it and needs one shared PDPT.
const KERNEL_WINDOW_PML4_SLOT: usize = 510;

/// Pages the kernel remap window spans: one PML4 slot is 512 entries at
/// each of the three levels below it.
const KERNEL_WINDOW_PAGES: usize = ENTRIES_PER_TABLE * ENTRIES_PER_TABLE * ENTRIES_PER_TABLE;

/// The window's shared PML4 entry, or `0` before
/// [`reserve_kernel_window`] runs.
///
/// Every root this port builds installs it, so a leaf added under the
/// shared PDPT it points at resolves identically whichever root is active —
/// the property that lets kernel code reach a remapped kernel address while
/// a user task's root is loaded.
static KERNEL_WINDOW_PML4: AtomicU64 = AtomicU64::new(0);

/// Base virtual address of the kernel remap window (canonical: PML4 slot
/// 510 sign-extends to the higher half).
#[must_use]
pub const fn kernel_window_base() -> u64 {
    // Bit 47 of the slot's base is set, so bits 63:48 sign-extend to ones.
    0xFFFF_0000_0000_0000 | ((KERNEL_WINDOW_PML4_SLOT as u64) << 39)
}

/// A window whose extent is not representable is refused at run time, which
/// would silently leave the kernel heap on its bootstrap region. Fail the
/// build instead.
const _: () = assert!(
    KernelWindow::new(kernel_window_base(), KERNEL_WINDOW_PAGES).is_some(),
    "the kernel remap window must be a representable extent"
);

/// Widen the live translation root's low identity map to `[0, gib GiB)`
/// and publish `gib` as the window every later root and every direct-map
/// translate reads ([`configured_identity_gigapages`]).
///
/// The boot trampoline maps a fixed [`BOOT_IDENTITY_GIB`] gigabytes before
/// the firmware memory map has been parsed, which is enough to reach the
/// architectural LAPIC/IO-APIC frames and the firmware tables but not the
/// RAM of a machine with more than that installed. Once the map is known
/// the boot path calls this with the discovered window, so a frame the
/// allocator draws from the top of a multi-gigabyte pool is reachable by
/// pointer like any other.
///
/// `directories` is the physical base of [`identity_directory_frames`]
/// contiguous page-aligned frames used as page directories; it is ignored
/// (and may be zero) when the part has 1 GiB pages, which need none. The
/// frames must lie inside the *pre-widening* window, since they are written
/// through it.
///
/// Returns `false`, having changed nothing, for a `gib` that is not a
/// widening ([`BOOT_IDENTITY_GIB`] or less), exceeds
/// [`MAX_IDENTITY_GIB`], comes with a `directories` run the widening would
/// have to write outside the window it is widening from, or arrives after
/// the window has already been widened — the caller then fails the boot
/// rather than running on a window it did not install.
///
/// # Safety
///
/// * Paging is enabled, the caller runs on the boot CPU before any
///   secondary is brought up, and no other CPU is walking the low PDPT.
/// * `directories` names [`identity_directory_frames`] page-aligned frames
///   that no other owner holds (the boot path reserves them out of the
///   memory map before the frame allocator is built).
///
/// The widening only ever re-expresses `[0, BOOT_IDENTITY_GIB GiB)` with
/// the identical output addresses and adds mappings above it, so no live
/// translation changes meaning; `CR3` is reloaded at the end so the CPU
/// drops the stale entries for the re-expressed range.
#[cfg(all(target_arch = "x86_64", target_os = "none"))]
pub unsafe fn widen_boot_identity(gib: usize, directories: u64) -> bool {
    if gib <= BOOT_IDENTITY_GIB || gib > MAX_IDENTITY_GIB {
        return false;
    }
    // One widening per boot: a second would rewrite the live PDPT under
    // roots already built from the published window.
    if configured_identity_gigapages() != BOOT_IDENTITY_GIB {
        return false;
    }
    let dir_frames = identity_directory_frames(gib);
    if dir_frames != 0 {
        let bytes = (dir_frames as u64) * PAGE_SIZE as u64;
        let fits_in_current_window = directories & (PAGE_SIZE as u64 - 1) == 0
            && directories
                .checked_add(bytes)
                .is_some_and(|end| end <= (BOOT_IDENTITY_GIB as u64) << 30);
        if !fits_in_current_window {
            return false;
        }
    }

    let root = active_root_phys();
    if root == 0 {
        return false;
    }
    // SAFETY: `CR3` names the live PML4, which sits in low physical memory
    // the trampoline identity-maps, so its physical address dereferences
    // directly (the round-trip every walk in this module relies on).
    let pml4 = unsafe { &mut *(root as *mut [u64; ENTRIES_PER_TABLE]) };
    let low = pml4[0];
    if low & flags::PRESENT == 0 || low & flags::HUGE != 0 {
        return false;
    }
    // SAFETY: as above — a present non-huge PML4 entry holds the low
    // identity-mapped PDPT the trampoline built.
    let pdpt = unsafe { &mut *((low & ADDR_MASK) as *mut [u64; ENTRIES_PER_TABLE]) };

    if dir_frames == 0 {
        for (slot_gib, slot) in pdpt.iter_mut().take(gib).enumerate() {
            *slot = ((slot_gib as u64) << 30) | flags::PRESENT | flags::WRITABLE | flags::HUGE;
        }
    } else {
        for (slot_gib, slot) in pdpt.iter_mut().enumerate().take(gib) {
            let pd_phys = directories + (slot_gib as u64) * PAGE_SIZE as u64;
            // SAFETY: the caller pins `directories` as unowned page-aligned
            // frames inside the pre-widening window, so each directory
            // dereferences here and aliases nothing live.
            let pd = unsafe { &mut *(pd_phys as *mut [u64; ENTRIES_PER_TABLE]) };
            let base = (slot_gib as u64) << 30;
            for (block, entry) in pd.iter_mut().enumerate() {
                *entry = (base + ((block as u64) << 21))
                    | flags::PRESENT
                    | flags::WRITABLE
                    | flags::HUGE;
            }
            *slot = pd_phys | flags::PRESENT | flags::WRITABLE;
        }
    }

    IDENTITY_GIGAPAGES.store(gib, Ordering::Release);

    // SAFETY: reloading `CR3` with the value it already holds is a pure
    // TLB flush of the non-global entries; the root is unchanged and still
    // maps the executing code, stack, and per-CPU data.
    unsafe {
        core::arch::asm!(
            "mov {t}, cr3",
            "mov cr3, {t}",
            t = out(reg) _,
            options(nostack, preserves_flags),
        );
    }
    true
}

/// Reserve the kernel remap window: draw the shared PDPT, publish the PML4
/// entry every root installs, and patch it into the live root so the
/// running CPUs see the window immediately.
///
/// Called once, from the boot path, after the frame allocator exists (the
/// table comes from it, not from the fixed boot pool). A second call
/// returns the same window without drawing anything. Returns `None`,
/// having changed nothing, when the frame source cannot supply the shared
/// table (fail closed — the kernel heap then stays on its bootstrap
/// region).
pub fn reserve_kernel_window(frames: &'static dyn PageTableFrames) -> Option<KernelWindow> {
    let window = KernelWindow::new(kernel_window_base(), KERNEL_WINDOW_PAGES)?;
    if KERNEL_WINDOW_PML4.load(Ordering::Acquire) != 0 {
        return Some(window);
    }
    let TableFrame { phys, entries: _ } = frames.alloc_table()?;
    KERNEL_WINDOW_PML4.store(phys | flags::PRESENT | flags::WRITABLE, Ordering::Release);
    install_kernel_window(active_root_phys());
    Some(window)
}

/// Install the published window entry into the PML4 at `root_phys`, or do
/// nothing when no window is reserved (or `root_phys` is zero, which is
/// what the host build reports).
#[cfg(all(target_arch = "x86_64", target_os = "none"))]
fn install_kernel_window(root_phys: u64) {
    if root_phys == 0 {
        return;
    }
    // SAFETY: a non-zero `CR3` base names the live PML4, which lives in low
    // physical memory the boot trampoline identity-maps, so the physical
    // address dereferences directly (the same round-trip `ensure_child`
    // relies on). The only entry written is the window's own slot, which no
    // other writer touches.
    let root = unsafe { &mut *(root_phys as *mut [u64; ENTRIES_PER_TABLE]) };
    install_kernel_window_slot(root);
}

/// Host substitute: there is no live `CR3` to patch.
#[cfg(not(all(target_arch = "x86_64", target_os = "none")))]
fn install_kernel_window(root_phys: u64) {
    let _ = root_phys;
}

/// Copy the published window entry into `pml4`'s slot.
///
/// Every root constructor calls this, so a space built before *or* after
/// the reservation ends up with the window (the boot root is patched in
/// place by [`reserve_kernel_window`]). A not-present-to-present entry
/// needs no TLB maintenance: the CPU never caches an absent translation.
fn install_kernel_window_slot(pml4: &mut [u64; ENTRIES_PER_TABLE]) {
    let entry = KERNEL_WINDOW_PML4.load(Ordering::Acquire);
    if entry != 0 {
        pml4[KERNEL_WINDOW_PML4_SLOT] = entry;
    }
}

/// The permanent kernel translation root a CPU parks on whenever it must
/// leave a user root — published set-once by the first
/// `AddressSpace::switch` (the boot space, whose tables live for the
/// image's lifetime), read by [`park_kernel_root`]. `0` means "not yet
/// published" (the boot space's PML4 is never at physical 0).
static PARK_ROOT: AtomicU64 = AtomicU64::new(0);

/// Park the calling CPU's translation regime on the published boot
/// kernel root, so no user space's root remains active after its task
/// suspends or exits. Returns `false`, changing nothing, when no park
/// root has been published yet (fail closed).
///
/// The dispatcher calls this after every switch-back from a user task;
/// address-space teardown calls it defensively before dismantling a root
/// that is somehow still active.
pub fn park_kernel_root() -> bool {
    let root = PARK_ROOT.load(Ordering::Acquire);
    if root == 0 {
        return false;
    }
    // SAFETY: the published root is the boot space's, which maps the low
    // identity window and the higher-half kernel window for the image's
    // lifetime — exactly `activate_user_root`'s contract (inert on the
    // host, where the root is never published anyway).
    unsafe { activate_user_root(root) };
    true
}

/// Publish the calling CPU's *current* translation root — the boot
/// trampoline's `CR3` tables, which live in permanent kernel storage
/// (`boot.s`) — as the park root, set-once.
///
/// The x86_64 boot never activates a Rust-built kernel `AddressSpace`
/// (it keeps running on the trampoline tables), so — unlike
/// aarch64/riscv64, where the boot space's `switch()` publishes — the
/// boot path calls this once on the BSP before any user space can be
/// spawned; a later `switch()` to a per-process space then cannot claim
/// the slot.
#[cfg(all(target_arch = "x86_64", target_os = "none"))]
pub fn publish_boot_park_root() {
    let _ = PARK_ROOT.compare_exchange(0, active_root_phys(), Ordering::AcqRel, Ordering::Relaxed);
}

/// Host substitute: there is no boot trampoline `CR3` on the host; the
/// park root stays unpublished and [`park_kernel_root`] reports `false`.
#[cfg(not(all(target_arch = "x86_64", target_os = "none")))]
pub fn publish_boot_park_root() {}

/// The physical root of the calling CPU's active translation regime
/// (`CR3`'s table base, PCID/flag bits masked off).
#[cfg(all(target_arch = "x86_64", target_os = "none"))]
fn active_root_phys() -> u64 {
    let cr3: u64;
    // SAFETY: reading `CR3` observes the active root without side
    // effects; no Rust spelling exists for the control register.
    unsafe {
        core::arch::asm!("mov {v}, cr3", v = out(reg) cr3, options(nostack, preserves_flags, nomem));
    }
    cr3 & !0xFFF
}

/// Reactivate `root_phys` as the active top-level translation root (load
/// `CR3`) on a CPU whose paging is already enabled.
///
/// This is the X1 user-kthread `pre_resume` primitive (`plans/PI.md` §X),
/// the x86_64 sibling of the aarch64 `activate_user_root`: immediately
/// before the kernel returns into a user task's ring 3, that task's own
/// PML4 must be installed so its translations — and only its — are in
/// force, keeping sibling processes hardware-isolated. It
/// takes only the `u64` root, so the per-task hook that calls it captures a
/// plain word and stays `Send`.
///
/// Unlike a full mode switch this only reloads `CR3`: the rest of the
/// paging configuration (`EFER.NXE`, `CR0`/`CR4` paging controls) is
/// already in force and identical across user spaces, and only the
/// top-level root changes between them. Loading `CR3` flushes the
/// non-global TLB entries as a side effect (Intel SDM Vol 3A §4.10.4), so
/// no explicit invalidation is needed.
///
/// # Safety
///
/// Paging must already be enabled, and the PML4 at `root_phys` must map the
/// currently-executing kernel `rip`, `rsp`, and the data the code touches
/// (the per-CPU `swapgs` TLS, the dispatcher's stack) identically to the
/// outgoing root — every TAIRiX user space maps the low identity window and
/// the higher-half kernel window, so this holds for any task root, but a
/// `root_phys` that does not faults the CPU on its next access.
#[cfg(all(target_arch = "x86_64", target_os = "none"))]
pub unsafe fn activate_user_root(root_phys: u64) {
    // SAFETY: `mov cr3, _` swaps the active translation root and flushes the
    // non-global TLB entries; it touches no memory and no Rust spelling
    // exists for `CR3`. The caller's contract guarantees the new root covers
    // the running kernel context (see the `# Safety` paragraph above).
    unsafe {
        core::arch::asm!(
            "mov cr3, {root}",
            root = in(reg) root_phys,
            options(nostack, preserves_flags),
        );
    }
}

/// Host substitute: reloading `CR3` is meaningful only on the bare-metal
/// x86_64 target. Never linked into a kernel image and never reached on the
/// host (the QEMU verticals exercise the real reload).
///
/// # Safety
///
/// Carries the same contract as the bare-metal definition above (paging
/// enabled; `root_phys` maps the running kernel context), so the two `cfg`
/// arms present one `unsafe` API. The host body is inert.
#[cfg(not(all(target_arch = "x86_64", target_os = "none")))]
pub unsafe fn activate_user_root(root_phys: u64) {
    let _ = root_phys;
}

impl MmuAddressSpace for AddressSpace {
    fn map_page(&mut self, vaddr: u64, paddr: u64, flags: PageFlags) -> Result<(), MapError> {
        if (vaddr & (PAGE_SIZE as u64 - 1)) != 0 || (paddr & (PAGE_SIZE as u64 - 1)) != 0 {
            return Err(MapError::Misaligned);
        }
        if flags.is_write_exec() {
            return Err(MapError::InvalidFlags);
        }
        if flags.contains(PageFlags::WRITE_COMBINE) {
            return Err(MapError::Unsupported);
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
            let user = flags.contains(PageFlags::USER);
            let executable = flags.contains(PageFlags::EXEC);
            let memory_attrs = if flags.contains(PageFlags::DEVICE) {
                flags::CACHE_DISABLE | flags::WRITE_THROUGH
            } else {
                0
            };
            let result = self.map_4k_inner(
                frames,
                vaddr,
                paddr,
                LeafPolicy {
                    writable,
                    user,
                    no_execute: !executable,
                    memory_attrs,
                },
            );
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

    fn access_tracking(&self) -> tairix_arch_api::mmu::AccessTracking {
        // x86_64 has an unconditional hardware referenced bit: the CPU
        // sets the leaf PTE's Accessed bit (bit 5) on the first access and
        // never clears it itself (Intel SDM Vol 3A §4.8), so a clock scan
        // can read and clear it with no software fault path — unlike the
        // aarch64 / riscv64 ports, whose access flag needs a software
        // access-flag-fault handler on parts that do not update it in the
        // page walk. Supported on every x86_64 CPU TAIRiX targets.
        tairix_arch_api::mmu::AccessTracking::Supported
    }

    fn test_and_clear_accessed(&mut self, vaddr: u64) -> Result<bool, MapError> {
        if (vaddr & (PAGE_SIZE as u64 - 1)) != 0 {
            return Err(MapError::Misaligned);
        }
        #[cfg(all(target_arch = "x86_64", target_os = "none"))]
        {
            let i4 = ((vaddr >> 39) & 0x1FF) as usize;
            let i3 = ((vaddr >> 30) & 0x1FF) as usize;
            let i2 = ((vaddr >> 21) & 0x1FF) as usize;
            let i1 = ((vaddr >> 12) & 0x1FF) as usize;
            // Navigate to the 4 KiB PT leaf without allocating, exactly as
            // `unmap` does. A missing level or a huge-page leaf means there
            // is no 4 KiB leaf whose referenced bit this reports — fail
            // closed with `NotMapped` (the tier tracks only 4 KiB
            // anonymous leaves, never a huge block).
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
            let was_accessed = e1 & flags::ACCESSED != 0;
            if was_accessed {
                // Clear the Accessed bit so the CPU re-sets it on the next
                // touch; a later probe reading it still clear proves the
                // page went untouched in between (the clock scan).
                pt[i1] = e1 & !flags::ACCESSED;
                // The stale TLB entry may still permit an access without a
                // page-walk (and so without re-setting Accessed), so the
                // cleared bit only becomes observable once the TLB entry is
                // invalidated: flush this page on the current CPU.
                self.flush_page(vaddr);
            }
            Ok(was_accessed)
        }
        #[cfg(not(all(target_arch = "x86_64", target_os = "none")))]
        {
            let _ = vaddr;
            unreachable!("the x86_64 page-table walk is only valid on the bare-metal target")
        }
    }

    fn block_split_support(&self) -> BlockSplit {
        // The four-level walk re-expresses a 1 GiB (PDPTE) / 2 MiB (PDE)
        // huge leaf as a table of finer leaves (`plans/PI.md` G1/G2 — the
        // x86_64 guard-page fault-form foundation, proven on the production
        // pipeline by `stack_guard_qemu_x86_64`), the sibling of the
        // aarch64 / riscv64 block split.
        BlockSplit::Supported
    }

    fn split_block(&mut self, vaddr: u64) -> Result<(), MapError> {
        // The HAL view of the inherent, QEMU-proven
        // `AddressSpace::split_block` (G1): one body, reached either
        // directly by the arch boot path / verticals or through the HAL
        // trait here. Inherent methods take precedence
        // over a same-named trait method, so this forwards to the inherent
        // body rather than recursing into itself.
        #[cfg(all(target_arch = "x86_64", target_os = "none"))]
        {
            self.split_block(vaddr)
        }
        #[cfg(not(all(target_arch = "x86_64", target_os = "none")))]
        {
            let _ = vaddr;
            unreachable!("the x86_64 page-table walk is only valid on the bare-metal target")
        }
    }

    fn prepare_guard_arena(&mut self, base: u64, len: u64) -> Result<(), MapError> {
        // The HAL view of the inherent, QEMU-proven
        // `AddressSpace::prepare_guard_arena` (G2): one body, reached either
        // directly or through the HAL trait here. As with
        // `split_block`, inherent-method resolution forwards to the inherent
        // body rather than recursing.
        #[cfg(all(target_arch = "x86_64", target_os = "none"))]
        {
            self.prepare_guard_arena(base, len)
        }
        #[cfg(not(all(target_arch = "x86_64", target_os = "none")))]
        {
            let _ = (base, len);
            unreachable!("the x86_64 page-table walk is only valid on the bare-metal target")
        }
    }

    unsafe fn reclaim_table_frames(&mut self) {
        // The four-level walk recovers tables through the low identity
        // map, so — exactly like `map_page` — it is only meaningful on the
        // bare-metal target; a host space never maps anything to reclaim.
        #[cfg(all(target_arch = "x86_64", target_os = "none"))]
        {
            // Defence in depth: the dispatcher parks a CPU off a user root
            // at every task suspend, so a dead space's root is never the
            // active translation here — but freeing the walked-from root
            // of a live regime would be catastrophic, so verify and
            // re-park first. With no park root published the frames are
            // retired unreclaimed rather than dismantling the active
            // translation (fail closed).
            if active_root_phys() == self.pml4_phys && !park_kernel_root() {
                return;
            }
            // The kernel remap window's PML4 entry points at a PDPT
            // *every* root shares, not at a table this hierarchy owns, and
            // the walk below cannot tell the two apart — it would free the
            // live kernel heap's page tables. Drop it from this root first;
            // the window itself is permanent and is reached through every
            // other root unchanged.
            self.pml4[KERNEL_WINDOW_PML4_SLOT] = 0;
            let frames = self.frames;
            // A four-level hierarchy rooted at the PML4: a present PML4
            // entry always points at a PDPT; a present PDPT/PD entry
            // without `HUGE` points at the next table; PT (depth 3)
            // entries are page leaves and are never descended into.
            let child_of = |entry: u64, depth: usize| -> Option<u64> {
                (depth < 3
                    && (entry & flags::PRESENT) != 0
                    && (depth == 0 || (entry & flags::HUGE) == 0))
                    .then_some(entry & ADDR_MASK)
            };
            // Tables are recovered from their physical address through the
            // low identity map — the same round-trip `leaf_present` and
            // `ensure_child` rely on.
            let entries_of = |phys: u64| phys as *const [u64; ENTRIES_PER_TABLE];
            // SAFETY: every phys `child_of` yields was written by
            // `ensure_child` / `new_identity` from a `TableFrame` of
            // `self.frames`, so it names a live, identity-reachable table
            // this hierarchy owns; the guard above upholds the not-active
            // contract the caller asserts, and `self` is borrowed mutably
            // so no other reference walks the tables.
            unsafe {
                tairix_arch_api::frames::reclaim_hierarchy(
                    self.pml4_phys,
                    &child_of,
                    &entries_of,
                    &mut |phys| {
                        frames.free_table(phys);
                    },
                );
            }
        }
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

    fn publish_mappings(&mut self, start_vaddr: u64, page_count: usize) {
        // Nothing is owed. A not-present paging-structure entry is never
        // cached (Intel SDM Vol 3A, "Caching Translation Information"), so
        // installing a leaf leaves no stale translation to discard, and the
        // store is already ordered ahead of the walk that reads it. The
        // default's per-page `invlpg` sweep would be pure waste.
        let _ = (start_vaddr, page_count);
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
    if pte & (flags::CACHE_DISABLE | flags::WRITE_THROUGH)
        == (flags::CACHE_DISABLE | flags::WRITE_THROUGH)
    {
        out = out | PageFlags::DEVICE;
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

/// Populate the freshly-allocated table `child` with 512 entries that
/// reproduce the huge leaf `block` at the next finer granularity,
/// preserving every permission/attribute bit.
///
/// `sub_shift` is the base-2 log of each sub-entry's coverage (21 for the
/// 2 MiB huge pages a 1 GiB leaf shatters into, 12 for the 4 KiB pages a
/// 2 MiB leaf shatters into). `keep_huge` carries the page-size bit:
/// shattering a 1 GiB PDPTE leaf yields 2 MiB PD leaves that are *still*
/// huge (PD `PS = 1`), but shattering a 2 MiB PD leaf yields 4 KiB PT
/// leaves where bit 7 ([`flags::HUGE`]) is the PAT attribute rather than a
/// page-size flag (Intel SDM Vol 3A §4.5), so it is cleared. The address
/// base is masked to the parent block's natural alignment so a stray PAT
/// bit on the source leaf never bleeds into a sub-entry's frame address
/// (one attribute decode, never re-derived).
///
/// Only compiled on the bare-metal target, where the page-table walk runs.
#[cfg(all(target_arch = "x86_64", target_os = "none"))]
fn shatter_huge_into(
    child: &mut [u64; ENTRIES_PER_TABLE],
    block: u64,
    sub_shift: u32,
    keep_huge: bool,
) {
    let sub_size = 1u64 << sub_shift;
    // The parent block spans 512 sub-entries; align the base to that span
    // so PAT / reserved bits below the parent page size are discarded.
    let region_size = sub_size << 9;
    let base = (block & ADDR_MASK) & !(region_size - 1);
    let mut attr = block & !ADDR_MASK;
    if !keep_huge {
        attr &= !flags::HUGE;
    }
    for (i, slot) in child.iter_mut().enumerate() {
        let sub_pa = base + (i as u64) * sub_size;
        *slot = sub_pa | attr;
    }
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
        // A present *huge* leaf is a data page, not a table: dereferencing
        // its address as one would let a mapping walk scribble page-table
        // entries over mapped memory. Refuse, so the caller fails closed
        // and the coarse leaf is split explicitly instead.
        if entry & flags::HUGE != 0 {
            return None;
        }
        // Existing child — recover the `&mut` from the physical address.
        // Identity mapping makes phys = virt here.
        let phys = entry & ADDR_MASK;
        // SAFETY: every entry that has PRESENT set was inserted below (or
        // by a `new_identity_*` constructor) with a physical address that came
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
    // This stub exists so `cargo test -p tairix-arch-x86_64` runs cleanly
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
