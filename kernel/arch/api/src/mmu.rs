//! MMU / page-table surface of the Arch HAL (`AGENTS.md` §17.2
//! "MMU/page-table primitives").
//!
//! Mapping a virtual page to a physical frame, switching the active
//! translation regime, and reading the root-table physical address are
//! privilege-neutral but deeply architecture-specific: x86_64 walks a
//! four-level PML4 and loads `CR3`, riscv64 walks an Sv39 hierarchy and
//! writes `satp`, aarch64 walks a three-level stage-1 table and programs
//! `TTBR0_EL1` + `SCTLR_EL1.M`. §17.2 makes the architecture surface a
//! closed set of traits on the HAL; this module is the "MMU/page-table"
//! member of that set, so the page-table primitive lives behind one
//! vocabulary instead of being re-described at every call site
//! (`AGENTS.md` §2.2). The parallel per-arch implementations of this one
//! trait are the deliberate shape of §17.1/§17.2 modularity, never
//! collapsed behind `cfg` (§2.2 carve-out).
//!
//! # What lives here
//!
//! * [`PageFlags`] — the architecture-neutral permission/attribute set a
//!   page leaf carries. Each port translates it into its native
//!   page-table-entry bits at the HAL boundary (one neutral vocabulary,
//!   §2.2). The default policy is W^X (`AGENTS.md` §19.2): a leaf is
//!   never both [`PageFlags::WRITE`] and [`PageFlags::EXEC`].
//! * [`MapError`] — the fail-closed result of installing a mapping. A
//!   bad address or an exhausted page-table pool is rejected, never
//!   silently truncated or clobbered (`AGENTS.md` §2.9 / §5.4).
//! * [`AddressSpace`] — the per-port handle the kernel reaches through.
//!   It installs a 4 KiB mapping ([`AddressSpace::map_page`], host-testable
//!   walk/encoding math), activates the translation regime
//!   ([`AddressSpace::activate`], the port's privileged register write),
//!   and reports the root-table physical address
//!   ([`AddressSpace::root_phys`]).
//! * [`conformance`] — the §17.2 conformance vertical: a host-run
//!   [`conformance::run_all`] check every paging-capable port runs over
//!   its real [`AddressSpace`], proving the fail-closed `map_page`
//!   contract (a non-zero root, misaligned addresses rejected, a good
//!   mapping accepted, a double mapping refused).
//!
//! # Why `map_page` is host-tested but `activate` is not
//!
//! [`AddressSpace::map_page`] is pure page-table-walk and entry-encoding
//! arithmetic over a caller-supplied address pair, so it runs and is
//! asserted on the host exactly like the [`crate::context::conformance`]
//! vertical. [`AddressSpace::activate`] is only meaningful on the
//! bare-metal target — it loads a translation regime and the *next*
//! instruction fetch runs under it, which cannot be observed from
//! `cargo test` — so, like [`crate::ContextSwitch::switch`] and
//! [`crate::EnterUser::enter_user`], it carries no host conformance
//! check; it is proven end-to-end by each port's `memory_isolation`
//! QEMU vertical (two address spaces that disagree about one address,
//! and the CPU faults the one without the mapping). Inventing a host
//! stub that "activates" would be a fake primitive (`AGENTS.md` §1).
//!
//! # Scope (the §17.2 burn-down)
//!
//! This is the `plans/WIRING.md` **Stage W5b-1** slice: the bootstrap
//! page-table primitive every port already owns, lifted behind one HAL
//! trait and exercised by the `memory_isolation` verticals through it.
//! Wiring `kernel/mem`'s allocator-backed per-process address space onto
//! this trait, and the per-page + cross-CPU TLB shootdown (which depends
//! on the aarch64 IPI from Stage W6), are the tracked Stage W5b-2 / W6
//! follow-ups — not silently duplicated here (`AGENTS.md` §2.2).

/// The architecture-neutral permission/attribute set a 4 KiB page leaf
/// carries.
///
/// A neutral subset of the bits every Tier-1 MMU supports; a port
/// translates it into native page-table-entry bits at the HAL boundary
/// (one definition, `AGENTS.md` §2.2). The default policy is W^X
/// (`AGENTS.md` §19.2): callers never request a leaf that is both
/// [`Self::WRITE`] and [`Self::EXEC`], and a port that is handed such a
/// combination is free to reject it.
#[repr(transparent)]
#[derive(Copy, Clone, Debug, PartialEq, Eq, Hash)]
pub struct PageFlags(u8);

