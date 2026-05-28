//! Virtual-memory manager — per-process address space.
//!
//! `kernel/mem` is architecture-neutral. The actual page-table format
//! (4-level x86_64, 4-level aarch64, Sv48 riscv64, WASM linear memory)
//! is implemented in `kernel/arch/*` (Stage 3) and plugged in through
//! the [`PageTableOps`] trait.
//!
//! [`AddressSpace`] is a *thin* generic façade over a [`PageTableOps`]
//! implementation. It owns the page-table object, exposes
//! capability-checked map / unmap / translate operations, and tracks
//! the high-level mapping ranges (so leak-checking and double-mapping
//! detection can be done independently of the arch).
//!
//! # Host-testability
//!
//! A `HostPageTable` test double — a `BTreeMap<Page, (Frame, MapFlags)>` —
//! is provided under `#[cfg(test)]`. It exercises every public path of
//! [`AddressSpace`] on a developer workstation without ever touching a
//! real CPU page table.

use alloc::collections::BTreeMap;
use core::fmt;

use crate::error::AllocError;
use crate::frame::{Frame, PAGE_SHIFT, PAGE_SIZE};

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

/// Architecture trait the Stage 3 crates implement.
///
/// Every method takes `&mut self` because, in a real implementation,
/// writing a page-table entry requires exclusive access to the table
/// and a TLB flush. The trait is **not** marked `Send` / `Sync` — the
/// containing [`AddressSpace`] is responsible for serialising callers
/// (today via the type system, tomorrow via a `SpinLock`).
pub trait PageTableOps {
    /// Map `page` → `frame` with `flags`.
    ///
    /// # Errors
    ///
    /// - [`PageTableError::AlreadyMapped`] if `page` is already in use.
    /// - [`PageTableError::InvalidFlags`] if the flags combination is
    ///   not representable.
    /// - [`PageTableError::AllocFailed`] for an underlying OOM while
    ///   allocating page-table levels.
    fn map(&mut self, page: Page, frame: Frame, flags: MapFlags) -> Result<(), PageTableError>;

    /// Tear down the mapping for `page` and return the frame that was
    /// mapped there.
    ///
    /// # Errors
    ///
    /// [`PageTableError::NotMapped`] if `page` has no live mapping.
    fn unmap(&mut self, page: Page) -> Result<Frame, PageTableError>;

    /// Translate `page` to `(frame, flags)`.
    ///
    /// Returns `None` if `page` is not currently mapped. This is
    /// deliberately not an error: read-only translation is the most
    /// common operation and the caller typically has fallback logic.
    fn translate(&self, page: Page) -> Option<(Frame, MapFlags)>;

    /// Flush the TLB entry for `page` on the current CPU. The trait
    /// default is `()`: the host test double has no TLB. Architecture
    /// implementations override this with an `invlpg` / `tlbi` / sfence
    /// as appropriate.
    fn flush(&mut self, _page: Page) {}
}

/// Per-process virtual address space.
///
/// `AddressSpace` is generic over `P: PageTableOps`. The arch crates
/// provide one (`X86PageTable`, `Aarch64PageTable`, …); this crate's
/// tests use `HostPageTable`.
pub struct AddressSpace<P: PageTableOps> {
    table: P,
    /// Live mappings observed by this layer.
    ///
    /// Mirrors the trait's state for accounting / leak-detection and to
    /// give `AddressSpace` a useful `Drop`. Kept page-keyed
    /// independently of the underlying table for cheap iteration.
    live: BTreeMap<Page, MapFlags>,
}

impl<P: PageTableOps> AddressSpace<P> {
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

    /// Map `page` → `frame`, recording the mapping.
    ///
    /// # Errors
    ///
    /// Propagates [`PageTableError`] from the underlying
    /// [`PageTableOps`].
    pub fn map(&mut self, page: Page, frame: Frame, flags: MapFlags) -> Result<(), PageTableError> {
        if self.live.contains_key(&page) {
            return Err(PageTableError::AlreadyMapped);
        }
        self.table.map(page, frame, flags)?;
        self.live.insert(page, flags);
        self.table.flush(page);
        Ok(())
    }

    /// Tear down the mapping at `page` and return the frame that was
    /// mapped there.
    ///
    /// # Errors
    ///
    /// [`PageTableError::NotMapped`] if no mapping is recorded.
    pub fn unmap(&mut self, page: Page) -> Result<Frame, PageTableError> {
        let frame = self.table.unmap(page)?;
        self.live.remove(&page);
        self.table.flush(page);
        Ok(frame)
    }

    /// Translate `page` to `(frame, flags)`, or `None` if unmapped.
    #[must_use]
    pub fn translate(&self, page: Page) -> Option<(Frame, MapFlags)> {
        self.table.translate(page)
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

// ---------------------------------------------------------------------------
// Host test double.
// ---------------------------------------------------------------------------

/// A pure-software page table used only in unit and integration tests.
///
/// It tracks `(Page, Frame, MapFlags)` triples in a [`BTreeMap`]. No
/// CPU page-table writes happen. The point is to exercise every code
/// path in [`AddressSpace`] on host hardware (`AGENTS.md` §7 — all
/// algorithms that do not need hardware must be host-tested).
#[cfg(test)]
#[derive(Debug, Default)]
pub struct HostPageTable {
    entries: BTreeMap<Page, (Frame, MapFlags)>,
    /// Counts how many times [`PageTableOps::flush`] has been called,
    /// so tests can assert the TLB-flush discipline is correct.
    pub(crate) flush_count: usize,
}

#[cfg(test)]
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

#[cfg(test)]
impl PageTableOps for HostPageTable {
    fn map(&mut self, page: Page, frame: Frame, flags: MapFlags) -> Result<(), PageTableError> {
        // Reject the obviously-bad combination W^X violation: a
        // simultaneous write+exec request. Real arches can do it but
        // RustOS's default policy is W^X (security default per
        // `AGENTS.md` §2.7).
        if flags.contains(MapFlags::WRITE) && flags.contains(MapFlags::EXEC) {
            return Err(PageTableError::InvalidFlags);
        }
        if self.entries.contains_key(&page) {
            return Err(PageTableError::AlreadyMapped);
        }
        self.entries.insert(page, (frame, flags));
        Ok(())
    }

    fn unmap(&mut self, page: Page) -> Result<Frame, PageTableError> {
        let (frame, _) = self
            .entries
            .remove(&page)
            .ok_or(PageTableError::NotMapped)?;
        Ok(frame)
    }

    fn translate(&self, page: Page) -> Option<(Frame, MapFlags)> {
        self.entries.get(&page).copied()
    }

    fn flush(&mut self, _page: Page) {
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
}
