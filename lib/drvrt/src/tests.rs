//! Host tests for [`RtDriverHost`] over a stateful mock [`GrantSyscalls`].
//!
//! The mock backs each MMIO grant handle with a real, owned byte buffer and
//! returns its base as the "mapped" VA, so a [`RegisterWindow`] the host
//! builds reads and writes the right bytes at the right offset. DMA carves
//! are backed the same way. This exercises the host's grant resolution,
//! bus→CPU translation, map-once caching, and fail-closed paths without a
//! kernel (`AGENTS.md` §7).
//!
//! The tests run on the 64-bit host, where the geometry constants
//! (`u64` device addresses) cannot truncate when narrowed to a `usize`
//! length or sign-wrap when marshalled as the `i64` syscall result the mock
//! returns — so those host-only casts are allowed here rather than wrapped in
//! `try_from` noise that the production code (which never narrows) does not
//! need.
#![allow(clippy::cast_possible_truncation, clippy::cast_possible_wrap)]

use super::*;
use core::cell::{Cell, RefCell};
use std::rc::Rc;

use rustos_abi::driver::dma::{DmaHost, SlabCoherencyFn};
use rustos_abi::hwtree::HwResource;
use rustos_abi::{
    CapabilityId, DriverError, DriverHost, DriverKind, Errno, MmioMapError, MmioMapper,
};
use rustos_caps::CapabilitySet;

/// One programmed response for a grant handle: a heap buffer the mock maps,
/// plus the device-visible base it reports for a DMA carve.
struct Backing {
    handle: u64,
    buffer: Box<[u8]>,
    device_base: u64,
}

/// A stateful mock `GrantSyscalls`. The `Rc<Cell>` call counters are cloned
/// out before the mock is moved into the host so a test can assert how many
/// times each syscall ran (the map-once guarantee).
struct MockSyscalls {
    backings: RefCell<Vec<Backing>>,
    mmio_calls: Rc<Cell<usize>>,
    dma_calls: Rc<Cell<usize>>,
    /// The grant set `resource_grants` delivers (the kernel-minted grants the
    /// driver process would learn at start-up). A test populates it before
    /// building a host with `from_grants_query`.
    delivered: RefCell<Vec<GrantedResource>>,
    /// When `Some`, `resource_grants` returns this `-errno` instead of
    /// serialising `delivered` (to model a kernel refusal).
    grants_error: Cell<Option<Errno>>,
}

impl MockSyscalls {
    fn new() -> Self {
        Self {
            backings: RefCell::new(Vec::new()),
            mmio_calls: Rc::new(Cell::new(0)),
            dma_calls: Rc::new(Cell::new(0)),
            delivered: RefCell::new(Vec::new()),
            grants_error: Cell::new(None),
        }
    }

    /// Add `grant` to the set `resource_grants` will deliver.
    fn deliver(&self, grant: GrantedResource) {
        self.delivered.borrow_mut().push(grant);
    }

    /// Make `resource_grants` fail with `-err` instead of delivering grants.
    fn fail_grants(&self, err: Errno) {
        self.grants_error.set(Some(err));
    }

    /// Register a `len`-byte backing buffer for `handle`, reporting
    /// `device_base` as its device-visible base on a DMA carve. Returns the
    /// base VA the mock will map it at (the `Box`'s heap pointer is stable
    /// across later `Vec` growth, so the value stays valid after the mock
    /// moves into the host).
    fn back(&self, handle: u64, len: usize, device_base: u64) -> u64 {
        let buffer = vec![0u8; len].into_boxed_slice();
        let base = buffer.as_ptr() as usize as u64;
        self.backings.borrow_mut().push(Backing {
            handle,
            buffer,
            device_base,
        });
        base
    }

    fn mmio_counter(&self) -> Rc<Cell<usize>> {
        Rc::clone(&self.mmio_calls)
    }
}

