//! Host tests for [`RtDriverHost`] over a stateful mock [`GrantSyscalls`].
//!
//! The mock backs each MMIO grant handle with a real, owned byte buffer and
//! returns its base as the "mapped" VA, so a [`RegisterWindow`] the host
//! builds reads and writes the right bytes at the right offset. DMA carves
//! are backed the same way. This exercises the host's grant resolution,
//! bus→CPU translation, map-once caching, and fail-closed paths without a
//! kernel.
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

/// The three shared `irq_*` observers a test clones out of the mock before it
/// moves into the host: `(last bound line, irq_bind call count, irq_wait call
/// count)`.
type IrqObservers = (Rc<Cell<u32>>, Rc<Cell<u32>>, Rc<Cell<u32>>);

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
    /// The `(offset, len)` of the most recent `mmio_map` call, so a test can
    /// assert the host requested only the sub-region (not the whole grant).
    last_mmio: Rc<Cell<(u64, usize)>>,
    dma_calls: Rc<Cell<usize>>,
    /// Every CPU base address passed to `dma_free`, in call order. Shared so a
    /// test can assert each carve's slab freed itself on drop.
    dma_frees: Rc<RefCell<Vec<u64>>>,
    /// The grant set `resource_grants` delivers (the kernel-minted grants the
    /// driver process would learn at start-up). A test populates it before
    /// building a host with `from_grants_query`.
    delivered: RefCell<Vec<GrantedResource>>,
    /// When `Some`, `resource_grants` returns this `-errno` instead of
    /// serialising `delivered` (to model a kernel refusal).
    grants_error: Cell<Option<Errno>>,
    /// The last line passed to `irq_bind` (`0` if never called). Shared so a
    /// test can read it after the mock moves into the host.
    irq_line_bound: Rc<Cell<u32>>,
    /// How many times `irq_bind` was called (the cache must bind once).
    irq_bind_calls: Rc<Cell<u32>>,
    /// How many times `irq_wait` was called.
    irq_wait_calls: Rc<Cell<u32>>,
    /// The raw signed result `irq_bind` returns: a positive handle by
    /// default, or `-errno` to model a refused bind.
    irq_bind_result: Cell<i64>,
    /// The endpoint captured by the last `ipc_call`.
    ipc_endpoint: Cell<u64>,
    /// The request bytes captured by the last `ipc_call`.
    ipc_request: RefCell<Vec<u8>>,
    /// The reply bytes `ipc_call` copies back to the caller on success.
    ipc_reply: RefCell<Vec<u8>>,
    /// When `Some`, `ipc_call` returns this `-errno` instead of replying.
    ipc_error: Cell<Option<Errno>>,
    /// Every node passed to `hw_emit_node`, in call order. Shared so a test
    /// can read it after the mock moves into the host.
    emitted: Rc<RefCell<Vec<rustos_abi::HwNode>>>,
    /// The raw signed result `hw_emit_node` returns (`0` published by
    /// default, or `-errno` to model a kernel refusal).
    emit_result: Cell<i64>,
}

impl MockSyscalls {
    fn new() -> Self {
        Self {
            backings: RefCell::new(Vec::new()),
            mmio_calls: Rc::new(Cell::new(0)),
            last_mmio: Rc::new(Cell::new((0, 0))),
            dma_calls: Rc::new(Cell::new(0)),
            dma_frees: Rc::new(RefCell::new(Vec::new())),
            delivered: RefCell::new(Vec::new()),
            grants_error: Cell::new(None),
            irq_line_bound: Rc::new(Cell::new(0)),
            irq_bind_calls: Rc::new(Cell::new(0)),
            irq_wait_calls: Rc::new(Cell::new(0)),
            irq_bind_result: Cell::new(7),
            ipc_endpoint: Cell::new(0),
            ipc_request: RefCell::new(Vec::new()),
            ipc_reply: RefCell::new(Vec::new()),
            ipc_error: Cell::new(None),
            emitted: Rc::new(RefCell::new(Vec::new())),
            emit_result: Cell::new(0),
        }
    }

    /// Make `hw_emit_node` fail with `-err` instead of publishing.
    fn fail_emit_node(&self, err: Errno) {
        self.emit_result.set(-i64::from(err.as_i32()));
    }