impl PageFlags {
    /// Page is readable.
    pub const READ: Self = Self(0b0000_0001);
    /// Page is writable.
    pub const WRITE: Self = Self(0b0000_0010);
    /// Page is executable (instruction-fetchable).
    pub const EXEC: Self = Self(0b0000_0100);
    /// Page is reachable from user mode (ring 3 / EL0 / U-mode).
    pub const USER: Self = Self(0b0000_1000);
    /// Page maps Device / strongly-ordered memory (MMIO), not cacheable
    /// Normal RAM.
    pub const DEVICE: Self = Self(0b0001_0000);

    /// The empty set.
    #[must_use]
    pub const fn empty() -> Self {
        Self(0)
    }

    /// `true` if `self` contains every bit in `other`.
    #[must_use]
    pub const fn contains(self, other: Self) -> bool {
        (self.0 & other.0) == other.0
    }

    /// Raw bit pattern.
    #[must_use]
    pub const fn bits(self) -> u8 {
        self.0
    }

    /// Reconstruct a flag set from raw bits, rejecting unknown bits.
    ///
    /// Returns `None` if `bits` sets any bit outside the defined flags,
    /// so a corrupt or forward-versioned value fails closed rather than
    /// being silently reinterpreted (`AGENTS.md` §2.9).
    #[must_use]
    pub const fn from_bits(bits: u8) -> Option<Self> {
        const ALL: u8 = PageFlags::READ.0
            | PageFlags::WRITE.0
            | PageFlags::EXEC.0
            | PageFlags::USER.0
            | PageFlags::DEVICE.0;
        if bits & !ALL == 0 {
            Some(Self(bits))
        } else {
            None
        }
    }

    /// `true` if the leaf is both writable and executable — the W^X
    /// violation a port may reject (`AGENTS.md` §19.2).
    #[must_use]
    pub const fn is_write_exec(self) -> bool {
        self.contains(Self::WRITE) && self.contains(Self::EXEC)
    }
}

impl core::ops::BitOr for PageFlags {
    type Output = Self;
    fn bitor(self, rhs: Self) -> Self {
        Self(self.0 | rhs.0)
    }
}

impl core::ops::BitAnd for PageFlags {
    type Output = Self;
    fn bitand(self, rhs: Self) -> Self {
        Self(self.0 & rhs.0)
    }
}

/// The fail-closed result of installing a mapping ([`AddressSpace::map_page`]).
///
/// An address that cannot be mapped cleanly is rejected, never silently
/// truncated, wrapped, or allowed to clobber an existing mapping
/// (`AGENTS.md` §2.9 / §5.4). The variants are the architecture-neutral
/// union every port reports; a port maps its primitive's error onto them
/// at the HAL boundary.
#[derive(Copy, Clone, Debug, PartialEq, Eq)]
pub enum MapError {
    /// `vaddr` or `paddr` was not 4 KiB-aligned.
    Misaligned,
    /// The page-table pool backing this address space was exhausted —
    /// deterministic OOM, never a panic (`AGENTS.md` §4).
    PoolExhausted,
    /// The target virtual address already has a live mapping (or the
    /// walk met a large-page leaf it would have to shatter); the port
    /// refuses to overwrite it rather than silently clobber.
    AlreadyMapped,
    /// The requested [`PageFlags`] are not representable on this port
    /// (e.g. a W^X-violating write+exec leaf, `AGENTS.md` §19.2).
    InvalidFlags,
}

