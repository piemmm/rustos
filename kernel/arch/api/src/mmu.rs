//! MMU / page-table surface of the Arch HAL (
//! "MMU/page-table primitives").
//!
//! Mapping a virtual page to a physical frame, switching the active
//! translation regime, and reading the root-table physical address are
//! privilege-neutral but deeply architecture-specific: x86_64 walks a
//! four-level PML4 and loads `CR3`, riscv64 walks an Sv39 hierarchy and
//! writes `satp`, aarch64 walks a three-level stage-1 table and programs
//! `TTBR0_EL1` + `SCTLR_EL1.M`. The charter makes the architecture surface a
//! closed set of traits on the HAL; this module is the "MMU/page-table"
//! member of that set, so the page-table primitive lives behind one
//! vocabulary instead of being re-described at every call site. The parallel per-arch implementations of this one
//! trait are the deliberate shape of modularity, never
//! collapsed behind `cfg` (carve-out).
//!
//! # What lives here
//!
//! * [`PageFlags`] — the architecture-neutral permission/attribute set a
//!   page leaf carries. Each port translates it into its native
//!   page-table-entry bits at the HAL boundary (one neutral vocabulary). The default policy is W^X: a leaf is
//!   never both [`PageFlags::WRITE`] and [`PageFlags::EXEC`].
//! * [`MapError`] — the fail-closed result of installing a mapping. A
//!   bad address or an exhausted page-table pool is rejected, never
//!   silently truncated or clobbered.
//! * [`AddressSpace`] — the per-port handle the kernel reaches through.
//!   It installs a 4 KiB mapping ([`AddressSpace::map_page`], host-testable
//!   walk/encoding math), activates the translation regime
//!   ([`AddressSpace::activate`], the port's privileged register write),
//!   and reports the root-table physical address
//!   ([`AddressSpace::root_phys`]).
//! * [`conformance`] — the conformance vertical: a host-run
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
//! stub that "activates" would be a fake primitive.
//!
//! # Scope (the burn-down)
//!
//! This is the `plans/WIRING.md` **Stage W5b-1** slice: the bootstrap
//! page-table primitive every port already owns, lifted behind one HAL
//! trait and exercised by the `memory_isolation` verticals through it.
//! Wiring `kernel/mem`'s allocator-backed per-process address space onto
//! this trait, and the per-page + cross-CPU TLB shootdown (which depends
//! on the aarch64 IPI from Stage W6), are the tracked Stage W5b-2 / W6
//! follow-ups — not silently duplicated here.

/// The architecture-neutral permission/attribute set a 4 KiB page leaf
/// carries.
///
/// A neutral subset of the bits every Tier-1 MMU supports; a port
/// translates it into native page-table-entry bits at the HAL boundary
/// (one definition). The default policy is W^X: callers never request a leaf that is both
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
    /// Page backs a buffer **shared with a DMA-capable device** that must
    /// stay coherent with that device without per-access cache maintenance.
    ///
    /// On a platform whose DMA masters snoop the CPU caches (x86_64,
    /// riscv64, the QEMU `virt` boards) this is ordinary cacheable Normal
    /// memory and a port may ignore the bit. On a platform whose DMA path is
    /// **not** I/O-coherent (the Raspberry Pi 4 BCM2711 PCIe root complex
    /// does not snoop the CPU caches), a port maps it Normal **Non-Cacheable**
    /// so a descriptor the CPU writes is visible to the device — and an event
    /// the device writes is visible to the CPU — with no `dc civac` dance.
    /// It is distinct from [`Self::DEVICE`]: a DMA buffer holds ring and
    /// context structures the driver accesses with ordinary (possibly
    /// unaligned) loads/stores, which Device-nGnRE memory forbids
    /// (the kernel owns the platform coherency, so
    /// the user-space driver stays arch-neutral).
    pub const DMA_COHERENT: Self = Self(0b0010_0000);
    /// Page is a write-mostly framebuffer aperture. CPU stores may be
    /// gathered and combined, reads remain valid, and the mapping is never
    /// executable. Distinct from bidirectional [`Self::DMA_COHERENT`].
    pub const WRITE_COMBINE: Self = Self(0b0100_0000);

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
    /// being silently reinterpreted.
    #[must_use]
    pub const fn from_bits(bits: u8) -> Option<Self> {
        const ALL: u8 = PageFlags::READ.0
            | PageFlags::WRITE.0
            | PageFlags::EXEC.0
            | PageFlags::USER.0
            | PageFlags::DEVICE.0
            | PageFlags::DMA_COHERENT.0
            | PageFlags::WRITE_COMBINE.0;
        if bits & !ALL == 0 {
            Some(Self(bits))
        } else {
            None
        }
    }

    /// The union of two sets — the `const` counterpart of
    /// [`core::ops::BitOr`], usable when building a flag constant.
    #[must_use]
    pub const fn union(self, other: Self) -> Self {
        Self(self.0 | other.0)
    }

    /// `true` if the leaf is both writable and executable — the W^X
    /// violation a port may reject.
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
/// truncated, wrapped, or allowed to clobber an existing mapping. The variants are the architecture-neutral
/// union every port reports; a port maps its primitive's error onto them
/// at the HAL boundary.
#[derive(Copy, Clone, Debug, PartialEq, Eq)]
pub enum MapError {
    /// `vaddr` or `paddr` was not 4 KiB-aligned.
    Misaligned,
    /// The page-table pool backing this address space was exhausted —
    /// deterministic OOM, never a panic.
    PoolExhausted,
    /// The target virtual address already has a live mapping (or the
    /// walk met a large-page leaf it would have to shatter); the port
    /// refuses to overwrite it rather than silently clobber.
    AlreadyMapped,
    /// The requested [`PageFlags`] are not representable on this port
    /// (e.g. a W^X-violating write+exec leaf).
    InvalidFlags,
    /// The target virtual address has no live 4 KiB leaf to operate on
    /// (reported by [`AddressSpace::unmap`] when asked to tear down an
    /// address that was never mapped); the port refuses rather than
    /// fabricating a frame.
    NotMapped,
    /// The operation is not implemented on this port. Returned by the
    /// default [`AddressSpace::split_block`] of a port whose
    /// [`AddressSpace::block_split_support`] is not [`BlockSplit::Supported`]
    /// — the coarse-block split (the guard-page fault-form, `plans/PI.md`
    /// G1–G3) is implemented on aarch64 but pending on the other ports, so
    /// asking for it elsewhere fails closed rather than silently doing
    /// nothing.
    Unsupported,
}

