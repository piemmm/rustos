//! Direct physical-memory map: turning a device-visible physical
//! address into a CPU-dereferenceable pointer.
//!
//! `kernel/mem`'s DMA pool ([`crate::dma::DmaPool`]) and MMIO mapper
//! ([`crate::mmio::MmioMap`]) both hand a *device* a physical address
//! (a DMA buffer's frame, a register block's BAR) and then need the
//! CPU to read and write the **same bytes** the device touches. On
//! real hardware those bytes are reachable because the kernel keeps a
//! direct map of physical memory: a fixed virtual window in the active
//! page table where a physical address `p` is reachable at `p +
//! offset`. The `x86_64` port's window is in the kernel half
//! (`kernel/arch/x86_64` `paging::PHYSMAP_VMA_BASE`), so `offset` is
//! non-zero there; the identity-linked `aarch64` and `riscv64` ports
//! carry an identity window, so `offset == 0`.
//!
//! This module is the seam between "a `PhysAddr` a device understands"
//! and "a `NonNull<u8>` the CPU can dereference". Production wires a
//! [`DirectPhysMap`] describing the boot direct map; host unit tests
//! wire a `SimPhysMap` that owns a real allocation standing in for
//! physical RAM, so the very pointer a test writes "as the device"
//! aliases the pointer the pool hands the driver (one model exercised in both worlds).
//!
//! The translation is the only place a physical address becomes a
//! pointer; callers route every dereference through the returned
//! [`NonNull`] and the crate's bounds-checked pointer helpers
//! (no raw pointer arithmetic without a
//! bounds-checked wrapper).

use core::ptr::NonNull;

use crate::frame::{PhysAddr, PAGE_SIZE};

/// Translates a device-visible [`PhysAddr`] into a CPU pointer valid
/// for `len` bytes within the kernel's direct physical map.
///
/// Returning [`None`] means `[phys, phys + len)` lies outside the
/// direct map; callers fail closed rather than synthesising a pointer
/// of their own.
pub trait PhysMap {
    /// Map `[phys, phys + len)` to a CPU pointer, or [`None`] if the
    /// range is not covered by the direct map.
    fn translate(&self, phys: PhysAddr, len: usize) -> Option<NonNull<u8>>;

    /// Clean and invalidate the direct-map alias of `[phys, phys + len)` to
    /// the point of coherency after the kernel has written bytes that a
    /// non-coherent DMA master — or a non-cacheable user mapping of the
    /// same frames — will also access.
    ///
    /// The DMA carve path zeroes freshly allocated (and freed) buffers
    /// through the *cacheable* direct-map alias while the driver reaches
    /// the same frames through a Normal-Non-Cacheable user mapping. On a
    /// non-I/O-coherent platform the dirty zero lines that zeroing leaves
    /// behind are written back at an arbitrary later time, silently
    /// overwriting rings and descriptors the driver has since published
    /// (the Pi 4 xHCI command ring went dead exactly this way). Every
    /// implementation must therefore state its coherence decision
    /// explicitly: a real cache clean+invalidate on a port whose DMA
    /// masters are not I/O-coherent, or a documented no-op where no
    /// incoherent alias can exist. There is deliberately no default — a
    /// silently inherited no-op is how the defect above shipped.
    fn clean_invalidate(&self, phys: PhysAddr, len: usize);

    /// Make bytes the kernel just wrote through the direct-map alias of
    /// `[phys, phys + len)` visible to **instruction fetch**, for a range
    /// that will be executed (a freshly-loaded program's code pages).
    ///
    /// The process loader fills code pages through the *cacheable* direct
    /// map, so the new instructions sit in the data cache. On a target whose
    /// instruction cache is not coherent with those data-side writes (the
    /// Cortex-A72), the PE can fetch stale instruction-cache lines — or
    /// memory that has not reached the point of unification — and take an
    /// EC=0 "unknown/unallocated instruction" abort on valid code. Every
    /// implementation must therefore state its coherence decision explicitly:
    /// a real clean-to-PoU + instruction-cache-invalidate on such a port, or
    /// a documented no-op where the instruction cache is coherent with kernel
    /// writes (x86_64) or the frames are never executed (host sims). There is
    /// deliberately no default — a silently inherited no-op is exactly how a
    /// non-coherent port would ship the stale-code wedge.
    fn sync_instruction_cache(&self, phys: PhysAddr, len: usize);