/// The per-process / bootstrap address-space handle an architecture port
/// exposes (`AGENTS.md` §17.2).
///
/// The kernel installs mappings with [`Self::map_page`], reads the
/// root-table physical address with [`Self::root_phys`], and makes the
/// space live on the calling CPU with [`Self::activate`]. The trait is
/// object-safe so the kernel can hold a `dyn AddressSpace` per task; it
/// is also usable as a generic bound (`P: AddressSpace`) so the hot
/// map/translate paths monomorphise to zero dynamic-dispatch cost
/// (`AGENTS.md` §2.3 — no needless overhead).
///
/// The trait deliberately does **not** require [`Send`] / [`Sync`]: a
/// port's page table owns interior `&'static mut` table references and
/// the containing kernel layer serialises access, exactly as
/// `kernel/mem`'s address-space façade does.
pub trait AddressSpace {
    /// Map the 4 KiB physical page `paddr` at virtual address `vaddr`
    /// with `flags`.
    ///
    /// # Errors
    ///
    /// Returns a [`MapError`] (and leaves the address space unchanged) if
    /// either address is misaligned, the page-table pool is exhausted,
    /// the target is already mapped, or the flags are not representable.
    /// The port fails closed rather than install a partial or corrupt
    /// mapping (`AGENTS.md` §2.9).
    fn map_page(&mut self, vaddr: u64, paddr: u64, flags: PageFlags) -> Result<(), MapError>;

    /// Physical address of this space's root translation table — the
    /// value programmed into `CR3` / `satp` / `TTBR0_EL1`.
    fn root_phys(&self) -> u64;

    /// Make this address space the active translation regime on the
    /// calling CPU.
    ///
    /// # Safety
    ///
    /// The caller must guarantee that this address space maps the
    /// currently-executing instruction pointer, the current stack, and
    /// every MMIO region touched before the next [`Self::activate`] —
    /// otherwise the CPU faults on the next fetch/access. The
    /// activation also performs the port's coarse TLB flush so no stale
    /// translation survives the switch.
    unsafe fn activate(&self);
}

/// The §17.2 MMU conformance vertical.
///
/// Every paging-capable architecture port runs [`conformance::run_all`]
/// against its real [`AddressSpace`]. The suite is portable — it names
/// only the trait — and runs on the host, exactly like the sibling
/// [`crate::context::conformance`] and [`crate::timer::conformance`]
/// verticals. It exercises only [`AddressSpace::map_page`] and
/// [`AddressSpace::root_phys`] (pure walk/encoding math);
/// [`AddressSpace::activate`] is proven by each port's `memory_isolation`
/// QEMU vertical (see the module docs).
///
/// It is driven per port (not folded into [`crate::conformance::run_all`])
/// because the suite needs a port-constructed address space and a
/// port-specific mappable address pair — the same precedent as
/// [`crate::irq::conformance`] and [`crate::timer::conformance`].
pub mod conformance {
    use super::{AddressSpace, MapError, PageFlags};

    /// Run the entire [`AddressSpace`] conformance suite against `space`,
    /// using `va` / `pa` as a port-specific 4 KiB-aligned virtual/physical
    /// address pair that is mappable in `space` (outside any pre-installed
    /// identity range).
    ///
    /// # Panics
    ///
    /// Panics (failing the test) if the root-table address is zero, if a
    /// misaligned address is *not* rejected fail-closed, if a good
    /// mapping is refused, or if a second mapping of the same page is
    /// *not* rejected.
    pub fn run_all<A: AddressSpace + ?Sized>(space: &mut A, va: u64, pa: u64) {
        const PAGE: u64 = 4096;
        assert!(
            va % PAGE == 0 && pa % PAGE == 0,
            "the conformance address pair must be page-aligned"
        );
        root_table_is_non_null(space);
        rejects_misaligned_vaddr(space, va, pa);
        rejects_misaligned_paddr(space, va, pa);
        maps_a_good_page_then_refuses_a_double_map(space, va, pa);
    }

    /// A constructed address space already carries its root table, so its
    /// physical address is non-zero.
    fn root_table_is_non_null<A: AddressSpace + ?Sized>(space: &A) {
        assert_ne!(
            space.root_phys(),
            0,
            "a constructed address space must have a non-null root table"
        );
    }

    /// A virtual address that is not 4 KiB-aligned is rejected.
    fn rejects_misaligned_vaddr<A: AddressSpace + ?Sized>(space: &mut A, va: u64, pa: u64) {
        assert_eq!(
            space.map_page(va | 0x1, pa, PageFlags::READ | PageFlags::WRITE),
            Err(MapError::Misaligned),
            "a misaligned vaddr must be rejected"
        );
    }

    /// A physical address that is not 4 KiB-aligned is rejected.
    fn rejects_misaligned_paddr<A: AddressSpace + ?Sized>(space: &mut A, va: u64, pa: u64) {
        assert_eq!(
            space.map_page(va, pa | 0x1, PageFlags::READ | PageFlags::WRITE),
            Err(MapError::Misaligned),
            "a misaligned paddr must be rejected"
        );
    }