/// A port's honest declaration of whether it can re-express a coarse
/// (large-page / block) mapping at 4 KiB granularity — the foundation of
/// the kthread guard-page fault-form (`plans/PI.md` G1–G3).
///
/// Re-expressing a coarse block as a table of finer leaves is what lets a
/// single 4 KiB page inside a boot-time identity *block* be unmapped (so
/// an overrun into it faults) without disturbing its neighbours
/// ([`AddressSpace::split_block`]). Only aarch64 implements it today; the
/// other paging ports honestly declare it [`BlockSplit::Pending`] rather
/// than pretend (the same honesty discipline as
/// [`crate::memtag::Tagging`] and [`crate::sidechannel::Mitigation`]).
#[derive(Copy, Clone, Debug, Eq, PartialEq)]
pub enum BlockSplit {
    /// The port re-expresses a coarse block at 4 KiB granularity
    /// ([`AddressSpace::split_block`] does real work).
    Supported,
    /// The port's translation regime has no coarse blocks to split (the
    /// payload is the justification; it must be non-empty).
    Unsupported(&'static str),
    /// The port *could* split blocks but the primitive has not landed yet
    /// (the payload is the tracking note — the `plans/PI.md` stage that
    /// will deliver it; it must be non-empty).
    Pending(&'static str),
}

impl BlockSplit {
    /// `true` if the port re-expresses coarse blocks at 4 KiB granularity.
    #[must_use]
    pub const fn is_supported(self) -> bool {
        matches!(self, Self::Supported)
    }

    /// `true` if the port has a tracked [`BlockSplit::Pending`] gap.
    #[must_use]
    pub const fn is_pending(self) -> bool {
        matches!(self, Self::Pending(_))
    }

    /// `true` if the declaration is release-ready: either supported or a
    /// justified [`BlockSplit::Unsupported`]. A [`BlockSplit::Pending`]
    /// gap is honest but not release-ready.
    #[must_use]
    pub const fn is_release_ready(self) -> bool {
        matches!(self, Self::Supported | Self::Unsupported(_))
    }

    /// The explanatory note for a non-supported declaration, or `None`
    /// when supported.
    #[must_use]
    pub const fn detail(self) -> Option<&'static str> {
        match self {
            Self::Supported => None,
            Self::Unsupported(reason) | Self::Pending(reason) => Some(reason),
        }
    }
}

/// A port's honest declaration of whether it can report which pages a
/// task has recently touched — the referenced-bit source the
/// page-replacement scanner (`kernel/mem::coldscan`) needs to tell a
/// genuinely cold page from a hot one before the compressed-memory tier
/// (`plans/SWAPSWAPSWAP.md`) reclaims it.
///
/// This mirrors the honesty discipline of [`BlockSplit`],
/// [`crate::memtag::Tagging`], and [`crate::sidechannel::Mitigation`]: a
/// port never pretends a referenced bit exists. When the declaration is
/// not [`AccessTracking::Supported`], [`AddressSpace::test_and_clear_accessed`]
/// fails closed with [`MapError::Unsupported`] and the scanner refuses to
/// classify *any* page cold on that port — reclaim is safe by omission,
/// never by guessing a page is unused.
#[derive(Copy, Clone, Debug, Eq, PartialEq)]
pub enum AccessTracking {
    /// The port reports and clears a per-page referenced (accessed) bit
    /// ([`AddressSpace::test_and_clear_accessed`] does real work).
    Supported,
    /// The port's translation regime exposes no referenced bit at all
    /// (the payload is the justification; it must be non-empty).
    Unsupported(&'static str),
    /// The port *could* report a referenced bit but the primitive has not
    /// landed yet (the payload is the tracking note — the plan stage that
    /// will deliver it; it must be non-empty).
    Pending(&'static str),
}

impl AccessTracking {
    /// `true` if the port reports and clears a per-page referenced bit.
    #[must_use]
    pub const fn is_supported(self) -> bool {
        matches!(self, Self::Supported)
    }

    /// `true` if the port has a tracked [`AccessTracking::Pending`] gap.
    #[must_use]
    pub const fn is_pending(self) -> bool {
        matches!(self, Self::Pending(_))
    }

    /// `true` if the declaration is release-ready: either supported or a
    /// justified [`AccessTracking::Unsupported`]. A
    /// [`AccessTracking::Pending`] gap is honest but not release-ready.
    #[must_use]
    pub const fn is_release_ready(self) -> bool {
        matches!(self, Self::Supported | Self::Unsupported(_))
    }

    /// The explanatory note for a non-supported declaration, or `None`
    /// when supported.
    #[must_use]
    pub const fn detail(self) -> Option<&'static str> {
        match self {
            Self::Supported => None,
            Self::Unsupported(reason) | Self::Pending(reason) => Some(reason),
        }
    }
}

/// A run of kernel virtual address space a port guarantees is free of any
/// other mapping **and** reachable from every address space it builds.
///
/// This is what makes a kernel `vmap` possible: growth that needs a
/// virtually-contiguous range assembled from scattered physical chunks
/// needs somewhere to assemble it, and — because kernel code runs with the
/// *current task's* translation root active — that somewhere must resolve
/// identically under every root. A port reserves the window by pointing
/// the covering top-level table entry of every root it builds at one
/// shared sub-hierarchy, so a leaf installed once is visible everywhere.
///
/// The window costs no RAM until something is mapped into it; only the
/// shared sub-hierarchy's table frames are drawn up front.
#[derive(Copy, Clone, Debug, Eq, PartialEq)]
pub struct KernelWindow {
    base: u64,
    pages: usize,
}

impl KernelWindow {
    /// Describe the window `[base, base + pages * 4 KiB)`.
    ///
    /// Returns `None` — never a truncated or wrapped window — unless
    /// `base` is 4 KiB-aligned, `pages` is non-zero, and the byte span and
    /// its exclusive top both fit the address width. A window ending
    /// exactly at the top of the address space is therefore refused too,
    /// so no consumer has to reason about wrapping arithmetic.
    #[must_use]
    pub const fn new(base: u64, pages: usize) -> Option<Self> {
        const PAGE_BYTES: u64 = 4096;
        if base & (PAGE_BYTES - 1) != 0 || pages == 0 {
            return None;
        }
        // `usize as u64` is lossless on every target (`usize` is at most
        // 64 bits), so the page count widens without a checked conversion.
        let Some(span) = (pages as u64).checked_mul(PAGE_BYTES) else {
            return None;
        };
        if base.checked_add(span).is_none() {
            return None;
        }
        Some(Self { base, pages })
    }