    /// Recover the [`PhysAddr`] a direct-map virtual address `virt` names,
    /// or [`None`] when this map cannot invert the translation.
    ///
    /// This is the inverse of [`translate`](Self::translate) for the region
    /// the map covers. The growable kernel heap uses it to hand a drained,
    /// direct-mapped region back to the frame allocator: it knows only the
    /// chunk's virtual base, and must recover the physical frame to free it.
    /// The default is [`None`] (a map that is not a simple linear direct map
    /// cannot invert), so a consumer that needs the inverse fails closed
    /// rather than synthesising an address of its own.
    fn reverse(&self, _virt: usize) -> Option<PhysAddr> {
        None
    }
}

/// The kernel's direct physical map: physical `p` is reachable at the
/// virtual address `p + offset`, for every `p` in `[base, limit)`.
///
/// `offset == 0` describes an identity map (what the identity-linked
/// `aarch64` and `riscv64` ports carry); a non-zero `offset` describes a
/// kernel-half direct map (what `x86_64` installs).
///
/// # A pointer, not an address
///
/// The map carries a *pointer* to the first byte it addresses. Its window
/// is not a Rust allocation — it exists because the port wrote page tables
/// for it — so the layer that knows that fact mints the pointer once
/// ([`Self::new`]) and every translation is derived from it. A `translate`
/// that rebuilt a pointer from a physical address per call would hand the
/// compiler one it believes aliases nothing, licensing it to reorder or
/// elide the DMA-visible writes the pool and the loader make through it,
/// and no interpreter could check the result.
#[derive(Debug, Clone, Copy)]
pub struct DirectPhysMap {
    root: NonNull<u8>,
    base: u64,
    limit: u64,
}

// SAFETY: `root` addresses the direct map the port installed in every
// translation root it builds, so it resolves identically on whichever CPU
// the holder runs. The descriptor is immutable and hands out no exclusive
// access, so sharing one grants nothing the bare offset it replaced did not.
unsafe impl Send for DirectPhysMap {}
// SAFETY: as `Send` above.
unsafe impl Sync for DirectPhysMap {}

impl DirectPhysMap {
    /// Lowest physical address a map at `offset` can hand out a pointer
    /// for.
    ///
    /// Zero for a windowed map. An identity window's alias of physical
    /// zero is the null pointer, which names nothing, so an identity map
    /// addresses from its second page — the same page the frame allocator
    /// permanently reserves for this reason.
    #[must_use]
    pub const fn addressable_base(offset: u64) -> u64 {
        if offset == 0 {
            PAGE_SIZE as u64
        } else {
            0
        }
    }

    /// Whether a map at `offset` covering physical `[0, limit)` is one this
    /// kernel can hold: at least one addressable page, and every byte of it
    /// reachable by a pointer.
    ///
    /// Checked where the map is *declared* rather than at each translate,
    /// so a window no pointer could address is refused up front instead of
    /// silently failing every caller later.
    #[must_use]
    pub const fn is_representable(offset: u64, limit: u64) -> bool {
        let base = Self::addressable_base(offset);
        if limit <= base {
            return false;
        }
        let Some(top) = offset.checked_add(limit) else {
            return false;
        };
        // `usize as u64` is lossless on every target, so the comparison
        // needs no checked conversion.
        top - 1 <= usize::MAX as u64
    }

    /// Build a direct map where physical `p` is reachable at `p + offset`,
    /// valid for physical addresses below `limit`, minting the pointer its
    /// translations derive from.
    ///
    /// Returns `None` — never a truncated or wrapped window — unless
    /// [`Self::is_representable`] accepts the extent.
    ///
    /// # Safety
    ///
    /// The caller must have installed a direct map of physical
    /// `[0, limit)` at `[offset, offset + limit)` in every translation
    /// root it builds, and must keep it live for as long as the map is
    /// used, so every derived pointer names the physical byte it claims.
    #[must_use]
    pub unsafe fn new(offset: u64, limit: u64) -> Option<Self> {
        if !Self::is_representable(offset, limit) {
            return None;
        }
        let base = Self::addressable_base(offset);
        let addr = usize::try_from(offset.checked_add(base)?).ok()?;
        // The one int-to-pointer step in the chain, stated where the fact
        // that this window exists is known rather than re-derived by each
        // translate.
        let root = NonNull::new(core::ptr::with_exposed_provenance_mut::<u8>(addr))?;
        Some(Self { root, base, limit })
    }

    /// Build an identity direct map (`offset == 0`) covering
    /// `[0, limit)` — the shape an identity-linked port carries.
    ///
    /// # Safety
    ///
    /// As [`Self::new`], with `offset == 0`.
    #[must_use]
    pub unsafe fn identity(limit: u64) -> Option<Self> {
        // SAFETY: the caller's obligation, forwarded unchanged.
        unsafe { Self::new(0, limit) }
    }

