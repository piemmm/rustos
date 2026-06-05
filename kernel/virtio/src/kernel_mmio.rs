//! In-kernel [`MmioMapper`] backed by a per-process [`MmioMap`].
//!
//! Stage 4.D Item 3 wiring: a bus driver (`drivers/bus/pci`,
//! `drivers/bus/mmio`) reaches a device's register block through the
//! [`MmioMapper`] ABI seam. The always-available path is this
//! [`KernelMmioMapper`], which routes every request through the
//! capability-gated [`rustos_kernel_sec::map_mmio`] (`AGENTS.md`
//! §5.4): it verifies [`CapabilityId::MMIO_MAP`], maps the device's
//! physical register block into the driver's address space with
//! caching disabled, and mints a [`RegisterWindow`] over the result.
//!
//! The driver never synthesises a pointer: the only constructor of a
//! [`RegisterWindow`] is `unsafe` and is called solely here, after
//! the kernel has validated the mapping (`AGENTS.md` §4 — no ambient
//! authority).
//!
//! # Lifetime / leak contract
//!
//! A [`RegisterWindow`] carries a raw pointer into the [`MmioMap`]'s
//! window backing but no lifetime in its type and no free-on-drop
//! shim (unlike [`rustos_virtio::DmaSlab`]). A device's register window
//! lives for the whole duration of a driver load — it is not a
//! transient allocation — so the mapper retains every mapped region
//! and the kernel reclaims them when the driver's [`MmioMap`] (and
//! its address space) is torn down at unload. The borrow of the
//! mapper (`&'a mut MmioMap`) outlives every window the borrow
//! checker can observe, and the mapper's window backing is allocated
//! once at construction and never resized, so the pointer a window
//! holds stays valid for the mapper's lifetime.
//!
//! [`MmioMapper`]: rustos_abi::MmioMapper
//! [`CapabilityId::MMIO_MAP`]: rustos_abi::CapabilityId::MMIO_MAP

use core::cell::RefCell;

use rustos_abi::{MmioMapError, MmioMapper, RegisterWindow};
use rustos_kernel_mem::{MmioError, MmioMap, PageTable};
use rustos_kernel_sec::captable::TaskCapabilities;
use rustos_kernel_sec::mmio::{map_mmio, MmioGateError};
use rustos_log::Sink;

/// Capability-checked, [`MmioMap`]-backed [`MmioMapper`].
///
/// Generic over the page-table backend `P` (so the same code is
/// exercised by `kernel/mem::HostPageTable` in unit tests and by the
/// architecture page-table types in production) and the audit
/// [`Sink`] implementation `S`.
pub struct KernelMmioMapper<'a, 'p, P: PageTable, S: Sink + ?Sized> {
    map: RefCell<&'a mut MmioMap<'p, P>>,
    caller: &'a TaskCapabilities,
    audit: &'a S,
}

impl<'a, 'p, P: PageTable, S: Sink + ?Sized> KernelMmioMapper<'a, 'p, P, S> {
    /// Wrap a borrowed [`MmioMap`] in a capability-checking mapper.
    ///
    /// `caller` is the [`TaskCapabilities`] of the bus-driver task;
    /// every map request is audited against this capability set.
    ///
    /// The `'p` lifetime is the [`MmioMap`]'s borrow of the direct
    /// physical map; it is kept distinct from the mapper's own borrow
    /// `'a` so the caller may hold the underlying `PhysMap` for longer
    /// than the mapper.
    #[must_use]
    pub fn new(map: &'a mut MmioMap<'p, P>, caller: &'a TaskCapabilities, audit: &'a S) -> Self {
        Self {
            map: RefCell::new(map),
            caller,
            audit,
        }
    }

    /// Number of register windows currently mapped through this
    /// mapper.
    #[must_use]
    pub fn live(&self) -> usize {
        self.map.borrow().live()
    }
}