    /// Lowest address in the window.
    #[must_use]
    pub const fn base(self) -> u64 {
        self.base
    }

    /// Number of 4 KiB pages the window spans.
    #[must_use]
    pub const fn pages(self) -> usize {
        self.pages
    }

    /// Byte length of the window. Never zero (a zero-page window is
    /// refused at construction), so there is no emptiness to test.
    #[must_use]
    pub const fn len_bytes(self) -> u64 {
        // Validated at construction, so the product cannot overflow.
        (self.pages as u64) * 4096
    }

    /// `true` if `vaddr` lies inside the window.
    #[must_use]
    pub const fn contains(self, vaddr: u64) -> bool {
        vaddr >= self.base && vaddr - self.base < self.len_bytes()
    }

    /// The 0-based page index of `vaddr` within the window, or `None` when
    /// `vaddr` lies outside it.
    #[must_use]
    pub const fn page_index(self, vaddr: u64) -> Option<usize> {
        if !self.contains(vaddr) {
            return None;
        }
        Some(((vaddr - self.base) / 4096) as usize)
    }
}

/// The per-process / bootstrap address-space handle an architecture port
/// exposes.
///
/// The kernel installs mappings with [`Self::map_page`], reads the
/// root-table physical address with [`Self::root_phys`], and makes the
/// space live on the calling CPU with [`Self::activate`]. The trait is
/// object-safe so the kernel can hold a `dyn AddressSpace` per task; it
/// is also usable as a generic bound (`P: AddressSpace`) so the hot
/// map/translate paths monomorphise to zero dynamic-dispatch cost
/// (no needless overhead).
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
    /// mapping.
    fn map_page(&mut self, vaddr: u64, paddr: u64, flags: PageFlags) -> Result<(), MapError>;

    /// Translate `vaddr` to the physical page it maps and the leaf's
    /// [`PageFlags`], or `None` when `vaddr` has no live 4 KiB leaf.
    ///
    /// A read-only page-table walk: it never installs or mutates a
    /// mapping. The returned physical address is the 4 KiB leaf base
    /// (`vaddr`'s page offset is *not* re-applied); the flags are the
    /// neutral permission set the leaf carries, decoded back from the
    /// port's native page-table-entry bits at the HAL boundary.
    fn translate(&self, vaddr: u64) -> Option<(u64, PageFlags)>;

    /// Tear down the 4 KiB mapping for `vaddr` and return the physical
    /// page it resolved to.
    ///
    /// # Errors
    ///
    /// Returns [`MapError::Misaligned`] if `vaddr` is not 4 KiB-aligned,
    /// or [`MapError::NotMapped`] if `vaddr` has no live 4 KiB leaf. The
    /// port leaves the address space unchanged on either error and never
    /// fabricates a frame.
    fn unmap(&mut self, vaddr: u64) -> Result<u64, MapError>;

    /// Physical address of this space's root translation table — the
    /// value programmed into `CR3` / `satp` / `TTBR0_EL1`.
    fn root_phys(&self) -> u64;