    /// Build a map over memory the caller already holds a pointer to, so a
    /// host test can drive a direct-map consumer with no port under it.
    ///
    /// `root` stands for physical `base`; the map addresses
    /// `[base, limit)`.
    ///
    /// # Safety
    ///
    /// `root` must be valid for reads and writes across `limit - base`
    /// bytes for as long as the map is used.
    #[cfg(any(test, feature = "host-tests"))]
    #[must_use]
    pub unsafe fn from_root(root: NonNull<u8>, base: u64, limit: u64) -> Option<Self> {
        if limit <= base || usize::try_from(limit - base).is_err() {
            return None;
        }
        Some(Self { root, base, limit })
    }
}

impl PhysMap for DirectPhysMap {
    fn translate(&self, phys: PhysAddr, len: usize) -> Option<NonNull<u8>> {
        let start = phys.as_u64();
        if start < self.base {
            return None;
        }
        let end = start.checked_add(u64::try_from(len).ok()?)?;
        if end > self.limit {
            return None;
        }
        let rel = usize::try_from(start - self.base).ok()?;
        // SAFETY: `rel` is below `limit - base`, which the constructor
        // proved the root is valid across, so the step lands inside the
        // window the port installed.
        Some(unsafe { self.root.byte_add(rel) })
    }

    fn reverse(&self, virt: usize) -> Option<PhysAddr> {
        // Invert `translate`: how far past the root an address sits is how
        // far past `base` its physical address sits. Only addresses inside
        // the window invert; anything else fails closed rather than
        // yielding a bogus frame.
        let rel = u64::try_from(virt.checked_sub(self.root.addr().get())?).ok()?;
        let phys = self.base.checked_add(rel)?;
        if phys >= self.limit {
            return None;
        }
        Some(PhysAddr::new(phys))
    }

    fn clean_invalidate(&self, _phys: PhysAddr, _len: usize) {
        // Deliberate no-op: `DirectPhysMap` carries no architecture handle,
        // so it can serve only I/O-coherent configurations (x86_64, the
        // QEMU `virt` boards) and purely cacheable uses (image build,
        // copy-out). A port whose DMA masters are not I/O-coherent (the
        // Pi 4's BCM2711 PCIe) must wire a `PhysMap` that wraps its cache
        // maintenance primitive instead — the aarch64
        // `ConfiguredPhysMap` — anywhere DMA buffers are zeroed
        // through the direct map.
    }

    fn sync_instruction_cache(&self, _phys: PhysAddr, _len: usize) {
        // Deliberate no-op: `DirectPhysMap` serves only targets whose
        // instruction cache is coherent with kernel data writes (x86_64) or
        // configurations that never execute freshly-loaded code through it
        // (the QEMU `virt` boards, which are I-cache-coherent). A port whose
        // I-cache is not coherent with the loader's cacheable writes (the
        // Pi 4's Cortex-A72) wires a `PhysMap` that performs the real
        // clean-to-PoU + I-cache-invalidate instead — the aarch64
        // `ConfiguredPhysMap`.
    }
}

/// Host-test stand-in for physical RAM.
///
/// Owns a single contiguous, page-aligned allocation representing the
/// physical range `[base, base + len)`. [`translate`](Self::translate)
/// returns a pointer into that allocation, so a unit test can write
/// bytes "as the device" at a physical address and observe them
/// through the same pool accessor a driver would use — the property
/// that makes the DMA/MMIO model hardware-faithful on the host.
#[cfg(any(test, feature = "host-tests"))]
pub struct SimPhysMap {
    base: u64,
    len: usize,
    storage: alloc::vec::Vec<u8>,
    align_pad: usize,
}

#[cfg(any(test, feature = "host-tests"))]
impl SimPhysMap {
    /// Allocate `len` bytes of simulated physical RAM mapped at
    /// physical `base`. The logical window is page-aligned so a
    /// [`crate::mmio::MmioMap`] register window minted over it meets
    /// its word-access alignment contract.
    ///
    /// # Panics
    ///
    /// Panics (test-only) if `len` is zero; a simulator covering no
    /// bytes is a test bug.
    #[must_use]
    pub fn new(base: PhysAddr, len: usize) -> Self {
        assert!(len != 0, "SimPhysMap needs a non-empty window");
        let raw = len + crate::frame::PAGE_SIZE;
        let storage = alloc::vec![0u8; raw];
        let align_pad = storage.as_ptr().align_offset(crate::frame::PAGE_SIZE);
        assert!(
            align_pad <= crate::frame::PAGE_SIZE,
            "over-allocation guarantees an aligned base"
        );
        Self {
            base: base.as_u64(),
            len,
            storage,
            align_pad,
        }
    }
}