impl GrantSyscalls for MockSyscalls {
    fn mmio_map(&self, handle: u64) -> i64 {
        self.mmio_calls.set(self.mmio_calls.get() + 1);
        let backings = self.backings.borrow();
        match backings.iter().find(|b| b.handle == handle) {
            Some(b) => b.buffer.as_ptr() as usize as i64,
            None => -i64::from(Errno::NotFound.as_i32()),
        }
    }

    fn dma_alloc(&self, handle: u64, len: usize, device_out: &mut u64) -> i64 {
        self.dma_calls.set(self.dma_calls.get() + 1);
        let backings = self.backings.borrow();
        match backings.iter().find(|b| b.handle == handle) {
            Some(b) if len <= b.buffer.len() => {
                *device_out = b.device_base;
                b.buffer.as_ptr() as usize as i64
            }
            Some(_) => -i64::from(Errno::OutOfMemory.as_i32()),
            None => -i64::from(Errno::NotFound.as_i32()),
        }
    }

    fn resource_grants(&self, buf: &mut [u8]) -> i64 {
        if let Some(err) = self.grants_error.get() {
            return -i64::from(err.as_i32());
        }
        let delivered = self.delivered.borrow();
        let total = delivered.len() * GrantedResource::WIRE_LEN;
        // Never deliver a partial set: fail closed exactly as the kernel
        // handler does (`AGENTS.md` §2.9).
        if total > buf.len() {
            return -i64::from(Errno::BufferTooSmall.as_i32());
        }
        for (i, grant) in delivered.iter().enumerate() {
            let off = i * GrantedResource::WIRE_LEN;
            buf[off..off + GrantedResource::WIRE_LEN].copy_from_slice(&grant.to_le_bytes());
        }
        total as i64
    }
}

fn caps(set: &[CapabilityId]) -> CapabilitySet {
    let mut c = CapabilitySet::empty();
    for cap in set {
        c.insert(*cap);
    }
    c
}

// A register block grant (CPU/identity space) and an outbound bus window
// modelled on the BCM2711 PCIe bridge geometry.
const REGS_HANDLE: u64 = 1;
const REGS_BASE: u64 = 0xFD50_0000;
const REGS_LEN: u64 = 0x9310;

const BUSWIN_HANDLE: u64 = 2;
const OUTBOUND_CPU_BASE: u64 = 0x6_0000_0000;
const OUTBOUND_PCIE_BASE: u64 = 0xF800_0000;
const OUTBOUND_SIZE: u64 = 0x400_0000;

const DMA_HANDLE: u64 = 3;
const DMA_ADDR_LIMIT: u64 = 0x4000_0000;
const DMA_DEVICE_BASE: u64 = 0x4_0000_0000;

fn regs_grant() -> GrantedResource {
    GrantedResource::new(REGS_HANDLE, HwResource::mmio(REGS_BASE, REGS_LEN))
}

fn buswin_grant() -> GrantedResource {
    GrantedResource::new(
        BUSWIN_HANDLE,
        HwResource::bus_window(OUTBOUND_CPU_BASE, OUTBOUND_SIZE, OUTBOUND_PCIE_BASE),
    )
}

fn dma_grant() -> GrantedResource {
    GrantedResource::new(DMA_HANDLE, HwResource::dma(DMA_ADDR_LIMIT, 0x4_0000))
}

#[test]
fn maps_a_register_block_at_offset_zero() {
    let mock = MockSyscalls::new();
    let base = mock.back(REGS_HANDLE, REGS_LEN as usize, 0);
    let host = RtDriverHost::new(caps(&[CapabilityId::MMIO_MAP]), mock, &[regs_grant()], None)
        .expect("grants fit");

    let window = host.map_window(REGS_BASE, 0x20).expect("regs map");
    assert_eq!(window.len(), 0x20);
    // The window records the device-visible base, not the host VA.
    assert_eq!(window.phys_base(), REGS_BASE);
    // A write through the window lands in the backing buffer at offset 0.
    window.write_u32(0, 0xDEAD_BEEF).expect("in-bounds write");
    let readback = unsafe { (base as *const u32).read() };
    assert_eq!(readback, 0xDEAD_BEEF);
}