    /// This port's honest declaration of whether it can report a per-page
    /// referenced (accessed) bit through [`Self::test_and_clear_accessed`]
    /// — the source `kernel/mem::coldscan` uses to find genuinely cold
    /// pages for the compressed-memory tier (`plans/SWAPSWAPSWAP.md`).
    ///
    /// The default is fail-closed: a port that has not implemented the
    /// referenced bit declares it [`AccessTracking::Unsupported`], so the
    /// scanner never classifies a page cold on it. A port with a hardware
    /// access flag (or a software access-flag-fault scheme) overrides this
    /// with [`AccessTracking::Supported`].
    fn access_tracking(&self) -> AccessTracking {
        AccessTracking::Unsupported("port exposes no per-page referenced bit")
    }

    /// Read and clear the per-page referenced (accessed) bit for the 4 KiB
    /// leaf mapping `vaddr`, returning whether the page had been accessed
    /// since the previous clear.
    ///
    /// This is the referenced-bit half of a clock / second-chance
    /// page-replacement scan: clearing the bit and invalidating the page's
    /// TLB entry means the hardware (or the software access-flag-fault
    /// path) re-sets it on the next access, so a later probe that still
    /// reads it clear proves the page went untouched in between — a
    /// genuinely cold page the compressed-memory tier may reclaim. The
    /// implementation performs the TLB invalidation itself; the caller
    /// guarantees the owning task is quiesced on other CPUs when the
    /// result drives a reclaim (the same exclusivity the tier's move path
    /// relies on).
    ///
    /// # Errors
    ///
    /// * [`MapError::Misaligned`] if `vaddr` is not 4 KiB-aligned.
    /// * [`MapError::NotMapped`] if `vaddr` has no live 4 KiB leaf.
    /// * [`MapError::Unsupported`] on a port whose [`Self::access_tracking`]
    ///   is not [`AccessTracking::Supported`] — asking it to report a
    ///   referenced bit fails closed rather than fabricating one.
    fn test_and_clear_accessed(&mut self, vaddr: u64) -> Result<bool, MapError> {
        let _ = vaddr;
        Err(MapError::Unsupported)
    }

    /// This port's honest declaration of whether it can re-express a
    /// coarse (large-page / block) mapping at 4 KiB granularity via
    /// [`Self::split_block`] (`plans/PI.md` G1–G3).
    ///
    /// Every port declares one honest position (never
    /// pretend a defence exists): [`BlockSplit::Supported`] (the port does
    /// it), [`BlockSplit::Unsupported`] (its translation regime has no
    /// coarse blocks to split), or [`BlockSplit::Pending`] (it could but
    /// the primitive has not landed). A non-supported declaration must
    /// carry a non-empty justification, which [`conformance::run_all`]
    /// checks.
    fn block_split_support(&self) -> BlockSplit;

    /// Re-express the coarse block covering `vaddr` at 4 KiB granularity,
    /// preserving the mapped output address and every attribute, so the
    /// single 4 KiB page containing `vaddr` can then be torn down with
    /// [`Self::unmap`] (+ a [`crate::tlb::TlbShootdown::flush_page`])
    /// without disturbing its neighbours.
    ///
    /// This is the foundation of the kthread guard-page fault-form
    /// (`plans/PI.md` G1–G3): a guard page that
    /// the boot path mapped inside a coarse identity *block* has no
    /// per-4 KiB leaf to clear until the block is re-expressed as a table
    /// of finer leaves. It is idempotent: an already-fine hierarchy
    /// allocates nothing and pays no maintenance.
    ///
    /// It is **not** break-before-make-free. Adding a table level
    /// preserves each address's output and permissions but changes the
    /// *granule* it translates at, and a CPU holding the coarse
    /// translation may fault a walk the tables plainly satisfy. A port
    /// therefore owes the maintenance for the whole former block's range
    /// — not the one page the caller came for — whenever a granule
    /// actually changed, and the root must not be the active translation
    /// regime (`plans/OPEN-DEFECTS.md` D81, D82).
    ///
    /// The default fails closed with [`MapError::Unsupported`] for a port
    /// whose [`Self::block_split_support`] is not [`BlockSplit::Supported`]
    /// — asking it to split a block does nothing silently is *not* an
    /// option. A supporting port overrides this.
    ///
    /// # Errors
    ///
    /// Returns [`MapError::Misaligned`] if `vaddr` is not 4 KiB-aligned,
    /// [`MapError::NotMapped`] if `vaddr` has no live mapping to split,
    /// [`MapError::PoolExhausted`] if the page-table pool cannot supply a
    /// replacement table, or [`MapError::Unsupported`] on a port that does
    /// not implement the split.
    fn split_block(&mut self, vaddr: u64) -> Result<(), MapError> {
        let _ = vaddr;
        Err(MapError::Unsupported)
    }