#[cfg(any(test, feature = "host-tests"))]
impl PhysMap for SimPhysMap {
    fn translate(&self, phys: PhysAddr, len: usize) -> Option<NonNull<u8>> {
        let p = phys.as_u64();
        if p < self.base {
            return None;
        }
        let rel = usize::try_from(p - self.base).ok()?;
        let end = rel.checked_add(len)?;
        if end > self.len {
            return None;
        }
        let off = rel.checked_add(self.align_pad)?;
        // Mirror the host-model pointer pattern used by `DmaPool` and
        // `MmioMap`: the backing `Vec` is allocated once and never
        // resized, so a pointer into it stays valid for the
        // simulator's lifetime. Writes through it are sound because
        // the pool's slot bitmap proves the covered bytes alias
        // nothing else live.
        let ptr = self.storage.as_ptr().wrapping_add(off).cast_mut();
        NonNull::new(ptr)
    }

    fn reverse(&self, virt: usize) -> Option<PhysAddr> {
        // Invert `translate`: a pointer into the simulator's backing `Vec`
        // maps back to the physical address it stands for. Only pointers
        // inside `[base, base + len)` invert; anything else fails closed.
        let origin = self.storage.as_ptr().wrapping_add(self.align_pad) as usize;
        let rel = virt.checked_sub(origin)?;
        if rel >= self.len {
            return None;
        }
        Some(PhysAddr::new(self.base + rel as u64))
    }

    fn clean_invalidate(&self, _phys: PhysAddr, _len: usize) {
        // Deliberate no-op: the simulator's "physical RAM" is ordinary
        // host memory with no hardware cache alias to maintain.
    }

    fn sync_instruction_cache(&self, _phys: PhysAddr, _len: usize) {
        // Deliberate no-op: the simulator's "physical RAM" is ordinary host
        // memory that is never fetched as instructions, so there is no
        // instruction-cache alias to synchronise.
    }
}

#[cfg(all(test, not(loom)))]
mod tests {
    use super::*;

    /// A host stand-in for the window a port installs: real memory the map
    /// can be rooted in, so a translation is a derivation the interpreter
    /// can follow rather than a pointer minted from an integer.
    fn rooted(base: u64, pages: usize) -> (DirectPhysMap, alloc::vec::Vec<u8>) {
        let mut backing = alloc::vec![0u8; pages * PAGE_SIZE];
        let root = NonNull::new(backing.as_mut_ptr()).expect("non-null backing");
        let limit = base + (pages * PAGE_SIZE) as u64;
        // SAFETY: `root` owns `pages * PAGE_SIZE` bytes and `backing`
        // outlives every use the caller makes of the returned map.
        let map = unsafe { DirectPhysMap::from_root(root, base, limit) }.expect("rooted map");
        (map, backing)
    }

    #[test]
    fn direct_translate_derives_the_offset_from_its_root() {
        let (map, backing) = rooted(0, 4);
        let p = map
            .translate(PhysAddr::new(PAGE_SIZE as u64), PAGE_SIZE)
            .expect("mapped");
        assert_eq!(
            p.addr().get() - backing.as_ptr() as usize,
            PAGE_SIZE,
            "the pointer sits one page past the root"
        );
    }

    /// The window's bytes are writable *through the very pointer*
    /// `translate` returned — the property an address-only assertion could
    /// never make.
    #[test]
    fn direct_translate_addresses_writable_window_bytes() {
        let (map, mut backing) = rooted(0, 2);
        let p = map.translate(PhysAddr::new(8), 4).expect("mapped");
        // SAFETY: the translate proved `[8, 12)` is inside the window, and
        // `backing` is not otherwise borrowed across the write.
        unsafe { p.write_bytes(0xC3, 4) };
        assert_eq!(&backing[8..12], &[0xC3; 4]);
        backing[0] = 0;
    }

    /// An identity map's alias of physical zero is the null pointer, so the
    /// map addresses from its second page. This is the hazard the frame
    /// allocator's permanent zero-page reservation defends against — a zero
    /// frame handed to the page-table source would be returned and re-drawn
    /// forever.
    #[test]
    fn an_identity_map_does_not_address_physical_page_zero() {
        assert_eq!(DirectPhysMap::addressable_base(0), PAGE_SIZE as u64);
        assert_eq!(DirectPhysMap::addressable_base(0x1_0000_0000), 0);
        let (map, _backing) = rooted(PAGE_SIZE as u64, 2);
        assert!(map.translate(PhysAddr::new(0), PAGE_SIZE).is_none());
        assert!(map
            .translate(PhysAddr::new(PAGE_SIZE as u64 - 1), 1)
            .is_none());
    }