#[test]
fn maps_a_sub_window_at_a_nonzero_offset() {
    let mock = MockSyscalls::new();
    let base = mock.back(REGS_HANDLE, REGS_LEN as usize, 0);
    let host =
        RtDriverHost::new(caps(&[CapabilityId::MMIO_MAP]), mock, &[regs_grant()], None).unwrap();

    let window = host.map_window(REGS_BASE + 0x100, 0x10).expect("sub map");
    assert_eq!(window.phys_base(), REGS_BASE + 0x100);
    window.write_u32(0, 0x0102_0304).expect("write");
    // The byte landed 0x100 into the backing buffer.
    let readback = unsafe { (base as *const u32).byte_add(0x100).read() };
    assert_eq!(readback, 0x0102_0304);
}

#[test]
fn translates_a_bar_inside_the_outbound_bus_window() {
    let mock = MockSyscalls::new();
    let base = mock.back(BUSWIN_HANDLE, OUTBOUND_SIZE as usize, 0);
    let host = RtDriverHost::new(
        caps(&[CapabilityId::MMIO_MAP]),
        mock,
        &[buswin_grant()],
        None,
    )
    .unwrap();

    // A BAR placed 0x1_0000 into the outbound PCIe window.
    let bar_pcie = OUTBOUND_PCIE_BASE + 0x1_0000;
    let window = host.map_window(bar_pcie, 0x1000).expect("bar map");
    // The window records the BAR's device-visible (PCIe-bus) base.
    assert_eq!(window.phys_base(), bar_pcie);
    window.write_u32(0, 0xCAFE_F00D).expect("write");
    // The CPU access lands at the same offset into the mapped CPU window.
    let readback = unsafe { (base as *const u32).byte_add(0x1_0000).read() };
    assert_eq!(readback, 0xCAFE_F00D);
}

#[test]
fn maps_each_window_only_once() {
    let mock = MockSyscalls::new();
    mock.back(REGS_HANDLE, REGS_LEN as usize, 0);
    let calls = mock.mmio_counter();
    let host =
        RtDriverHost::new(caps(&[CapabilityId::MMIO_MAP]), mock, &[regs_grant()], None).unwrap();

    let _ = host.map_window(REGS_BASE, 0x10).expect("first");
    let _ = host.map_window(REGS_BASE + 0x40, 0x10).expect("second");
    let _ = host.map_window(REGS_BASE + 0x80, 0x10).expect("third");
    assert_eq!(calls.get(), 1, "the granted window is mapped exactly once");
}

#[test]
fn map_window_without_capability_is_refused_before_any_syscall() {
    let mock = MockSyscalls::new();
    mock.back(REGS_HANDLE, REGS_LEN as usize, 0);
    let calls = mock.mmio_counter();
    let host = RtDriverHost::new(caps(&[]), mock, &[regs_grant()], None).unwrap();

    assert_eq!(
        host.map_window(REGS_BASE, 0x10).unwrap_err(),
        MmioMapError::CapabilityMissing
    );
    assert_eq!(calls.get(), 0, "capability checked before the syscall");
}

#[test]
fn map_window_rejects_zero_length() {
    let mock = MockSyscalls::new();
    mock.back(REGS_HANDLE, REGS_LEN as usize, 0);
    let host =
        RtDriverHost::new(caps(&[CapabilityId::MMIO_MAP]), mock, &[regs_grant()], None).unwrap();
    assert_eq!(
        host.map_window(REGS_BASE, 0).unwrap_err(),
        MmioMapError::InvalidRegion
    );
}