    /// Re-express every coarse block covering the arena
    /// `[base, base + len)` at 4 KiB granularity, so any single page in
    /// the arena (e.g. a kthread kernel-stack guard page) can later be
    /// torn down with [`Self::unmap`] (+ a
    /// [`crate::tlb::TlbShootdown::flush_page`]) without disturbing the
    /// block the running CPU executes on.
    ///
    /// This is [`Self::split_block`] applied to every coarse block the
    /// arena spans (`plans/PI.md` G2): a guard-page arena laid down inside
    /// the boot path's coarse identity blocks has no per-4 KiB leaf to
    /// clear until those blocks are re-expressed as tables of finer
    /// leaves. Done up-front, at boot, while the arena holds no running
    /// context. It is idempotent: an already-fine hierarchy allocates
    /// nothing and changes no granule.
    ///
    /// It is **not** break-before-make-free. Adding a table level
    /// preserves each address's output and permissions but changes the
    /// *granule* it translates at, and a TLB holding both granules for one
    /// address is CONSTRAINED UNPREDICTABLE — which is why
    /// [`Self::split_block`] owns the maintenance for the whole former
    /// block's range rather than leaving a caller to flush one page of it
    /// (`plans/OPEN-DEFECTS.md` D81). The precondition is therefore that
    /// the root **must not** be the active translation regime; a live
    /// refinement is bounded by that maintenance but still opens a window
    /// the architecture leaves undefined (D82).
    ///
    /// The default fails closed with [`MapError::Unsupported`] for a port
    /// whose [`Self::block_split_support`] is not [`BlockSplit::Supported`]
    /// — the arena defence falls back to the software canary for such a
    /// port, never silently no-ops.
    /// A supporting port overrides this.
    ///
    /// # Errors
    ///
    /// Returns [`MapError::Misaligned`] if `len` is zero or `base` is not
    /// 4 KiB-aligned, [`MapError::NotMapped`] if any covering block has no
    /// live mapping, [`MapError::PoolExhausted`] if the page-table pool
    /// cannot supply a replacement table, or [`MapError::Unsupported`] on a
    /// port that does not implement the split.
    fn prepare_guard_arena(&mut self, base: u64, len: u64) -> Result<(), MapError> {
        let _ = (base, len);
        Err(MapError::Unsupported)
    }

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

    /// Return every allocator-drawn page-table frame of this space — the
    /// root table and every intermediate table its walks allocated — to
    /// the frame source it was drawn from, leaving the space unusable.
    ///
    /// This is the teardown half of the page-table frame seam
    /// ([`crate::frames::PageTableFrames`]): a dead process's stage-1
    /// hierarchy is walked post-order (children before parents, the root
    /// last, through the one shared
    /// [`crate::frames::reclaim_hierarchy`] walk) and each *table* frame
    /// is handed back through
    /// [`crate::frames::PageTableFrames::free_table`]. Leaf frames are
    /// never touched — user RAM, MMIO windows, and shared regions belong
    /// to their own owners and are reclaimed by the caller before this
    /// runs.
    ///
    /// The default is the honest no-op for a backend with no
    /// allocator-drawn table frames (a bookkeeping-only host double, the
    /// wasm32 sandbox); every paging port whose tables come from a
    /// [`crate::frames::PageTableFrames`] source overrides it.
    ///
    /// # Safety
    ///
    /// The caller must guarantee this space is not, and can never again
    /// become, the active translation regime of **any** CPU (the owning
    /// task has exited and every CPU that ran it has since parked on a
    /// permanent kernel root), and that no other reference into the
    /// space's tables is live. After the call the space translates
    /// nothing; using it in any way is a bug.
    unsafe fn reclaim_table_frames(&mut self) {}
}

/// The MMU conformance vertical.
///
/// Every paging-capable architecture port runs [`conformance::run_all`]
/// against its real [`AddressSpace`]. The suite is portable — it names
/// only the trait — and runs on the host, exactly like the sibling
/// [`crate::context::conformance`] and [`crate::timer::conformance`]
/// verticals. It exercises [`AddressSpace::map_page`],
/// [`AddressSpace::translate`], [`AddressSpace::unmap`], and
/// [`AddressSpace::root_phys`] (pure walk/encoding math);
/// [`AddressSpace::activate`] is proven by each port's `memory_isolation`
/// QEMU vertical (see the module docs).
///
/// It is driven per port (not folded into [`crate::conformance::run_all`])
/// because the suite needs a port-constructed address space and a
/// port-specific mappable address pair — the same precedent as
/// [`crate::irq::conformance`] and [`crate::timer::conformance`].
pub mod conformance {
    use super::{AccessTracking, AddressSpace, BlockSplit, MapError, PageFlags};

    /// Run the entire [`AddressSpace`] conformance suite against `space`,
    /// using `va` / `pa` as a port-specific 4 KiB-aligned virtual/physical
    /// address pair that is mappable in `space` (outside any pre-installed
    /// identity range).
    ///
    /// # Panics
    ///
    /// Panics (failing the test) if the root-table address is zero, if a
    /// misaligned address is *not* rejected fail-closed, if a good
    /// mapping is refused, if a second mapping of the same page is *not*
    /// rejected, if the mapping does not then translate back to its
    /// physical page, or if the round-trip map → translate → unmap →
    /// translate-again lifecycle does not fail closed on the torn-down
    /// page.
    pub fn run_all<A: AddressSpace + ?Sized>(space: &mut A, va: u64, pa: u64) {
        const PAGE: u64 = 4096;
        assert!(
            va.is_multiple_of(PAGE) && pa.is_multiple_of(PAGE),
            "the conformance address pair must be page-aligned"
        );
        root_table_is_non_null(space);
        rejects_misaligned_vaddr(space, va, pa);
        rejects_misaligned_paddr(space, va, pa);
        unmapped_address_does_not_translate(space, va);
        maps_translates_then_refuses_a_double_map(space, va, pa);
        unmaps_then_translates_to_nothing(space, va, pa);
        block_split_declaration_is_honest(space, va);
        access_tracking_declaration_is_honest(space, va);
    }