    /// A shared handle to the recorded `hw_emit_node` nodes, read after the
    /// mock moves into the host.
    fn emitted(&self) -> Rc<RefCell<Vec<rustos_abi::HwNode>>> {
        Rc::clone(&self.emitted)
    }

    /// Program the bytes `ipc_call` copies back as the reply.
    fn set_ipc_reply(&self, reply: &[u8]) {
        *self.ipc_reply.borrow_mut() = reply.to_vec();
    }

    /// Make `ipc_call` fail with `-err` instead of replying.
    fn fail_ipc_call(&self, err: Errno) {
        self.ipc_error.set(Some(err));
    }

    /// Make `irq_bind` fail with `-err` instead of returning a handle.
    fn fail_irq_bind(&self, err: Errno) {
        self.irq_bind_result.set(-i64::from(err.as_i32()));
    }

    /// Shared observers a test reads after the mock moves into the host: the
    /// last bound line, the `irq_bind` call count, and the `irq_wait` count.
    fn irq_observers(&self) -> IrqObservers {
        (
            Rc::clone(&self.irq_line_bound),
            Rc::clone(&self.irq_bind_calls),
            Rc::clone(&self.irq_wait_calls),
        )
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

    /// A shared handle to the CPU bases passed to `dma_free`, read after the
    /// mock moves into the host to assert each slab freed itself on drop.
    fn dma_frees(&self) -> Rc<RefCell<Vec<u64>>> {
        Rc::clone(&self.dma_frees)
    }

    /// A shared handle to the most recent `mmio_map` `(offset, len)`, read
    /// after the mock moves into the host.
    fn last_mmio(&self) -> Rc<Cell<(u64, usize)>> {
        Rc::clone(&self.last_mmio)
    }
}

impl GrantSyscalls for MockSyscalls {
    fn mmio_map(&self, handle: u64, offset: u64, len: usize) -> i64 {
        self.mmio_calls.set(self.mmio_calls.get() + 1);
        self.last_mmio.set((offset, len));
        let backings = self.backings.borrow();
        match backings.iter().find(|b| b.handle == handle) {
            // Mirror the kernel: the `[offset, offset + len)` sub-region must
            // lie wholly inside the granted backing, and the returned VA is
            // the sub-region's base (`backing_base + offset`), never the whole
            // window's base.
            Some(b) => {
                let Some(end) = offset.checked_add(len as u64) else {
                    return -i64::from(Errno::LengthOutOfRange.as_i32());
                };
                if len == 0 || end > b.buffer.len() as u64 {
                    return -i64::from(Errno::OutOfRange.as_i32());
                }
                b.buffer.as_ptr() as usize as i64 + offset as i64
            }
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

    fn dma_free(&self, handle: u64, cpu_va: u64) -> i64 {
        // Mirror the kernel: the carve must lie inside a backing the grant
        // names, else fail closed. Record the freed base so a test can assert
        // each slab released itself on drop.
        let backings = self.backings.borrow();
        match backings.iter().find(|b| b.handle == handle) {
            Some(b) => {
                let base = b.buffer.as_ptr() as usize as u64;
                let end = base + b.buffer.len() as u64;
                if cpu_va < base || cpu_va >= end {
                    return -i64::from(Errno::OutOfRange.as_i32());
                }
                self.dma_frees.borrow_mut().push(cpu_va);
                0
            }
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
        // handler does.
        if total > buf.len() {
            return -i64::from(Errno::BufferTooSmall.as_i32());
        }
        for (i, grant) in delivered.iter().enumerate() {
            let off = i * GrantedResource::WIRE_LEN;
            buf[off..off + GrantedResource::WIRE_LEN].copy_from_slice(&grant.to_le_bytes());
        }
        total as i64
    }

    fn irq_bind(&self, line: u32) -> i64 {
        self.irq_line_bound.set(line);
        self.irq_bind_calls.set(self.irq_bind_calls.get() + 1);
        self.irq_bind_result.get()
    }

    fn irq_wait(&self, _handle: u64, _timeout_ns: u64) -> i64 {
        self.irq_wait_calls.set(self.irq_wait_calls.get() + 1);
        0
    }

    fn ipc_call(&self, endpoint: u64, request: &[u8], reply: &mut [u8]) -> i64 {
        self.ipc_endpoint.set(endpoint);
        *self.ipc_request.borrow_mut() = request.to_vec();
        if let Some(err) = self.ipc_error.get() {
            return -i64::from(err.as_i32());
        }
        let src = self.ipc_reply.borrow();
        let n = src.len().min(reply.len());
        reply[..n].copy_from_slice(&src[..n]);
        n as i64
    }

    fn shm_map(&self, handle: u64, len_out: &mut u64) -> i64 {
        // Map the whole granted shared region: return its backing buffer's
        // base VA and report its length, mirroring the kernel mapping the
        // region's frames into the caller and writing the registry's own
        // record of the size. An unknown handle fails closed `NotFound`
        // with `len_out` untouched.
        let backings = self.backings.borrow();
        match backings.iter().find(|b| b.handle == handle) {
            Some(b) => {
                *len_out = b.buffer.len() as u64;
                b.buffer.as_ptr() as usize as i64
            }
            None => -i64::from(Errno::NotFound.as_i32()),
        }
    }

    fn hw_emit_node(&self, node: &rustos_abi::HwNode) -> i64 {
        self.emitted.borrow_mut().push(*node);
        self.emit_result.get()
    }

    fn msi_alloc(&self) -> Result<rustos_abi::MsiAllocation, i64> {
        // A canned allocation: a doorbell pair and a virtual line, so a test
        // exercising the MSI-routing path sees a stable, non-failing result.
        Ok(rustos_abi::MsiAllocation::new(0xFFFF_FFFC, 0x6540, 1024))
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
fn maps_a_framebuffer_scanout_window() {
    // Regression: a boot display node grants its scan-out surface as a
    // `Framebuffer` resource (geometry-carrying), and the host must treat
    // it as a mappable CPU-addressed window exactly like a plain register
    // block — the display service's bring-up failed closed when it did not.
    const FB_HANDLE: u64 = 7;
    const FB_BASE: u64 = 0x4120_0000;
    let mode = rustos_abi::driver::display::DisplayMode {
        width_px: 8,
        height_px: 4,
        stride_bytes: 32,
        format: rustos_abi::driver::display::DisplayFormat::Bgra8888,
    };
    let fb = HwResource::framebuffer(FB_BASE, &mode).expect("valid mode");
    let len = usize::try_from(fb.length()).expect("small surface");

    let mock = MockSyscalls::new();
    let base = mock.back(FB_HANDLE, len, 0);
    let host = RtDriverHost::new(
        caps(&[CapabilityId::MMIO_MAP]),
        mock,
        &[GrantedResource::new(FB_HANDLE, fb)],
        None,
    )
    .unwrap();

    let window = host.map_window(FB_BASE, len).expect("scan-out map");
    assert_eq!(window.phys_base(), FB_BASE);
    assert_eq!(window.len(), len);
    window.write_u32(0, 0x00FF_00FF).expect("in-bounds write");
    let readback = unsafe { (base as *const u32).read() };
    assert_eq!(readback, 0x00FF_00FF);
}

#[test]
fn translates_a_bar_inside_the_outbound_bus_window() {
    let mock = MockSyscalls::new();
    let base = mock.back(BUSWIN_HANDLE, OUTBOUND_SIZE as usize, 0);
    let last_mmio = mock.last_mmio();
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
    // The host asked the kernel to map ONLY the BAR sub-region — its offset
    // into the outbound window and the BAR length — never the whole (here
    // 64 MiB, on metal 1 GiB) bus aperture. Mapping the whole grant is the
    // defect that exhausted the per-task MMIO window and failed closed with
    // `OutOfMemory`.
    assert_eq!(last_mmio.get(), (0x1_0000, 0x1000));
}

#[test]
fn maps_each_sub_region_offset_only_once() {
    let mock = MockSyscalls::new();
    mock.back(REGS_HANDLE, REGS_LEN as usize, 0);
    let calls = mock.mmio_counter();
    let host =
        RtDriverHost::new(caps(&[CapabilityId::MMIO_MAP]), mock, &[regs_grant()], None).unwrap();

    // Repeated requests for the *same* sub-region offset reuse the cached VA
    // — exactly one syscall. A different length at the
    // same offset still hits the cache (the offset keys it).
    let _ = host.map_window(REGS_BASE, 0x10).expect("first");
    let _ = host.map_window(REGS_BASE, 0x20).expect("same offset again");
    assert_eq!(calls.get(), 1, "the same sub-region offset is mapped once");

    // A request at a *different* offset is a distinct sub-region (e.g. a
    // second BAR), so it maps afresh rather than aliasing the first
    // (sub-region mapping, not whole-window).
    let _ = host
        .map_window(REGS_BASE + 0x40, 0x10)
        .expect("second offset");
    assert_eq!(
        calls.get(),
        2,
        "a distinct sub-region offset maps separately"
    );
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

#[test]
fn dma_slab_frees_itself_on_drop_and_repeated_cycles_do_not_leak() {
    // The leak fix: a carve's slab releases its buffer through `dma_free` when
    // it drops, instead of leaking until process exit. A long-running driver
    // issuing many transfers therefore frees one buffer per transfer — the
    // `dma_free` syscall count matches the carve count, with nothing live.
    let mock = MockSyscalls::new();
    let base = mock.back(DMA_HANDLE, 0x4000, DMA_DEVICE_BASE);
    let frees = mock.dma_frees();
    let host =
        RtDriverHost::new(caps(&[CapabilityId::MEM_DMA]), mock, &[dma_grant()], None).unwrap();

    {
        let _slab = host.alloc_dma_zeroed(0x1000).expect("carve");
        assert!(
            frees.borrow().is_empty(),
            "a live slab must not have freed its carve yet"
        );
    }
    // Dropping the slab released exactly its CPU base through `dma_free`.
    assert_eq!(
        &*frees.borrow(),
        &[base],
        "the slab freed its carve on drop"
    );

    // Many alloc/free cycles: each dropped slab frees exactly once, so the
    // free count tracks the carve count — no buffer leaks across cycles.
    for _ in 0..16 {
        let _ = host.alloc_dma_zeroed(0x1000).expect("carve");
    }
    assert_eq!(
        frees.borrow().len(),
        17,
        "every carve's slab freed itself exactly once across cycles"
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
    // result: the host builds, but any map is refused.
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
    // fails closed as a packaging defect.
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
    // host builds no grant table rather than guessing.
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
    // second `resource_grants` syscall.
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

// --- `notify_wait`: interrupt-driven park on a granted IRQ line ----------

/// An interrupt-line grant: handle `4`, GICv2 SPI line `34`.
const IRQ_HANDLE: u64 = 4;
const IRQ_LINE: u32 = 34;

fn irq_grant() -> GrantedResource {
    GrantedResource::new(IRQ_HANDLE, HwResource::irq(u64::from(IRQ_LINE), 1))
}

#[test]
fn notify_wait_binds_the_granted_line_once_then_parks_each_call() {
    // An interrupt-driven driver (e.g. the user-space virtio-input keyboard)
    // parks on its granted device interrupt rather than busy-polling. The first `notify_wait` binds the line the
    // node granted; every call (the first included) parks on `irq_wait`. The
    // bind is cached, so a second call binds no second time.
    let mock = MockSyscalls::new();
    let (line, binds, waits) = mock.irq_observers();
    let host =
        RtDriverHost::new(caps(&[CapabilityId::IRQ_BIND]), mock, &[irq_grant()], None).unwrap();

    rustos_abi::driver::virtio::VirtioHost::notify_wait(&host, 0);
    rustos_abi::driver::virtio::VirtioHost::notify_wait(&host, 0);

    assert_eq!(line.get(), IRQ_LINE, "binds the node's granted line");
    assert_eq!(binds.get(), 1, "the line is bound exactly once (cached)");
    assert_eq!(waits.get(), 2, "every call parks on irq_wait");
}

#[test]
fn notify_wait_without_an_irq_grant_is_a_noop() {
    // A driver granted no IRQ line cannot park on one; `notify_wait` returns
    // without binding or waiting, and its caller falls back to a polling
    // re-scan + yield (fail safe, never a wedged wait).
    let mock = MockSyscalls::new();
    let (_, binds, waits) = mock.irq_observers();
    let host = RtDriverHost::new(
        caps(&[CapabilityId::IRQ_BIND, CapabilityId::MEM_DMA]),
        mock,
        &[dma_grant()],
        None,
    )
    .unwrap();

    rustos_abi::driver::virtio::VirtioHost::notify_wait(&host, 0);
    assert_eq!(binds.get(), 0);
    assert_eq!(waits.get(), 0);
}

#[test]
fn notify_wait_without_the_bind_capability_is_a_noop() {
    // Capability before the trap: a driver lacking
    // `CAP_IRQ_BIND` never issues the bind, even with an IRQ grant present.
    let mock = MockSyscalls::new();
    let (_, binds, waits) = mock.irq_observers();
    let host = RtDriverHost::new(caps(&[]), mock, &[irq_grant()], None).unwrap();

    rustos_abi::driver::virtio::VirtioHost::notify_wait(&host, 0);
    assert_eq!(binds.get(), 0);
    assert_eq!(waits.get(), 0);
}

#[test]
fn notify_wait_does_not_park_when_the_bind_is_refused() {
    // A refused bind (the kernel rejects the line) must not be papered over
    // with a wait on an unbound handle: `notify_wait` returns and the bind is
    // retried on the next call (fail closed, no cached
    // bogus handle).
    let mock = MockSyscalls::new();
    mock.fail_irq_bind(Errno::PermissionDenied);
    let (_, binds, waits) = mock.irq_observers();
    let host =
        RtDriverHost::new(caps(&[CapabilityId::IRQ_BIND]), mock, &[irq_grant()], None).unwrap();

    rustos_abi::driver::virtio::VirtioHost::notify_wait(&host, 0);
    rustos_abi::driver::virtio::VirtioHost::notify_wait(&host, 0);
    assert_eq!(binds.get(), 2, "a refused bind is retried, never cached");
    assert_eq!(waits.get(), 0, "never parks on an unbound handle");
}

#[test]
fn bind_irq_binds_once_and_caches_the_handle() {
    // The explicit preflight a park-dependent event loop runs: the first
    // call binds the granted line, the second is answered from the cache.
    let mock = MockSyscalls::new();
    let (line, binds, _) = mock.irq_observers();
    let host =
        RtDriverHost::new(caps(&[CapabilityId::IRQ_BIND]), mock, &[irq_grant()], None).unwrap();

    assert_eq!(host.bind_irq(), Ok(()));
    assert_eq!(host.bind_irq(), Ok(()));
    assert_eq!(line.get(), IRQ_LINE, "binds the node's granted line");
    assert_eq!(binds.get(), 1, "the line is bound exactly once (cached)");
}

#[test]
fn bind_irq_without_the_bind_capability_is_permission_denied() {
    // Capability before the trap: no `CAP_IRQ_BIND`, no bind syscall, and
    // the caller learns its interrupt park cannot work (fail loud).
    let mock = MockSyscalls::new();
    let (_, binds, _) = mock.irq_observers();
    let host = RtDriverHost::new(caps(&[]), mock, &[irq_grant()], None).unwrap();

    assert_eq!(host.bind_irq(), Err(DriverError::PermissionDenied));
    assert_eq!(binds.get(), 0);
}

#[test]
fn bind_irq_without_an_irq_grant_is_not_found() {
    // A mis-provisioned node granted no IRQ line: the preflight reports it
    // rather than letting the event loop degrade into a busy re-poll.
    let mock = MockSyscalls::new();
    let (_, binds, _) = mock.irq_observers();
    let host = RtDriverHost::new(
        caps(&[CapabilityId::IRQ_BIND, CapabilityId::MEM_DMA]),
        mock,
        &[dma_grant()],
        None,
    )
    .unwrap();

    assert_eq!(host.bind_irq(), Err(DriverError::NotFound));
    assert_eq!(binds.get(), 0);
}

#[test]
fn bind_irq_surfaces_a_refused_bind_and_caches_nothing() {
    // A kernel-refused bind is surfaced and retried on the next call —
    // never a cached bogus handle.
    let mock = MockSyscalls::new();
    mock.fail_irq_bind(Errno::PermissionDenied);
    let (_, binds, _) = mock.irq_observers();
    let host =
        RtDriverHost::new(caps(&[CapabilityId::IRQ_BIND]), mock, &[irq_grant()], None).unwrap();

    assert_eq!(host.bind_irq(), Err(DriverError::DeviceFault));
    assert_eq!(host.bind_irq(), Err(DriverError::DeviceFault));
    assert_eq!(binds.get(), 2, "a refused bind is retried, never cached");
}

// --- `mailbox`: client-side firmware property exchange over `ipc_call` ---

#[test]
fn mailbox_exchange_marshals_to_the_endpoint_and_decodes_the_reply() {
    use rustos_abi::driver::mailbox::{MailboxChannel, MAILBOX_PROPERTY_WORDS};
    use rustos_abi::mailbox_ipc;

    // The host's `MailboxChannel` is purely the client side of the IPC: it
    // encodes the request, posts it to the well-known mailbox endpoint, and
    // decodes the service's response back into the caller's buffer in place.
    let mut request = [0u32; MAILBOX_PROPERTY_WORDS];
    for (i, word) in request.iter_mut().enumerate() {
        *word = 0x2000_0000 + u32::try_from(i).expect("index fits u32");
    }
    let mut response = [0u32; MAILBOX_PROPERTY_WORDS];
    for (i, word) in response.iter_mut().enumerate() {
        *word = 0x3000_0000 + u32::try_from(i).expect("index fits u32");
    }
    let mut reply_bytes = [0u8; mailbox_ipc::REPLY_LEN];
    mailbox_ipc::encode_reply(&mut reply_bytes, &response).expect("encodes");

    let mock = MockSyscalls::new();
    mock.set_ipc_reply(&reply_bytes);
    let host = RtDriverHost::new(caps(&[CapabilityId::MAILBOX]), mock, &[], None).unwrap();

    let mut message = request;
    MailboxChannel::exchange(&host, &mut message).expect("exchange succeeds");
    assert_eq!(message, response, "the reply buffer is decoded in place");
}

#[test]
fn mailbox_exchange_fails_closed_on_a_transport_error() {
    use rustos_abi::driver::mailbox::{MailboxChannel, MAILBOX_PROPERTY_WORDS};

    // A missing `CAP_MAILBOX` (or any other kernel refusal of the call) is
    // surfaced as a `DriverError`, never papered over.
    let mock = MockSyscalls::new();
    mock.fail_ipc_call(Errno::PermissionDenied);
    let host = RtDriverHost::new(caps(&[]), mock, &[], None).unwrap();

    let mut message = [0u32; MAILBOX_PROPERTY_WORDS];
    assert_eq!(
        MailboxChannel::exchange(&host, &mut message),
        Err(DriverError::PermissionDenied)
    );
}

#[test]
fn mailbox_exchange_surfaces_a_service_error_reply() {
    use rustos_abi::driver::mailbox::{MailboxChannel, MAILBOX_PROPERTY_WORDS};
    use rustos_abi::mailbox_ipc;

    // A status-framed error reply (the service mapped its `DriverError` to an
    // `Errno`) fails the exchange closed: a firmware fault / timeout
    // (`NotImplemented` image) folds to `DeviceFault`.
    let mut reply_bytes = [0u8; mailbox_ipc::REPLY_LEN];
    let n =
        mailbox_ipc::encode_error_reply(&mut reply_bytes, Errno::NotImplemented).expect("encodes");

    let mock = MockSyscalls::new();
    mock.set_ipc_reply(&reply_bytes[..n]);
    let host = RtDriverHost::new(caps(&[CapabilityId::MAILBOX]), mock, &[], None).unwrap();

    let mut message = [0u32; MAILBOX_PROPERTY_WORDS];
    assert_eq!(
        MailboxChannel::exchange(&host, &mut message),
        Err(DriverError::DeviceFault)
    );
}

#[test]
fn emit_node_publishes_the_child_through_the_syscall() {
    use rustos_abi::hwtree::{HwDeviceClass, HwMatchKey, HwNode};

    // A bus driver publishes an enumerated child; the host forwards the
    // encoded node through `hw_emit_node` and reports success. The kernel — not the host — enforces `CAP_HW_EMIT` and
    // the grant-coverage check, so the host adds no authority of its own.
    let mock = MockSyscalls::new();
    let emitted = mock.emitted();
    let host = RtDriverHost::new(caps(&[CapabilityId::HW_EMIT]), mock, &[], None).unwrap();

    let mut child = HwNode::new(3, 2, HwDeviceClass::Input);
    child
        .push_match_key(HwMatchKey::usb(0x1234, 0x5678, 0x03_01_01))
        .expect("match key fits");
    assert_eq!(DriverHost::emit_node(&host, child), Ok(()));

    // The exact node reached the syscall seam.
    assert_eq!(emitted.borrow().len(), 1);
    assert_eq!(emitted.borrow()[0], child);
}

#[test]
fn emit_node_surfaces_a_kernel_refusal_fail_closed() {
    use rustos_abi::hwtree::{HwDeviceClass, HwNode};

    // A kernel refusal — the driver lacks `CAP_HW_EMIT`, or the node requests
    // a resource outside its grants — is surfaced as `PermissionDenied`,
    // never papered over.
    let mock = MockSyscalls::new();
    mock.fail_emit_node(Errno::PermissionDenied);
    let host = RtDriverHost::new(caps(&[]), mock, &[], None).unwrap();

    let child = HwNode::new(3, 2, HwDeviceClass::Input);
    assert_eq!(
        DriverHost::emit_node(&host, child),
        Err(DriverError::PermissionDenied)
    );
}

// --- URB transport: the class driver's endpoint id + shared buffer ------

/// A grant handle for the forwarded shared URB buffer.
const SHM_HANDLE: u64 = 9;

#[test]
fn endpoint_grant_reads_the_endpoint_grant_base() {
    // A class driver's matched interface node carried a per-endpoint grant;
    // its `base` is the URB transport endpoint id the driver `ipc_call`s.
    let mock = MockSyscalls::new();
    let host = RtDriverHost::new(
        caps(&[]),
        mock,
        &[GrantedResource::new(7, HwResource::endpoint(0xD012_5701))],
        None,
    )
    .unwrap();
    assert_eq!(host.endpoint_grant(), Some(0xD012_5701));
}

#[test]
fn endpoint_grant_is_none_without_an_endpoint_grant() {
    let mock = MockSyscalls::new();
    let host =
        RtDriverHost::new(caps(&[CapabilityId::MMIO_MAP]), mock, &[regs_grant()], None).unwrap();
    assert_eq!(host.endpoint_grant(), None);
}

#[test]
fn map_shared_maps_the_granted_region() {
    // The HCD created the region and forwarded it as a `Shared` grant; the
    // class driver maps the same frames through `shm_map`.
    let mock = MockSyscalls::new();
    let base = mock.back(SHM_HANDLE, 64, 0);
    let host = RtDriverHost::new(
        caps(&[CapabilityId::SHM]),
        mock,
        &[GrantedResource::new(SHM_HANDLE, HwResource::shared(0x5147))],
        None,
    )
    .unwrap();
    // Both the base and the kernel-reported region length come back; the
    // driver never sizes the shared bytes from the granting task's claim.
    assert_eq!(host.map_shared(), Ok((base, 64)));
}

#[test]
fn map_shared_without_a_grant_is_not_found() {
    let mock = MockSyscalls::new();
    let host = RtDriverHost::new(caps(&[CapabilityId::SHM]), mock, &[regs_grant()], None).unwrap();
    assert_eq!(host.map_shared(), Err(DriverError::NotFound));
}

#[test]
fn map_shared_surfaces_a_kernel_refusal_fail_closed() {
    // An unknown/forged grant handle fails the `shm_map` closed; the host
    // never fabricates a mapping.
    let mock = MockSyscalls::new();
    // No backing registered for `SHM_HANDLE`, so the mock's `shm_map` returns
    // `NotFound`, which the host folds to `Unsupported` (a non-permission
    // kernel refusal).
    let host = RtDriverHost::new(
        caps(&[CapabilityId::SHM]),
        mock,
        &[GrantedResource::new(SHM_HANDLE, HwResource::shared(0x5147))],
        None,
    )
    .unwrap();
    assert_eq!(host.map_shared(), Err(DriverError::Unsupported));
}