#[test]
fn map_window_with_no_covering_grant_is_refused() {
    let mock = MockSyscalls::new();
    mock.back(REGS_HANDLE, REGS_LEN as usize, 0);
    let host =
        RtDriverHost::new(caps(&[CapabilityId::MMIO_MAP]), mock, &[regs_grant()], None).unwrap();
    // A window far outside the only granted region.
    assert_eq!(
        host.map_window(0x1_0000_0000, 0x10).unwrap_err(),
        MmioMapError::InvalidRegion
    );
}

#[test]
fn map_window_overrunning_the_grant_is_refused() {
    let mock = MockSyscalls::new();
    mock.back(REGS_HANDLE, REGS_LEN as usize, 0);
    let host =
        RtDriverHost::new(caps(&[CapabilityId::MMIO_MAP]), mock, &[regs_grant()], None).unwrap();
    // The base is inside the grant but the length runs past its end.
    assert_eq!(
        host.map_window(REGS_BASE, REGS_LEN as usize + 1)
            .unwrap_err(),
        MmioMapError::InvalidRegion
    );
}

#[test]
fn map_window_surfaces_a_kernel_refusal_and_does_not_cache_it() {
    // The grant covers the request, but the (mock) kernel has no backing for
    // the handle, so `mmio_map` returns `-NotFound`.
    let mock = MockSyscalls::new();
    let calls = mock.mmio_counter();
    let host =
        RtDriverHost::new(caps(&[CapabilityId::MMIO_MAP]), mock, &[regs_grant()], None).unwrap();
    assert_eq!(
        host.map_window(REGS_BASE, 0x10).unwrap_err(),
        MmioMapError::InvalidRegion
    );
    // A failed map is not cached, so a retry re-issues the syscall.
    let _ = host.map_window(REGS_BASE, 0x10);
    assert_eq!(calls.get(), 2);
}

#[test]
fn carves_a_dma_buffer_against_the_dma_grant() {
    let mock = MockSyscalls::new();
    mock.back(DMA_HANDLE, 0x4000, DMA_DEVICE_BASE);
    let host =
        RtDriverHost::new(caps(&[CapabilityId::MEM_DMA]), mock, &[dma_grant()], None).unwrap();

    let mut slab = host.alloc_dma_zeroed(0x1000).expect("dma carve");
    assert_eq!(slab.len(), 0x1000);
    // The device-visible base is the mock's reported base, not the CPU VA.
    assert_eq!(slab.phys(), DMA_DEVICE_BASE);
    // The carve is genuine writable memory and starts zeroed.
    assert!(slab.as_bytes().iter().all(|&b| b == 0));
    slab.as_bytes_mut()[0] = 0xAB;
    assert_eq!(slab.as_bytes()[0], 0xAB);
}

#[test]
fn dma_without_capability_is_refused_before_any_syscall() {
    let mock = MockSyscalls::new();
    mock.back(DMA_HANDLE, 0x4000, DMA_DEVICE_BASE);
    let host = RtDriverHost::new(caps(&[]), mock, &[dma_grant()], None).unwrap();
    assert_eq!(
        host.alloc_dma_zeroed(0x1000).err(),
        Some(DriverError::PermissionDenied)
    );
}

#[test]
fn dma_rejects_zero_size() {
    let mock = MockSyscalls::new();
    mock.back(DMA_HANDLE, 0x4000, DMA_DEVICE_BASE);
    let host =
        RtDriverHost::new(caps(&[CapabilityId::MEM_DMA]), mock, &[dma_grant()], None).unwrap();
    assert_eq!(
        host.alloc_dma_zeroed(0).err(),
        Some(DriverError::LengthOutOfRange)
    );
}

#[test]
fn dma_without_a_dma_grant_is_unsupported() {
    // Only a register grant is held; there is no DMA constraint to carve
    // against.
    let mock = MockSyscalls::new();
    mock.back(REGS_HANDLE, REGS_LEN as usize, 0);
    let host =
        RtDriverHost::new(caps(&[CapabilityId::MEM_DMA]), mock, &[regs_grant()], None).unwrap();
    assert_eq!(
        host.alloc_dma_zeroed(0x1000).err(),
        Some(DriverError::Unsupported)
    );
}