    /// A good address pair maps once; a second map of the same page is
    /// refused rather than silently clobbering the first.
    fn maps_a_good_page_then_refuses_a_double_map<A: AddressSpace + ?Sized>(
        space: &mut A,
        va: u64,
        pa: u64,
    ) {
        space
            .map_page(va, pa, PageFlags::READ | PageFlags::WRITE)
            .expect("a page-aligned, in-range mapping must succeed");
        assert_eq!(
            space.map_page(va, pa, PageFlags::READ | PageFlags::WRITE),
            Err(MapError::AlreadyMapped),
            "mapping the same page twice must be refused"
        );
    }

    #[cfg(test)]
    mod tests {
        use super::super::{AddressSpace, MapError, PageFlags};
        use super::run_all;

        /// A faithful host double: it honours the same fail-closed
        /// contract a real port owes and records the single page the
        /// suite maps so the double-map check has something to collide
        /// with. `activate` is never exercised on the host, so its body
        /// is empty (the suite calls only `map_page` / `root_phys`).
        #[derive(Default)]
        struct CellAddressSpace {
            mapped: Option<u64>,
        }

        impl AddressSpace for CellAddressSpace {
            fn map_page(
                &mut self,
                vaddr: u64,
                paddr: u64,
                flags: PageFlags,
            ) -> Result<(), MapError> {
                if vaddr & 0xFFF != 0 || paddr & 0xFFF != 0 {
                    return Err(MapError::Misaligned);
                }
                if flags.is_write_exec() {
                    return Err(MapError::InvalidFlags);
                }
                if self.mapped == Some(vaddr) {
                    return Err(MapError::AlreadyMapped);
                }
                self.mapped = Some(vaddr);
                Ok(())
            }

            fn root_phys(&self) -> u64 {
                0x1000
            }

            unsafe fn activate(&self) {}
        }

        #[test]
        fn suite_accepts_a_faithful_address_space() {
            let mut space = CellAddressSpace::default();
            run_all(&mut space, 0x10_0000_0000, 0x20_0000);
            let mut dynamic = CellAddressSpace::default();
            let erased: &mut dyn AddressSpace = &mut dynamic;
            run_all(erased, 0x10_0000_0000, 0x20_0000);
        }

        /// A broken `map_page` that accepts a misaligned address must be
        /// caught by the fail-closed check.
        #[derive(Default)]
        struct LenientAddressSpace;

        impl AddressSpace for LenientAddressSpace {
            fn map_page(
                &mut self,
                _vaddr: u64,
                _paddr: u64,
                _flags: PageFlags,
            ) -> Result<(), MapError> {
                // Bug: never validates alignment and never collides.
                Ok(())
            }

            fn root_phys(&self) -> u64 {
                0x1000
            }

            unsafe fn activate(&self) {}
        }

        #[test]
        #[should_panic(expected = "a misaligned vaddr must be rejected")]
        fn suite_rejects_an_address_space_that_accepts_a_misaligned_vaddr() {
            run_all(&mut LenientAddressSpace, 0x10_0000_0000, 0x20_0000);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn page_flags_compose_and_inspect() {
        let rw = PageFlags::READ | PageFlags::WRITE;
        assert!(rw.contains(PageFlags::READ));
        assert!(rw.contains(PageFlags::WRITE));
        assert!(!rw.contains(PageFlags::EXEC));
        assert!(!rw.is_write_exec());
        let rwx = rw | PageFlags::EXEC;
        assert!(rwx.is_write_exec());
        let masked = rwx & PageFlags::READ;
        assert_eq!(masked.bits(), PageFlags::READ.bits());
        assert_eq!(PageFlags::empty().bits(), 0);
    }

    #[test]
    fn page_flags_from_bits_rejects_unknown_bits() {
        assert_eq!(
            PageFlags::from_bits(PageFlags::READ.bits()),
            Some(PageFlags::READ)
        );
        let all = PageFlags::READ
            | PageFlags::WRITE
            | PageFlags::EXEC
            | PageFlags::USER
            | PageFlags::DEVICE;
        assert_eq!(PageFlags::from_bits(all.bits()), Some(all));
        assert_eq!(PageFlags::from_bits(0b1000_0000), None);
    }
}
