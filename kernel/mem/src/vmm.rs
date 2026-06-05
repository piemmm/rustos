//! Virtual-memory manager — per-process address space.
//!
//! `kernel/mem` is architecture-neutral. The actual page-table format
//! (4-level x86_64, 4-level aarch64, Sv39 riscv64, WASM linear memory)
//! is implemented in `kernel/arch/*` and plugged in through the Arch HAL
//! page-table surface — [`rustos_arch_api::mmu::AddressSpace`] for the
//! map / translate / unmap walk and [`rustos_arch_api::tlb::TlbShootdown`]
//! for the per-page TLB invalidation. `kernel/mem` names **only** those
//! HAL traits; it no longer defines its own page-table trait
//! (`AGENTS.md` §2.2 — one vocabulary; `plans/WIRING.md` Stage W5b-2).
//!
//! [`AddressSpace`] is a *thin* generic façade over a port's HAL
//! [`rustos_arch_api::mmu::AddressSpace`] (`+ TlbShootdown`)
//! implementation. It owns the page-table object, exposes
//! capability-checked map / unmap / translate operations in
//! `kernel/mem`'s own [`Page`] / [`Frame`] / [`MapFlags`] currency
//! (bridged to the HAL's `u64` / [`rustos_arch_api::mmu::PageFlags`]
//! vocabulary at the boundary), and tracks the high-level mapping ranges
//! (so leak-checking and double-mapping detection can be done
//! independently of the arch).
//!
//! # Host-testability
//!
//! A `HostPageTable` test double — a `BTreeMap<Page, (Frame, MapFlags)>` —
//! is provided under `#[cfg(test)]`. It exercises every public path of
//! [`AddressSpace`] on a developer workstation without ever touching a
//! real CPU page table.

use alloc::collections::BTreeMap;
use core::fmt;

use rustos_arch_api::mmu::{AddressSpace as HalAddressSpace, MapError, PageFlags};
use rustos_arch_api::tlb::TlbShootdown;

use crate::error::AllocError;
use crate::frame::{Frame, PhysAddr, PAGE_SHIFT, PAGE_SIZE};