#[test]
fn dma_exhaustion_maps_to_length_out_of_range() {
    let mock = MockSyscalls::new();
    mock.back(DMA_HANDLE, 0x1000, DMA_DEVICE_BASE);
    let host =
        RtDriverHost::new(caps(&[CapabilityId::MEM_DMA]), mock, &[dma_grant()], None).unwrap();
    // Larger than the mock's backing buffer → the mock returns -OutOfMemory.
    assert_eq!(
        host.alloc_dma_zeroed(0x4000).err(),
        Some(DriverError::LengthOutOfRange)
    );
}

thread_local! {
    static COHERENCY_HITS: Cell<usize> = const { Cell::new(0) };
}

fn record_coherency(_base: *const u8, _len: usize) {
    COHERENCY_HITS.with(|c| c.set(c.get() + 1));
}

#[test]
fn attached_coherency_shim_is_invoked_by_sync_range() {
    let mock = MockSyscalls::new();
    mock.back(DMA_HANDLE, 0x4000, DMA_DEVICE_BASE);
    let shim: SlabCoherencyFn = record_coherency;
    let host = RtDriverHost::new(
        caps(&[CapabilityId::MEM_DMA]),
        mock,
        &[dma_grant()],
        Some(shim),
    )
    .unwrap();
    COHERENCY_HITS.with(|c| c.set(0));
    let slab = host.alloc_dma_zeroed(0x1000).expect("carve");
    slab.sync_range(0, 0x100);
    assert_eq!(COHERENCY_HITS.with(Cell::get), 1);
}

#[test]
fn reports_capabilities_and_user_space_kind() {
    let mock = MockSyscalls::new();
    let host =
        RtDriverHost::new(caps(&[CapabilityId::MMIO_MAP]), mock, &[regs_grant()], None).unwrap();
    assert!(host.has_capability(CapabilityId::MMIO_MAP));
    assert!(!host.has_capability(CapabilityId::MEM_DMA));
    assert_eq!(host.kind(), DriverKind::UserSpace);
    assert!(host.mmio_mapper().is_some());
    assert!(host.virtio_host().is_some());
}

#[test]
fn rejects_an_over_long_grant_table() {
    let mock = MockSyscalls::new();
    let grants = [regs_grant(); MAX_GRANTS + 1];
    assert_eq!(
        RtDriverHost::new(caps(&[]), mock, &grants, None).err(),
        Some(DriverError::LengthOutOfRange)
    );
}

#[test]
fn resolves_the_right_grant_among_several() {
    // A host holding all three grants resolves an MMIO request to the
    // register grant, a bus request to the bus window, and a carve to the DMA
    // grant — never crossing them.
    let mock = MockSyscalls::new();
    mock.back(REGS_HANDLE, REGS_LEN as usize, 0);
    mock.back(BUSWIN_HANDLE, OUTBOUND_SIZE as usize, 0);
    mock.back(DMA_HANDLE, 0x4000, DMA_DEVICE_BASE);
    let host = RtDriverHost::new(
        caps(&[CapabilityId::MMIO_MAP, CapabilityId::MEM_DMA]),
        mock,
        &[regs_grant(), buswin_grant(), dma_grant()],
        None,
    )
    .unwrap();

    assert_eq!(
        host.map_window(REGS_BASE, 0x10).unwrap().phys_base(),
        REGS_BASE
    );
    let bar = OUTBOUND_PCIE_BASE + 0x2000;
    assert_eq!(host.map_window(bar, 0x10).unwrap().phys_base(), bar);
    assert_eq!(
        host.alloc_dma_zeroed(0x1000).unwrap().phys(),
        DMA_DEVICE_BASE
    );
}

// --- `from_grants_query` (the production start-up path, `plans/PI.md` 5d-2) --