    /// The port's [`AddressSpace::access_tracking`] is honest: a
    /// non-supported declaration carries a non-empty justification (a
    /// referenced bit is never pretended), and a port that does *not*
    /// support it fails [`AddressSpace::test_and_clear_accessed`] closed
    /// with [`MapError::Unsupported`] rather than fabricating an
    /// access verdict the cold-page scanner would then trust. A supporting
    /// port's positive behaviour needs a real referenced bit the portable
    /// suite cannot set, so it is proven by that port's own host tests and
    /// its `memory_isolation`/reclaim QEMU vertical, not here.
    fn access_tracking_declaration_is_honest<A: AddressSpace + ?Sized>(space: &mut A, va: u64) {
        let support = space.access_tracking();
        if let Some(reason) = support.detail() {
            assert!(
                !reason.trim().is_empty(),
                "a non-supported access-tracking declaration must carry a non-empty justification"
            );
        }
        if !matches!(support, AccessTracking::Supported) {
            assert_eq!(
                space.test_and_clear_accessed(va),
                Err(MapError::Unsupported),
                "a port without access tracking must fail test_and_clear_accessed closed"
            );
        }
    }

    /// The port's [`AddressSpace::block_split_support`] is honest: a
    /// non-supported declaration carries a non-empty justification
    /// (a defence is never pretended), and a port that
    /// does *not* support the split fails both [`AddressSpace::split_block`]
    /// and [`AddressSpace::prepare_guard_arena`] closed with
    /// [`MapError::Unsupported`] rather than silently doing nothing
    /// (the guard-page arena that builds on the split
    /// must fall back to the software canary, never pretend success). A
    /// supporting port's positive split behaviour is proven by its own host
    /// tests (it needs a known coarse block, which this portable suite does
    /// not have), so the supported case is only required to declare itself,
    /// not exercised here.
    fn block_split_declaration_is_honest<A: AddressSpace + ?Sized>(space: &mut A, va: u64) {
        let support = space.block_split_support();
        if let Some(reason) = support.detail() {
            assert!(
                !reason.trim().is_empty(),
                "a non-supported block-split declaration must carry a non-empty justification"
            );
        }
        if !matches!(support, BlockSplit::Supported) {
            assert_eq!(
                space.split_block(va),
                Err(MapError::Unsupported),
                "a port that does not support block-split must fail split_block closed"
            );
            assert_eq!(
                space.prepare_guard_arena(va, 4096),
                Err(MapError::Unsupported),
                "a port that does not support block-split must fail prepare_guard_arena closed"
            );
        }
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

    /// An address with no live leaf translates to nothing rather than
    /// fabricating a frame.
    fn unmapped_address_does_not_translate<A: AddressSpace + ?Sized>(space: &A, va: u64) {
        assert_eq!(
            space.translate(va),
            None,
            "an address with no live leaf must not translate"
        );
    }

    /// A good address pair maps once and then translates back to its
    /// physical page with the permissions it was mapped with; a second
    /// map of the same page is refused rather than silently clobbering
    /// the first.
    fn maps_translates_then_refuses_a_double_map<A: AddressSpace + ?Sized>(
        space: &mut A,
        va: u64,
        pa: u64,
    ) {
        space
            .map_page(va, pa, PageFlags::READ | PageFlags::WRITE)
            .expect("a page-aligned, in-range mapping must succeed");
        let (mapped_pa, flags) = space
            .translate(va)
            .expect("a freshly mapped page must translate");
        assert_eq!(mapped_pa, pa, "translate must report the mapped frame");
        assert!(
            flags.contains(PageFlags::READ) && flags.contains(PageFlags::WRITE),
            "translate must report the permissions the leaf was mapped with"
        );
        assert_eq!(
            space.map_page(va, pa, PageFlags::READ | PageFlags::WRITE),
            Err(MapError::AlreadyMapped),
            "mapping the same page twice must be refused"
        );
    }

    /// The page mapped above unmaps once (returning its frame), then no
    /// longer translates, and a second unmap fails closed with
    /// [`MapError::NotMapped`] — the map/unmap lifecycle is symmetric.
    fn unmaps_then_translates_to_nothing<A: AddressSpace + ?Sized>(
        space: &mut A,
        va: u64,
        pa: u64,
    ) {
        assert_eq!(
            space.unmap(va),
            Ok(pa),
            "unmapping a live page must return its frame"
        );
        assert_eq!(
            space.translate(va),
            None,
            "an unmapped page must no longer translate"
        );
        assert_eq!(
            space.unmap(va),
            Err(MapError::NotMapped),
            "unmapping an absent page must fail closed"
        );
    }

    #[cfg(test)]
    mod tests {
        use super::super::{AccessTracking, AddressSpace, BlockSplit, MapError, PageFlags};
        use super::run_all;

        /// A faithful host double: it honours the same fail-closed
        /// contract a real port owes and records the single page the
        /// suite maps (its frame and flags) so the double-map, translate,
        /// and unmap checks have something to collide with / recover.
        /// `activate` is never exercised on the host, so its body is
        /// empty (the suite calls only `map_page` / `translate` / `unmap`
        /// / `root_phys`).
        #[derive(Default)]
        struct CellAddressSpace {
            mapped: Option<(u64, u64, PageFlags)>,
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
                if matches!(self.mapped, Some((v, _, _)) if v == vaddr) {
                    return Err(MapError::AlreadyMapped);
                }
                self.mapped = Some((vaddr, paddr, flags));
                Ok(())
            }

            fn translate(&self, vaddr: u64) -> Option<(u64, PageFlags)> {
                match self.mapped {
                    Some((v, pa, flags)) if v == vaddr => Some((pa, flags)),
                    _ => None,
                }
            }

            fn unmap(&mut self, vaddr: u64) -> Result<u64, MapError> {
                if vaddr & 0xFFF != 0 {
                    return Err(MapError::Misaligned);
                }
                match self.mapped {
                    Some((v, pa, _)) if v == vaddr => {
                        self.mapped = None;
                        Ok(pa)
                    }
                    _ => Err(MapError::NotMapped),
                }
            }

            fn root_phys(&self) -> u64 {
                0x1000
            }

            fn block_split_support(&self) -> BlockSplit {
                BlockSplit::Unsupported("host double tracks single 4 KiB entries; no coarse blocks")
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

            fn translate(&self, _vaddr: u64) -> Option<(u64, PageFlags)> {
                None
            }

            fn unmap(&mut self, _vaddr: u64) -> Result<u64, MapError> {
                Err(MapError::NotMapped)
            }

            fn root_phys(&self) -> u64 {
                0x1000
            }

            fn block_split_support(&self) -> BlockSplit {
                BlockSplit::Unsupported("test double")
            }

            unsafe fn activate(&self) {}
        }

        #[test]
        #[should_panic(expected = "a misaligned vaddr must be rejected")]
        fn suite_rejects_an_address_space_that_accepts_a_misaligned_vaddr() {
            run_all(&mut LenientAddressSpace, 0x10_0000_0000, 0x20_0000);
        }

        /// A port that *claims* to split blocks (`BlockSplit::Supported`)
        /// is taken at its word by the portable suite (its positive split
        /// behaviour is proven by its own host tests), so a faithful
        /// double may declare itself supported and still pass.
        #[derive(Default)]
        struct SupportedSplitAddressSpace {
            inner: CellAddressSpace,
        }

        impl AddressSpace for SupportedSplitAddressSpace {
            fn map_page(
                &mut self,
                vaddr: u64,
                paddr: u64,
                flags: PageFlags,
            ) -> Result<(), MapError> {
                self.inner.map_page(vaddr, paddr, flags)
            }
            fn translate(&self, vaddr: u64) -> Option<(u64, PageFlags)> {
                self.inner.translate(vaddr)
            }
            fn unmap(&mut self, vaddr: u64) -> Result<u64, MapError> {
                self.inner.unmap(vaddr)
            }
            fn root_phys(&self) -> u64 {
                self.inner.root_phys()
            }
            fn block_split_support(&self) -> BlockSplit {
                BlockSplit::Supported
            }
            fn split_block(&mut self, _vaddr: u64) -> Result<(), MapError> {
                Ok(())
            }
            unsafe fn activate(&self) {}
        }

        #[test]
        fn suite_accepts_a_port_that_declares_block_split_supported() {
            let mut space = SupportedSplitAddressSpace::default();
            run_all(&mut space, 0x10_0000_0000, 0x20_0000);
        }

        /// A port that declares the split *unsupported* but then fails to
        /// fail `split_block` closed (it silently no-ops) is a fail-open
        /// hole and must be caught.
        #[derive(Default)]
        struct FailOpenSplitAddressSpace {
            inner: CellAddressSpace,
        }

        impl AddressSpace for FailOpenSplitAddressSpace {
            fn map_page(
                &mut self,
                vaddr: u64,
                paddr: u64,
                flags: PageFlags,
            ) -> Result<(), MapError> {
                self.inner.map_page(vaddr, paddr, flags)
            }
            fn translate(&self, vaddr: u64) -> Option<(u64, PageFlags)> {
                self.inner.translate(vaddr)
            }
            fn unmap(&mut self, vaddr: u64) -> Result<u64, MapError> {
                self.inner.unmap(vaddr)
            }
            fn root_phys(&self) -> u64 {
                self.inner.root_phys()
            }
            fn block_split_support(&self) -> BlockSplit {
                BlockSplit::Unsupported("no coarse blocks")
            }
            fn split_block(&mut self, _vaddr: u64) -> Result<(), MapError> {
                // Bug: claims unsupported yet silently succeeds.
                Ok(())
            }
            unsafe fn activate(&self) {}
        }

        #[test]
        #[should_panic(expected = "must fail split_block closed")]
        fn suite_rejects_a_fail_open_unsupported_split() {
            let mut space = FailOpenSplitAddressSpace::default();
            run_all(&mut space, 0x10_0000_0000, 0x20_0000);
        }

        /// A non-supported declaration with an empty justification is a
        /// dishonest profile and must be caught.
        #[derive(Default)]
        struct EmptyJustificationAddressSpace {
            inner: CellAddressSpace,
        }

        impl AddressSpace for EmptyJustificationAddressSpace {
            fn map_page(
                &mut self,
                vaddr: u64,
                paddr: u64,
                flags: PageFlags,
            ) -> Result<(), MapError> {
                self.inner.map_page(vaddr, paddr, flags)
            }
            fn translate(&self, vaddr: u64) -> Option<(u64, PageFlags)> {
                self.inner.translate(vaddr)
            }
            fn unmap(&mut self, vaddr: u64) -> Result<u64, MapError> {
                self.inner.unmap(vaddr)
            }
            fn root_phys(&self) -> u64 {
                self.inner.root_phys()
            }
            fn block_split_support(&self) -> BlockSplit {
                BlockSplit::Pending("   ")
            }
            unsafe fn activate(&self) {}
        }

        #[test]
        #[should_panic(expected = "must carry a non-empty justification")]
        fn suite_rejects_an_empty_block_split_justification() {
            let mut space = EmptyJustificationAddressSpace::default();
            run_all(&mut space, 0x10_0000_0000, 0x20_0000);
        }

        /// A port that declares access tracking unsupported yet returns a
        /// concrete access verdict from `test_and_clear_accessed` is
        /// fail-open: the cold-page scanner would trust a fabricated bit
        /// and could reclaim a hot page. The conformance vertical must
        /// catch it.
        #[derive(Default)]
        struct FailOpenAccessAddressSpace {
            inner: CellAddressSpace,
        }

        impl AddressSpace for FailOpenAccessAddressSpace {
            fn map_page(
                &mut self,
                vaddr: u64,
                paddr: u64,
                flags: PageFlags,
            ) -> Result<(), MapError> {
                self.inner.map_page(vaddr, paddr, flags)
            }
            fn translate(&self, vaddr: u64) -> Option<(u64, PageFlags)> {
                self.inner.translate(vaddr)
            }
            fn unmap(&mut self, vaddr: u64) -> Result<u64, MapError> {
                self.inner.unmap(vaddr)
            }
            fn root_phys(&self) -> u64 {
                self.inner.root_phys()
            }
            fn block_split_support(&self) -> BlockSplit {
                BlockSplit::Unsupported("no coarse blocks")
            }
            fn access_tracking(&self) -> AccessTracking {
                AccessTracking::Unsupported("claims no referenced bit")
            }
            fn test_and_clear_accessed(&mut self, _vaddr: u64) -> Result<bool, MapError> {
                // Bug: claims unsupported yet fabricates a verdict.
                Ok(false)
            }
            unsafe fn activate(&self) {}
        }

        #[test]
        #[should_panic(expected = "must fail test_and_clear_accessed closed")]
        fn suite_rejects_a_fail_open_access_tracking() {
            let mut space = FailOpenAccessAddressSpace::default();
            run_all(&mut space, 0x10_0000_0000, 0x20_0000);
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

    #[test]
    fn block_split_helpers_classify_each_declaration() {
        assert!(BlockSplit::Supported.is_supported());
        assert!(!BlockSplit::Supported.is_pending());
        assert!(BlockSplit::Supported.is_release_ready());
        assert_eq!(BlockSplit::Supported.detail(), None);

        let unsupported = BlockSplit::Unsupported("no coarse blocks");
        assert!(!unsupported.is_supported());
        assert!(!unsupported.is_pending());
        assert!(unsupported.is_release_ready());
        assert_eq!(unsupported.detail(), Some("no coarse blocks"));

        let pending = BlockSplit::Pending("lands in plans/PI.md G3");
        assert!(!pending.is_supported());
        assert!(pending.is_pending());
        // A Pending gap is honest but not release-ready.
        assert!(!pending.is_release_ready());
        assert_eq!(pending.detail(), Some("lands in plans/PI.md G3"));
    }

    #[test]
    fn access_tracking_helpers_classify_each_declaration() {
        assert!(AccessTracking::Supported.is_supported());
        assert!(!AccessTracking::Supported.is_pending());
        assert!(AccessTracking::Supported.is_release_ready());
        assert_eq!(AccessTracking::Supported.detail(), None);

        let unsupported = AccessTracking::Unsupported("no referenced bit");
        assert!(!unsupported.is_supported());
        assert!(!unsupported.is_pending());
        assert!(unsupported.is_release_ready());
        assert_eq!(unsupported.detail(), Some("no referenced bit"));

        let pending = AccessTracking::Pending("software AF fault in b1a");
        assert!(!pending.is_supported());
        assert!(pending.is_pending());
        // A Pending gap is honest but not release-ready.
        assert!(!pending.is_release_ready());
        assert_eq!(pending.detail(), Some("software AF fault in b1a"));
    }

    #[test]
    fn kernel_window_refuses_a_misaligned_empty_or_overflowing_extent() {
        assert_eq!(KernelWindow::new(0x4000_0001, 4), None);
        assert_eq!(KernelWindow::new(0x4000_0000, 0), None);
        // The exclusive top must be representable, so the very last page
        // of the address space is refused rather than wrapped.
        assert_eq!(KernelWindow::new(u64::MAX - 0xFFF, 1), None);
        assert!(KernelWindow::new(u64::MAX - 0x1FFF, 1).is_some());
    }

    #[test]
    fn kernel_window_locates_addresses_inside_it_only() {
        let window = KernelWindow::new(0x80_0000_0000, 3).expect("valid window");
        assert_eq!(window.base(), 0x80_0000_0000);
        assert_eq!(window.pages(), 3);
        assert_eq!(window.len_bytes(), 3 * 4096);

        assert!(window.contains(0x80_0000_0000));
        assert!(window.contains(0x80_0000_2FFF));
        assert!(!window.contains(0x80_0000_3000), "the exclusive top");
        assert!(!window.contains(0x7F_FFFF_FFFF), "one byte below");

        assert_eq!(window.page_index(0x80_0000_0000), Some(0));
        assert_eq!(window.page_index(0x80_0000_1FFF), Some(1));
        assert_eq!(window.page_index(0x80_0000_2000), Some(2));
        assert_eq!(window.page_index(0x80_0000_3000), None);
        assert_eq!(window.page_index(0), None);
    }
}