    /// An extent no pointer could address is refused where the map is
    /// declared, not silently at every translate.
    #[test]
    fn an_unrepresentable_extent_is_refused_at_declaration() {
        assert!(DirectPhysMap::is_representable(
            0x1_0000_0000,
            0x2_0000_0000
        ));
        assert!(DirectPhysMap::is_representable(0, 2 * PAGE_SIZE as u64));
        // An identity map covering only the page it cannot address.
        assert!(!DirectPhysMap::is_representable(0, PAGE_SIZE as u64));
        assert!(!DirectPhysMap::is_representable(0, 0));
        // The window's exclusive top must not wrap.
        assert!(!DirectPhysMap::is_representable(u64::MAX - 0xFFF, 0x2000));
        // A zero-span or unrepresentable root is refused too.
        let mut byte = 0u8;
        let root = NonNull::from(&mut byte);
        // SAFETY: the constructor is expected to refuse both extents before
        // it can derive anything from `root`.
        unsafe {
            assert!(DirectPhysMap::from_root(root, 4, 4).is_none());
            assert!(DirectPhysMap::from_root(root, 4, 3).is_none());
        }
    }

    #[test]
    fn direct_rejects_range_past_limit() {
        let (map, _backing) = rooted(0, 2);
        let limit = 2 * PAGE_SIZE as u64;
        assert!(map.translate(PhysAddr::new(limit - 1), 2).is_none());
        assert!(map.translate(PhysAddr::new(limit), 1).is_none());
    }

    #[test]
    fn direct_reverse_inverts_translate() {
        // The growable heap hands a drained direct-mapped region back to the
        // frame allocator by recovering its physical base from its virtual
        // base: `reverse` must invert `translate` exactly, and fail closed
        // outside the mapped window.
        let (map, _backing) = rooted(0, 4);
        let phys = PhysAddr::new(2 * PAGE_SIZE as u64);
        let virt = map.translate(phys, 8).expect("mapped").addr().get();
        assert_eq!(map.reverse(virt), Some(phys));
        // Below the root and past the window both fail closed.
        assert!(map.reverse(virt - 2 * PAGE_SIZE - 1).is_none());
        assert!(map.reverse(virt + 2 * PAGE_SIZE).is_none());
    }

    #[test]
    fn sim_reverse_round_trips_a_translated_pointer() {
        let base = PhysAddr::new(PAGE_SIZE as u64 * 16);
        let sim = SimPhysMap::new(base, 4 * PAGE_SIZE);
        let target = PhysAddr::new(base.as_u64() + PAGE_SIZE as u64);
        let ptr = sim.translate(target, 4).expect("mapped").as_ptr() as usize;
        assert_eq!(sim.reverse(ptr), Some(target));
        // A pointer one byte below the mapped base fails closed.
        let base_ptr = sim.translate(base, 4).expect("mapped").as_ptr() as usize;
        assert!(sim.reverse(base_ptr - 1).is_none());
    }

    #[test]
    fn sim_aliases_writes_at_a_physical_address() {
        let base = PhysAddr::new(PAGE_SIZE as u64 * 16);
        let sim = SimPhysMap::new(base, 4 * PAGE_SIZE);
        let target = PhysAddr::new(base.as_u64() + PAGE_SIZE as u64);
        let a = sim.translate(target, 4).expect("a");
        let b = sim.translate(target, 4).expect("b");
        // SAFETY: both pointers name the same in-bounds simulated
        // frame; the simulator outlives this test body.
        unsafe {
            a.as_ptr().write(0xAB);
            assert_eq!(b.as_ptr().read(), 0xAB);
        }
    }

    #[test]
    fn sim_is_page_aligned_at_base() {
        let base = PhysAddr::new(0xFEBD_0000);
        let sim = SimPhysMap::new(base, PAGE_SIZE);
        let p = sim.translate(base, 4).expect("mapped");
        assert_eq!(p.as_ptr() as usize % PAGE_SIZE, 0);
    }

    #[test]
    fn sim_rejects_below_base_and_past_end() {
        let base = PhysAddr::new(PAGE_SIZE as u64 * 16);
        let sim = SimPhysMap::new(base, PAGE_SIZE);
        assert!(sim.translate(PhysAddr::new(0), 1).is_none());
        assert!(sim
            .translate(PhysAddr::new(base.as_u64()), PAGE_SIZE + 1)
            .is_none());
    }
}