#[test]
fn from_grants_query_builds_the_table_the_kernel_delivered() {
    // The kernel minted a register grant and a DMA grant for this driver and
    // delivers them through `resource_grants`; the host decodes the delivery
    // and maps/carves against exactly those grants.
    let mock = MockSyscalls::new();
    mock.deliver(regs_grant());
    mock.deliver(dma_grant());
    let base = mock.back(REGS_HANDLE, REGS_LEN as usize, 0);
    mock.back(DMA_HANDLE, 0x4000, DMA_DEVICE_BASE);
    let host = RtDriverHost::from_grants_query(
        caps(&[CapabilityId::MMIO_MAP, CapabilityId::MEM_DMA]),
        mock,
        None,
    )
    .expect("the delivered grants build a host");

    let window = host.map_window(REGS_BASE, 0x10).expect("regs map");
    assert_eq!(window.phys_base(), REGS_BASE);
    window.write_u32(0, 0x1234_5678).expect("write");
    let readback = unsafe { (base as *const u32).read() };
    assert_eq!(readback, 0x1234_5678);
    assert_eq!(
        host.alloc_dma_zeroed(0x1000).expect("carve").phys(),
        DMA_DEVICE_BASE
    );
}

#[test]
fn from_grants_query_with_no_grants_builds_a_host_that_maps_nothing() {
    // An unbound driver (the kernel minted no grants) is a valid, empty
    // result (`AGENTS.md` §18.4): the host builds, but any map is refused.
    let mock = MockSyscalls::new();
    let host = RtDriverHost::from_grants_query(caps(&[CapabilityId::MMIO_MAP]), mock, None)
        .expect("an empty grant set still builds a host");
    assert_eq!(
        host.map_window(REGS_BASE, 0x10).unwrap_err(),
        MmioMapError::InvalidRegion
    );
}

#[test]
fn from_grants_query_refuses_more_grants_than_the_cap() {
    // The kernel delivering more than `MAX_GRANTS` records cannot fit the
    // host's fixed buffer; the syscall reports `BufferTooSmall` and the host
    // fails closed as a packaging defect (`AGENTS.md` §2.9 / §24.4).
    let mock = MockSyscalls::new();
    for _ in 0..=MAX_GRANTS {
        mock.deliver(regs_grant());
    }
    assert_eq!(
        RtDriverHost::from_grants_query(caps(&[]), mock, None).err(),
        Some(DriverError::LengthOutOfRange)
    );
}

#[test]
fn from_grants_query_surfaces_a_kernel_refusal_fail_closed() {
    // Any other negative result from the syscall is a kernel refusal: the
    // host builds no grant table rather than guessing (`AGENTS.md` §2.9).
    let mock = MockSyscalls::new();
    mock.fail_grants(Errno::PermissionDenied);
    assert_eq!(
        RtDriverHost::from_grants_query(caps(&[]), mock, None).err(),
        Some(DriverError::Unsupported)
    );
}

#[test]
fn resources_exposes_the_granted_resources_in_delivery_order() {
    // A driver process derives its concrete bring-up inputs (the BAR window,
    // the DMA aperture) from the same grants the host maps over, without a
    // second `resource_grants` syscall (`AGENTS.md` §2.16).
    let mock = MockSyscalls::new();
    let host = RtDriverHost::new(
        caps(&[CapabilityId::MMIO_MAP, CapabilityId::MEM_DMA]),
        mock,
        &[buswin_grant(), dma_grant()],
        None,
    )
    .unwrap();

    let resources: Vec<HwResource> = host.resources().copied().collect();
    assert_eq!(
        resources,
        vec![buswin_grant().resource, dma_grant().resource]
    );
}

#[test]
fn resources_is_empty_for_an_unbound_driver() {
    let mock = MockSyscalls::new();
    let host = RtDriverHost::new(caps(&[]), mock, &[], None).unwrap();
    assert_eq!(host.resources().count(), 0);
}