impl<P: PageTable, S: Sink + ?Sized> MmioMapper for KernelMmioMapper<'_, '_, P, S> {
    fn map_window(&self, phys_base: u64, len: usize) -> Result<RegisterWindow, MmioMapError> {
        let region = {
            let mut map = self.map.borrow_mut();
            map_mmio(*map, self.caller, phys_base, len, self.audit).map_err(map_gate_error)?
        };
        // `region_base` cannot fail for a region minted one statement
        // above, but the result is plumbed through so an allocator-
        // internal inconsistency surfaces as `Unsupported` instead of
        // a panic (`AGENTS.md` §2.9).
        let base = self
            .map
            .borrow()
            .region_base(&region)
            .map_err(|_| MmioMapError::Unsupported)?;
        // SAFETY: `base` points at the device register block the
        // kernel just mapped; it covers exactly `region.len()` bytes
        // (the mapper's slot bitmap proves disjointness from every
        // other live window), the mapper's window backing outlives
        // every window it mints (see the module-level lifetime
        // contract), and `region.phys()` is the device-visible base
        // of `base[0]`.
        let window = unsafe { RegisterWindow::from_mapping(region.phys(), base, region.len()) };
        Ok(window)
    }
}

/// Map a [`MmioGateError`] to the ABI [`MmioMapError`] the bus driver
/// observes.
fn map_gate_error(e: MmioGateError) -> MmioMapError {
    match e {
        MmioGateError::CapabilityMissing => MmioMapError::CapabilityMissing,
        MmioGateError::Map(MmioError::InvalidRegion) => MmioMapError::InvalidRegion,
        // Every other mapper failure (no virtual space, page-table
        // error, guard violation, bad config) is reported as
        // `Unsupported`: the bus driver has no recovery action beyond
        // surfacing it. The wildcard also keeps this total against the
        // `#[non_exhaustive]` `MmioGateError` without a panic
        // (`AGENTS.md` §2.9).
        _ => MmioMapError::Unsupported,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use alloc::vec::Vec;
    use core::cell::RefCell as StdRefCell;
    use rustos_abi::CapabilityId;
    use rustos_caps::CapabilitySet;
    use rustos_kernel_mem::{AddressSpace, HostPageTable, PhysAddr, SimPhysMap, VirtAddr};
    use rustos_kernel_sec::captable::{TaskCapabilities, TaskId};
    use rustos_kernel_sec::identity::UserId;
    use rustos_log::{Event, Sink};

    struct Recorder {
        events: StdRefCell<Vec<u32>>,
    }
    impl Recorder {
        fn new() -> Self {
            Self {
                events: StdRefCell::new(Vec::new()),
            }
        }
        fn ids(&self) -> Vec<u32> {
            self.events.borrow().clone()
        }
    }
    impl Sink for Recorder {
        fn write_event(&self, event: &Event<'_>) {
            self.events.borrow_mut().push(event.id.0);
        }
    }

    const MMIO_MAPPED_ID: u32 = 1040;
    const MMIO_MAP_DENIED_ID: u32 = 1041;

    /// Simulated register block covering the BAR addresses the tests
    /// map (`0xFEBD_0000` and `0xFEBE_0000`).
    fn fresh_sim() -> SimPhysMap {
        SimPhysMap::new(PhysAddr::new(0xFEBD_0000), 0x4_0000)
    }

    fn fresh_map(phys: &SimPhysMap) -> MmioMap<'_, HostPageTable> {
        MmioMap::new(
            AddressSpace::new(HostPageTable::new()),
            VirtAddr::new(0x6000_0000),
            16,
            phys,
        )
        .expect("mapper constructs")
    }

    fn task_with(caps: &[CapabilityId], sink: &Recorder) -> TaskCapabilities {
        let mut set = CapabilitySet::empty();
        for c in caps {
            set.insert(*c);
        }
        TaskCapabilities::derive(TaskId(7), UserId(1000), set, set, sink)
    }

    #[test]
    fn map_window_grants_and_round_trips() {
        let sim = fresh_sim();
        let mut map = fresh_map(&sim);
        let sink = Recorder::new();
        let caller = task_with(&[CapabilityId::MMIO_MAP], &sink);
        let mapper = KernelMmioMapper::new(&mut map, &caller, &sink);
        let window = mapper.map_window(0xFEBD_0000, 0x1000).expect("granted");
        assert_eq!(window.phys_base(), 0xFEBD_0000);
        assert_eq!(window.len(), 0x1000);
        window.write_u32(0x20, 0x1234_ABCD).expect("in bounds");
        assert_eq!(window.read_u32(0x20).expect("in bounds"), 0x1234_ABCD);
        assert_eq!(mapper.live(), 1);
        assert!(sink.ids().contains(&MMIO_MAPPED_ID));
    }

    #[test]
    fn map_window_without_capability_is_permission_denied() {
        let sim = fresh_sim();
        let mut map = fresh_map(&sim);
        let sink = Recorder::new();
        let caller = task_with(&[CapabilityId::DRV_LOAD], &sink);
        let mapper = KernelMmioMapper::new(&mut map, &caller, &sink);
        let err = mapper.map_window(0xFEBD_0000, 0x1000).unwrap_err();
        assert_eq!(err, MmioMapError::CapabilityMissing);
        assert_eq!(mapper.live(), 0);
        assert!(sink.ids().contains(&MMIO_MAP_DENIED_ID));
    }

    #[test]
    fn zero_length_request_is_invalid_region() {
        let sim = fresh_sim();
        let mut map = fresh_map(&sim);
        let sink = Recorder::new();
        let caller = task_with(&[CapabilityId::MMIO_MAP], &sink);
        let mapper = KernelMmioMapper::new(&mut map, &caller, &sink);
        let err = mapper.map_window(0xFEBD_0000, 0).unwrap_err();
        assert_eq!(err, MmioMapError::InvalidRegion);
    }

    #[test]
    fn exhausted_window_is_unsupported() {
        // Capacity 4 pages: one 0x1000 window fits (1 data + 2 guard);
        // a second cannot.
        let sim = fresh_sim();
        let mut map = MmioMap::new(
            AddressSpace::new(HostPageTable::new()),
            VirtAddr::new(0x6000_0000),
            4,
            &sim,
        )
        .expect("mapper constructs");
        let sink = Recorder::new();
        let caller = task_with(&[CapabilityId::MMIO_MAP], &sink);
        let mapper = KernelMmioMapper::new(&mut map, &caller, &sink);
        let _first = mapper.map_window(0xFEBD_0000, 0x1000).expect("first fits");
        let err = mapper.map_window(0xFEBE_0000, 0x1000).unwrap_err();
        assert_eq!(err, MmioMapError::Unsupported);
    }

    #[test]
    fn two_windows_are_disjoint() {
        let sim = fresh_sim();
        let mut map = fresh_map(&sim);
        let sink = Recorder::new();
        let caller = task_with(&[CapabilityId::MMIO_MAP], &sink);
        let mapper = KernelMmioMapper::new(&mut map, &caller, &sink);
        let a = mapper.map_window(0xFEBD_0000, 0x1000).expect("first");
        let b = mapper.map_window(0xFEBE_0000, 0x1000).expect("second");
        a.write_u32(0, 0xAAAA_AAAA).expect("write a");
        b.write_u32(0, 0xBBBB_BBBB).expect("write b");
        assert_eq!(a.read_u32(0).expect("read a"), 0xAAAA_AAAA);
        assert_eq!(b.read_u32(0).expect("read b"), 0xBBBB_BBBB);
        assert_eq!(mapper.live(), 2);
    }

    #[test]
    fn map_gate_error_maps_to_abi() {
        assert_eq!(
            map_gate_error(MmioGateError::CapabilityMissing),
            MmioMapError::CapabilityMissing
        );
        assert_eq!(
            map_gate_error(MmioGateError::Map(MmioError::InvalidRegion)),
            MmioMapError::InvalidRegion
        );
        assert_eq!(
            map_gate_error(MmioGateError::Map(MmioError::NoVirtualSpace)),
            MmioMapError::Unsupported
        );
    }
}