// `bitflags_like!` — a tiny in-crate macro that synthesises just enough of
// the well-known `bitflags` crate to avoid adding a dependency for one
// type. It is defined here at the top of the module so the `MapFlags!`
// invocation below has it in scope.
macro_rules! bitflags_like {
    (
        $(#[$outer:meta])*
        $vis:vis struct $Name:ident($T:ty) {
            $( $(#[$inner:meta])* const $Flag:ident = $Value:expr; )*
        }
    ) => {
        $(#[$outer])*
        $vis struct $Name($T);

        impl $Name {
            $(
                $(#[$inner])*
                pub const $Flag: Self = Self($Value);
            )*

            /// The empty set.
            #[must_use]
            pub const fn empty() -> Self { Self(0) }

            /// `true` if `self` contains every bit in `other`.
            #[must_use]
            pub const fn contains(self, other: Self) -> bool {
                (self.0 & other.0) == other.0
            }

            /// Raw bit pattern.
            #[must_use]
            pub const fn bits(self) -> $T { self.0 }
        }

        impl core::ops::BitOr for $Name {
            type Output = Self;
            fn bitor(self, rhs: Self) -> Self { Self(self.0 | rhs.0) }
        }
        impl core::ops::BitAnd for $Name {
            type Output = Self;
            fn bitand(self, rhs: Self) -> Self { Self(self.0 & rhs.0) }
        }
    };
}

/// Virtual byte address.
///
/// Mirrors [`crate::PhysAddr`] for the virtual half of the world.
/// Keeping them distinct types prevents accidental cross-use in kernel
/// APIs.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct VirtAddr(u64);

impl VirtAddr {
    /// Wrap a raw `u64` as a virtual address.
    #[must_use]
    pub const fn new(addr: u64) -> Self {
        Self(addr)
    }

    /// The numeric value.
    #[must_use]
    pub const fn as_u64(self) -> u64 {
        self.0
    }

    /// `true` if this address is page-aligned.
    #[must_use]
    pub const fn is_page_aligned(self) -> bool {
        (self.0 & (PAGE_SIZE as u64 - 1)) == 0
    }
}

impl fmt::Display for VirtAddr {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{:#018x}", self.0)
    }
}

/// A page-aligned virtual address.
///
/// Pages always exist at frame granularity (4 KiB). Constructing a
/// `Page` from a misaligned [`VirtAddr`] is a programmer error and
/// surfaces as [`PageTableError::Misaligned`].
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct Page(VirtAddr);

impl Page {
    /// Create a [`Page`] from a page-aligned virtual address.
    ///
    /// # Errors
    ///
    /// [`PageTableError::Misaligned`] if `addr` is not page-aligned.
    pub fn from_addr(addr: VirtAddr) -> Result<Self, PageTableError> {
        if addr.is_page_aligned() {
            Ok(Self(addr))
        } else {
            Err(PageTableError::Misaligned)
        }
    }

    /// Start address of the page.
    #[must_use]
    pub fn start(self) -> VirtAddr {
        self.0
    }

    /// Page number (`addr >> 12`).
    #[must_use]
    pub fn number(self) -> u64 {
        self.0.as_u64() >> PAGE_SHIFT
    }
}

bitflags_like! {
    /// Permission and attribute bits a page may carry.
    ///
    /// This is an architecture-neutral subset of the bits every Tier-1
    /// MMU supports. Architecture crates translate these into native
    /// page-table entries during Stage 3.
    #[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
    pub struct MapFlags(u8) {
        /// Page is readable.
        const READ      = 0b0000_0001;
        /// Page is writable.
        const WRITE     = 0b0000_0010;
        /// Page is executable.
        const EXEC      = 0b0000_0100;
        /// Page is accessible from ring 3 / EL0 / U-mode.
        const USER      = 0b0000_1000;
        /// Page is mapped with caching disabled (MMIO).
        const NO_CACHE  = 0b0001_0000;
    }
}

/// Errors a page-table operation may report.
///
/// These are *distinct* from [`AllocError`] because the caller wants to
/// distinguish "the operation itself was malformed" from "we ran out of
/// memory while servicing it".
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
#[non_exhaustive]
pub enum PageTableError {
    /// The address was not page-aligned.
    Misaligned,
    /// The page is already mapped (caller must unmap first).
    AlreadyMapped,
    /// The page is not currently mapped.
    NotMapped,
    /// The flags are inconsistent (e.g. `EXEC | WRITE` for a process
    /// that requested W^X). Defensive — the trait implementation may
    /// also use this to surface MMU constraints.
    InvalidFlags,
    /// The underlying allocator failed.
    AllocFailed(AllocError),
}

impl From<AllocError> for PageTableError {
    fn from(e: AllocError) -> Self {
        Self::AllocFailed(e)
    }
}

/// The page-table backend a [`AddressSpace`] drives.
///
/// `kernel/mem` no longer defines its own page-table trait: the backend
/// is exactly the Arch HAL [`rustos_arch_api::mmu::AddressSpace`] (the
/// map / translate / unmap walk) plus [`TlbShootdown`] (the per-page
/// invalidation the map/unmap path issues), held together by this alias
/// so the bound is written once (`AGENTS.md` §2.2 / §2.3). Every
/// architecture port implements both traits; the in-crate
/// `HostPageTable` double implements them too so the façade stays
/// host-testable.
pub trait PageTable: HalAddressSpace + TlbShootdown {}

impl<T: HalAddressSpace + TlbShootdown> PageTable for T {}

/// Translate `kernel/mem`'s [`MapFlags`] into the HAL's neutral
/// [`PageFlags`] permission set (one decode at the boundary,
/// `AGENTS.md` §2.2). `kernel/mem`'s `NO_CACHE` is the HAL's `DEVICE`
/// (uncached / strongly-ordered) attribute.
fn to_page_flags(flags: MapFlags) -> PageFlags {
    let mut out = PageFlags::empty();
    if flags.contains(MapFlags::READ) {
        out = out | PageFlags::READ;
    }
    if flags.contains(MapFlags::WRITE) {
        out = out | PageFlags::WRITE;
    }
    if flags.contains(MapFlags::EXEC) {
        out = out | PageFlags::EXEC;
    }
    if flags.contains(MapFlags::USER) {
        out = out | PageFlags::USER;
    }
    if flags.contains(MapFlags::NO_CACHE) {
        out = out | PageFlags::DEVICE;
    }
    out
}

/// Inverse of [`to_page_flags`]: decode a HAL [`PageFlags`] leaf back
/// into `kernel/mem`'s [`MapFlags`] currency (`AGENTS.md` §2.2).
fn from_page_flags(flags: PageFlags) -> MapFlags {
    let mut out = MapFlags::empty();
    if flags.contains(PageFlags::READ) {
        out = out | MapFlags::READ;
    }
    if flags.contains(PageFlags::WRITE) {
        out = out | MapFlags::WRITE;
    }
    if flags.contains(PageFlags::EXEC) {
        out = out | MapFlags::EXEC;
    }
    if flags.contains(PageFlags::USER) {
        out = out | MapFlags::USER;
    }
    if flags.contains(PageFlags::DEVICE) {
        out = out | MapFlags::NO_CACHE;
    }
    out
}

/// Map a HAL [`MapError`] onto `kernel/mem`'s [`PageTableError`] at the
/// boundary. A pool-exhaustion failure becomes the allocator's
/// [`AllocError::OutOfMemory`], so callers see one OOM type
/// (`AGENTS.md` §2.2).
fn from_map_error(err: MapError) -> PageTableError {
    match err {
        MapError::Misaligned => PageTableError::Misaligned,
        MapError::AlreadyMapped => PageTableError::AlreadyMapped,
        MapError::InvalidFlags => PageTableError::InvalidFlags,
        MapError::PoolExhausted => PageTableError::AllocFailed(AllocError::OutOfMemory),
        MapError::NotMapped => PageTableError::NotMapped,
    }
}

/// Per-process virtual address space.
///
/// `AddressSpace` is generic over `P: PageTable` — the Arch HAL
/// page-table backend (a port's [`rustos_arch_api::mmu::AddressSpace`]
/// `+ TlbShootdown`). The arch crates provide one (their `paging`
/// `AddressSpace`); this crate's tests use `HostPageTable`.
pub struct AddressSpace<P: PageTable> {
    table: P,
    /// Live mappings observed by this layer.
    ///
    /// Mirrors the trait's state for accounting / leak-detection and to
    /// give `AddressSpace` a useful `Drop`. Kept page-keyed
    /// independently of the underlying table for cheap iteration.
    live: BTreeMap<Page, MapFlags>,
}

impl<P: PageTable> AddressSpace<P> {
    /// Construct a new address space wrapping `table`.
    ///
    /// The supplied table is expected to be empty. We do not assert it,
    /// because Stage-3 arch implementations may pre-install kernel
    /// fixed mappings before handing the table off.
    #[must_use]
    pub fn new(table: P) -> Self {
        Self {
            table,
            live: BTreeMap::new(),
        }
    }

    /// Map `page` → `frame`, recording the mapping and flushing the
    /// page's stale TLB entry on the calling CPU.
    ///
    /// # Errors
    ///
    /// [`PageTableError::AlreadyMapped`] if this layer already records a
    /// mapping for `page`; otherwise propagates the bridged HAL
    /// [`MapError`] from [`rustos_arch_api::mmu::AddressSpace::map_page`].
    pub fn map(&mut self, page: Page, frame: Frame, flags: MapFlags) -> Result<(), PageTableError> {
        if self.live.contains_key(&page) {
            return Err(PageTableError::AlreadyMapped);
        }
        let vaddr = page.start().as_u64();
        self.table
            .map_page(vaddr, frame.start().as_u64(), to_page_flags(flags))
            .map_err(from_map_error)?;
        self.live.insert(page, flags);
        self.table.flush_page(vaddr);
        Ok(())
    }

    /// Tear down the mapping at `page` and return the frame that was
    /// mapped there, flushing the page's TLB entry on the calling CPU.
    ///
    /// # Errors
    ///
    /// [`PageTableError::NotMapped`] if `page` has no live mapping.
    pub fn unmap(&mut self, page: Page) -> Result<Frame, PageTableError> {
        let vaddr = page.start().as_u64();
        let paddr = self.table.unmap(vaddr).map_err(from_map_error)?;
        self.live.remove(&page);
        self.table.flush_page(vaddr);
        Ok(Frame::containing(PhysAddr::new(paddr)))
    }

    /// Translate `page` to `(frame, flags)`, or `None` if unmapped.
    #[must_use]
    pub fn translate(&self, page: Page) -> Option<(Frame, MapFlags)> {
        self.table
            .translate(page.start().as_u64())
            .map(|(paddr, flags)| {
                (
                    Frame::containing(PhysAddr::new(paddr)),
                    from_page_flags(flags),
                )
            })
    }

    /// Number of live mappings in this address space.
    #[must_use]
    pub fn mapped_pages(&self) -> usize {
        self.live.len()
    }

    /// Borrow the underlying page table immutably. Provided so arch
    /// code can inspect (e.g. to dump for a crash report). Mutable
    /// access is intentionally not exposed: mapping must go through
    /// the bookkeeping in [`Self::map`] / [`Self::unmap`].
    #[must_use]
    pub fn table(&self) -> &P {
        &self.table
    }
}

/// Object-safe, read-only view of a task's user address space.
///
/// [`AddressSpace`] is generic over its [`PageTable`] backend, so the
/// kernel cannot hold one address space per task — each potentially a
/// different architecture's page table — behind a single concrete type.
/// `UserAddressSpace` erases that backend down to the *one* operation the
/// [`crate::uaccess`] page walk needs: translating a [`Page`] to its
/// backing `(`[`Frame`]`, `[`MapFlags`]`)`. A per-task registry (the
/// kernel orchestrator in `kernel/core`) keys `dyn UserAddressSpace`
/// values by task id and hands the user-memory copy path a
/// `&dyn UserAddressSpace` to walk (`AGENTS.md` §5.4 /
/// `tests/SECURITY.md` §5).
///
/// The trait deliberately exposes **only** `translate`: the copy path
/// must never be able to *mutate* a caller's mappings, and a read-only
/// translation is all the fail-closed permission checks in
/// [`crate::uaccess`] require. Mapping and unmapping stay behind
/// [`AddressSpace`]'s own accounted [`AddressSpace::map`] /
/// [`AddressSpace::unmap`] (`AGENTS.md` §2.4 — no widening of the
/// interface to "make access easier").
pub trait UserAddressSpace {
    /// Translate `page` to its backing `(frame, flags)`, or `None` when
    /// the page is not currently mapped.
    ///
    /// Mirrors [`AddressSpace::translate`] exactly; the blanket impl
    /// forwards to it so there is one translation definition, not two
    /// (`AGENTS.md` §2.2).
    fn translate(&self, page: Page) -> Option<(Frame, MapFlags)>;
}

impl<P: PageTable> UserAddressSpace for AddressSpace<P> {
    fn translate(&self, page: Page) -> Option<(Frame, MapFlags)> {
        AddressSpace::translate(self, page)
    }
}

// ---------------------------------------------------------------------------
// Host test double.
// ---------------------------------------------------------------------------

/// A pure-software page table used only in unit and integration tests.
///
/// It tracks `vaddr → (paddr, `[`PageFlags`]`)` entries in a
/// [`BTreeMap`], implementing the Arch HAL
/// [`rustos_arch_api::mmu::AddressSpace`] + [`TlbShootdown`] surface in
/// pure software. No CPU page-table writes happen. The point is to
/// exercise every code path in [`AddressSpace`] on host hardware
/// (`AGENTS.md` §7 — all algorithms that do not need hardware must be
/// host-tested).
///
/// Visible to downstream crates only behind the `host-tests` cargo
/// feature so production kernel builds never link the test double.
#[cfg(any(test, feature = "host-tests"))]
#[derive(Debug, Default)]
pub struct HostPageTable {
    entries: BTreeMap<u64, (u64, PageFlags)>,
    /// Counts how many times [`TlbShootdown::flush_page`] has been
    /// called, so tests can assert the TLB-flush discipline is correct.
    pub(crate) flush_count: usize,
}

#[cfg(any(test, feature = "host-tests"))]
impl HostPageTable {
    /// Construct an empty host page table.
    #[must_use]
    pub fn new() -> Self {
        Self {
            entries: BTreeMap::new(),
            flush_count: 0,
        }
    }
}

#[cfg(any(test, feature = "host-tests"))]
impl HalAddressSpace for HostPageTable {
    fn map_page(&mut self, vaddr: u64, paddr: u64, flags: PageFlags) -> Result<(), MapError> {
        if vaddr & (PAGE_SIZE as u64 - 1) != 0 || paddr & (PAGE_SIZE as u64 - 1) != 0 {
            return Err(MapError::Misaligned);
        }
        // RustOS's default leaf policy is W^X (`AGENTS.md` §19.2): a
        // simultaneous write+exec leaf is refused, mirroring what a real
        // port does at the HAL boundary.
        if flags.is_write_exec() {
            return Err(MapError::InvalidFlags);
        }
        if self.entries.contains_key(&vaddr) {
            return Err(MapError::AlreadyMapped);
        }
        self.entries.insert(vaddr, (paddr, flags));
        Ok(())
    }

    fn translate(&self, vaddr: u64) -> Option<(u64, PageFlags)> {
        self.entries.get(&vaddr).copied()
    }

    fn unmap(&mut self, vaddr: u64) -> Result<u64, MapError> {
        if vaddr & (PAGE_SIZE as u64 - 1) != 0 {
            return Err(MapError::Misaligned);
        }
        let (paddr, _) = self.entries.remove(&vaddr).ok_or(MapError::NotMapped)?;
        Ok(paddr)
    }

    fn root_phys(&self) -> u64 {
        // The double has no real root table; a non-zero sentinel keeps
        // it honouring the `root_phys` contract (non-null once built).
        PAGE_SIZE as u64
    }

    unsafe fn activate(&self) {
        // The host double never activates a translation regime; the
        // façade only ever calls map/translate/unmap on it.
    }
}

#[cfg(any(test, feature = "host-tests"))]
impl TlbShootdown for HostPageTable {
    fn flush_page(&mut self, _vaddr: u64) {
        self.flush_count += 1;
    }
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(all(test, not(loom)))]
mod tests {
    use super::*;

    fn p(n: u64) -> Page {
        Page::from_addr(VirtAddr::new(n * PAGE_SIZE as u64)).unwrap()
    }

    #[test]
    fn page_rejects_misaligned() {
        assert!(matches!(
            Page::from_addr(VirtAddr::new(0x1234)),
            Err(PageTableError::Misaligned)
        ));
    }

    #[test]
    fn map_then_translate_round_trip() {
        let mut s = AddressSpace::new(HostPageTable::new());
        s.map(p(1), Frame(7), MapFlags::READ | MapFlags::WRITE)
            .unwrap();
        let (f, fl) = s.translate(p(1)).unwrap();
        assert_eq!(f, Frame(7));
        assert!(fl.contains(MapFlags::READ));
        assert!(fl.contains(MapFlags::WRITE));
        assert!(!fl.contains(MapFlags::EXEC));
    }

    #[test]
    fn double_map_rejected() {
        let mut s = AddressSpace::new(HostPageTable::new());
        s.map(p(1), Frame(7), MapFlags::READ).unwrap();
        let e = s.map(p(1), Frame(8), MapFlags::READ).unwrap_err();
        assert!(matches!(e, PageTableError::AlreadyMapped));
    }

    #[test]
    fn unmap_returns_frame() {
        let mut s = AddressSpace::new(HostPageTable::new());
        s.map(p(2), Frame(11), MapFlags::READ).unwrap();
        let f = s.unmap(p(2)).unwrap();
        assert_eq!(f, Frame(11));
        assert!(s.translate(p(2)).is_none());
    }

    #[test]
    fn unmap_not_mapped() {
        let mut s = AddressSpace::new(HostPageTable::new());
        assert!(matches!(s.unmap(p(5)), Err(PageTableError::NotMapped)));
    }

    #[test]
    fn wxor_x_enforced_by_host_double() {
        let mut s = AddressSpace::new(HostPageTable::new());
        let e = s
            .map(p(0), Frame(0), MapFlags::WRITE | MapFlags::EXEC)
            .unwrap_err();
        assert!(matches!(e, PageTableError::InvalidFlags));
    }

    #[test]
    fn flush_called_on_map_and_unmap() {
        let mut s = AddressSpace::new(HostPageTable::new());
        s.map(p(3), Frame(3), MapFlags::READ).unwrap();
        s.unmap(p(3)).unwrap();
        assert_eq!(s.table().flush_count, 2);
    }

    #[test]
    fn mapped_pages_accounting() {
        let mut s = AddressSpace::new(HostPageTable::new());
        assert_eq!(s.mapped_pages(), 0);
        s.map(p(0), Frame(0), MapFlags::READ).unwrap();
        s.map(p(1), Frame(1), MapFlags::READ).unwrap();
        assert_eq!(s.mapped_pages(), 2);
        s.unmap(p(0)).unwrap();
        assert_eq!(s.mapped_pages(), 1);
    }

    #[test]
    fn map_flags_bit_ops_compose() {
        let f = MapFlags::READ | MapFlags::WRITE | MapFlags::USER;
        assert!(f.contains(MapFlags::READ));
        assert!(f.contains(MapFlags::USER));
        assert!(!f.contains(MapFlags::EXEC));
        let masked = f & MapFlags::WRITE;
        assert_eq!(masked.bits(), MapFlags::WRITE.bits());
    }

    #[test]
    fn allocerror_converts_into_pagetableerror() {
        let e: PageTableError = AllocError::OutOfMemory.into();
        assert!(matches!(
            e,
            PageTableError::AllocFailed(AllocError::OutOfMemory)
        ));
    }

    #[test]
    fn virt_addr_alignment_helper() {
        assert!(VirtAddr::new(0).is_page_aligned());
        assert!(VirtAddr::new(PAGE_SIZE as u64).is_page_aligned());
        assert!(!VirtAddr::new(1).is_page_aligned());
    }

    #[test]
    fn user_address_space_trait_object_forwards_translate() {
        let mut s = AddressSpace::new(HostPageTable::new());
        s.map(p(4), Frame(9), MapFlags::READ | MapFlags::USER)
            .unwrap();
        // Erase the concrete `HostPageTable` backend: the registry that
        // composes this increment stores `dyn UserAddressSpace`.
        let erased: &dyn UserAddressSpace = &s;
        let (f, fl) = erased.translate(p(4)).expect("mapped page resolves");
        assert_eq!(f, Frame(9));
        assert!(fl.contains(MapFlags::READ));
        assert!(fl.contains(MapFlags::USER));
        assert!(erased.translate(p(5)).is_none());
    }
}
