//! Unit tests for the xHCI protocol layers against a register-level
//! mock controller plus an in-memory ring/DMA model (mirrors the
//! `emmc2` `MockSdhci` seam).

extern crate alloc;

use alloc::collections::VecDeque;
use alloc::rc::Rc;
use alloc::vec::Vec;
use core::cell::RefCell;

use super::device::{
    hub_port_connected, hub_port_enabled, hub_port_speed, route_for_child, AttachOutcome,
    BulkDirection, BulkPipe, DeviceDescriptor, DmaBank, EnumStage, EventWait, HubEvent,
    InterfaceInfo, UsbDevice, BULK_BUF_LEN, BULK_SLOTS, EVENT_RING_SEGMENT_MIN_TRBS, MAX_HUB_DEPTH,
    REPORT_LEN, RING_TRBS,
};
use super::ring::{EventRingCursor, ProducerRing};
use super::trb::{CompletionCode, Trb, TrbType, CONTROL_CYCLE, TRB_LEN};
use super::*;
use rustos_abi::driver::input::Input;
use rustos_abi::Delay;
use rustos_hid::BootKeyboard;

/// The primary bulk pipes most fixtures exercise (BOT-shaped devices).
const IN_PIPE: BulkPipe = BulkPipe::primary(BulkDirection::In);
const OUT_PIPE: BulkPipe = BulkPipe::primary(BulkDirection::Out);

/// The mock's `CAPLENGTH` (so its operational base).
const MOCK_CAPLENGTH: u32 = 0x20;
/// The mock's doorbell-array offset.
const MOCK_DBOFF: u32 = 0x1000;
/// The mock's runtime-block offset.
const MOCK_RTSOFF: u32 = 0x2000;
/// The mock's register-window byte length.
const MOCK_WINDOW_LEN: usize = 0x3000;
/// Device-visible base address of the shared DMA buffer.
const MOCK_DMA_BASE: u64 = 0x0010_0000;
/// Byte length of the shared DMA buffer backing the mock bank. Chunks are
/// carved monotonically and released space is never reused (mirroring the
/// production bank's stale-offset fail-closed property), so the buffer is
/// sized generously: each served device's region chunk is ~67 KiB, a hub
/// watch chunk ~1 KiB, the shared structures ~2 KiB, and the scratchpad
/// test reserves 31 more pages — 4 MiB absorbs the deepest fan-out and the
/// re-attach cycles the tests run.
const MOCK_DMA_LEN: usize = 0x40_0000;
/// The mock's 64-byte contexts (its `HCCPARAMS1` sets CSZ).
const MOCK_CTX_SIZE: usize = 64;

#[test]
fn event_ring_segment_meets_xhci_minimum() {
    let ring_trbs = core::hint::black_box(RING_TRBS);
    let event_min = core::hint::black_box(EVENT_RING_SEGMENT_MIN_TRBS);
    assert!(ring_trbs >= event_min);
    assert_eq!(ring_trbs, 16);
}

/// Memory shared between the engine's [`DmaBank`] and the mock
/// controller's device model — the in-memory stand-in for DMA.
type SharedMem = Rc<RefCell<Vec<u8>>>;

fn shared_mem() -> SharedMem {
    Rc::new(RefCell::new(alloc::vec![0u8; MOCK_DMA_LEN]))
}

/// The engine-side [`DmaBank`] view of the shared buffer: chunks are
/// carved monotonically from the one `Vec`, with each virtual offset equal
/// to its buffer offset (so the register-level device model reads the same
/// bytes at `MOCK_DMA_BASE + offset` exactly as before). Released chunk
/// space is never reused — the production bank's monotonic-base property —
/// so a stale offset fails closed here too, and exhausting the buffer is
/// the mock's deterministic-OOM stand-in.
struct MockDma {
    mem: SharedMem,
    phys: u64,
    /// Live chunks as `(base, len)`, ascending by base.
    chunks: Vec<(usize, usize)>,
    /// The next chunk's base offset; monotonic, 4096-aligned.
    next_base: usize,
}

impl MockDma {
    fn new(mem: SharedMem, phys: u64) -> Self {
        Self {
            mem,
            phys,
            chunks: Vec::new(),
            next_base: 0,
        }
    }

    /// Number of live (unreleased) chunks — the observable the
    /// release-on-detach tests assert on.
    fn live_chunks(&self) -> usize {
        self.chunks.len()
    }

    /// The live chunk containing `[offset, offset + len)` wholly (and
    /// `offset` itself strictly inside the chunk).
    fn chunk_covering(&self, offset: usize, len: usize) -> Result<(), DriverError> {
        let end = offset.checked_add(len).ok_or(DriverError::OutOfRange)?;
        self.chunks
            .iter()
            .any(|&(base, chunk_len)| {
                offset >= base && offset < base + chunk_len && end <= base + chunk_len
            })
            .then_some(())
            .ok_or(DriverError::OutOfRange)
    }
}

impl DmaBank for MockDma {
    fn grow(&mut self, len: usize) -> Result<usize, DriverError> {
        if len == 0 {
            return Err(DriverError::OutOfRange);
        }
        let base = self.next_base;
        let end = base.checked_add(len).ok_or(DriverError::LengthOutOfRange)?;
        if end > self.mem.borrow().len() {
            return Err(DriverError::LengthOutOfRange);
        }
        self.next_base = end.next_multiple_of(4096);
        self.chunks.push((base, len));
        Ok(base)
    }

    fn release(&mut self, base: usize) -> Result<(), DriverError> {
        let index = self
            .chunks
            .iter()
            .position(|&(chunk_base, _)| chunk_base == base)
            .ok_or(DriverError::NotFound)?;
        self.chunks.remove(index);
        Ok(())
    }

    fn phys_of(&self, offset: usize) -> Result<u64, DriverError> {
        self.chunk_covering(offset, 0)?;
        Ok(self.phys + offset as u64)
    }

    fn read(&mut self, offset: usize, buf: &mut [u8]) -> Result<(), DriverError> {
        self.chunk_covering(offset, buf.len())?;
        let mem = self.mem.borrow();
        buf.copy_from_slice(&mem[offset..offset + buf.len()]);
        Ok(())
    }

    fn write(&mut self, offset: usize, bytes: &[u8]) -> Result<(), DriverError> {
        self.chunk_covering(offset, bytes.len())?;
        let mut mem = self.mem.borrow_mut();
        mem[offset..offset + bytes.len()].copy_from_slice(bytes);
        Ok(())
    }
}

/// File-scope recorder for the [`DmaSlab`] coherency hook (a bare `fn`
/// pointer, so the observed call count and length are published through
/// atomics). Used by a single test so no cross-test race is possible
/// (no flaky tests).
mod slab_coherency_test_state {
    use core::sync::atomic::{AtomicUsize, Ordering};
    pub(super) static CALLS: AtomicUsize = AtomicUsize::new(0);
    pub(super) static LAST_LEN: AtomicUsize = AtomicUsize::new(0);

    /// A `rustos_abi::driver::dma::SlabCoherencyFn`.
    pub(super) fn record(_base: *const u8, len: usize) {
        CALLS.fetch_add(1, Ordering::SeqCst);
        LAST_LEN.store(len, Ordering::SeqCst);
    }
}

/// Test support for the production [`SlabBank`]: a [`DmaHost`] minting
/// leaked slabs at ascending device-visible bases, with an observable free
/// count, an injectable allocation failure, and an optional coherency hook
/// stamped onto every minted slab.
///
/// [`DmaHost`]: rustos_abi::driver::dma::DmaHost
mod bank_test {
    use core::cell::Cell;
    use core::ptr::NonNull;
    use core::sync::atomic::{AtomicUsize, Ordering};

    use rustos_abi::driver::dma::{DmaHost, DmaSlab, PoolId, SlabCoherencyFn};
    use rustos_abi::DriverError;

    /// Free shim recording each dropped slab on the host's own counter.
    ///
    /// # Safety
    ///
    /// `pool` is the address of the minting [`MockSlabHost`]'s `frees`
    /// counter; the host is borrowed by the bank for the bank's whole
    /// lifetime, so it outlives every slab it minted.
    unsafe fn count_free(pool: *const (), _cpu: NonNull<u8>, _slot: usize, _len: usize) {
        // SAFETY: per the function contract, `pool` points at the live
        // host's `frees` counter.
        let frees = unsafe { &*(pool.cast::<AtomicUsize>()) };
        frees.fetch_add(1, Ordering::SeqCst);
    }

    /// The mock slab-minting host.
    pub(super) struct MockSlabHost {
        /// Device-visible base of the next minted slab; each allocation
        /// advances it by 64 KiB, so chunk bases are distinct, 64-aligned,
        /// and ascending.
        next_phys: Cell<u64>,
        /// When set, the next allocation fails (the pool is exhausted).
        pub(super) fail: Cell<bool>,
        /// Coherency hook stamped onto every minted slab.
        pub(super) coherency: Cell<Option<SlabCoherencyFn>>,
        /// Dropped-slab count, incremented by the free shim.
        pub(super) frees: AtomicUsize,
    }

    impl MockSlabHost {
        pub(super) fn new(phys_base: u64) -> Self {
            Self {
                next_phys: Cell::new(phys_base),
                fail: Cell::new(false),
                coherency: Cell::new(None),
                frees: AtomicUsize::new(0),
            }
        }

        pub(super) fn free_count(&self) -> usize {
            self.frees.load(Ordering::SeqCst)
        }
    }

    impl DmaHost for MockSlabHost {
        fn alloc_dma_zeroed(&self, size: usize) -> Result<DmaSlab, DriverError> {
            if self.fail.get() {
                return Err(DriverError::LengthOutOfRange);
            }
            let phys = self.next_phys.get();
            self.next_phys.set(phys + 0x1_0000);
            let storage = alloc::vec![0u8; size].into_boxed_slice();
            let leaked: &'static mut [u8] = alloc::boxed::Box::leak(storage);
            let ptr = NonNull::new(leaked.as_mut_ptr()).expect("box leak is non-null");
            let pool_ptr: *const () = (&raw const self.frees).cast();
            // SAFETY: `ptr` covers `size` leaked zeroed bytes nothing else
            // references; `phys` is the test's device-visible base for
            // `ptr[0]`; `pool_ptr` is this host's free counter, which
            // outlives every slab it mints (the host outlives the bank in
            // every test).
            let slab = unsafe {
                DmaSlab::from_pool(phys, ptr, size, PoolId::MOCK, 0, pool_ptr, count_free)
            };
            Ok(match self.coherency.get() {
                Some(hook) => slab.with_coherency(hook),
                None => slab,
            })
        }
    }
}

#[test]
fn slab_bank_grows_reads_writes_and_maps_phys_per_chunk() {
    let host = bank_test::MockSlabHost::new(0x1000);
    let mut bank = SlabBank::new(&host);
    let first = bank.grow(128).expect("first chunk");
    let second = bank.grow(64).expect("second chunk");
    assert_ne!(first, second, "chunks own distinct base offsets");

    // A write is read back from the same chunk, and each chunk's phys
    // derives from its own slab, not a shared base.
    bank.write(first + 32, &[0xAA; 8]).expect("write");
    let mut buf = [0u8; 8];
    bank.read(first + 32, &mut buf).expect("read");
    assert_eq!(buf, [0xAA; 8]);
    assert_eq!(bank.phys_of(first).expect("phys"), 0x1000);
    assert_eq!(bank.phys_of(second).expect("phys"), 0x1_1000);

    // An access crossing a chunk's end fails closed rather than spilling
    // into whatever chunk follows in the virtual offset space.
    assert_eq!(
        bank.read(first + 120, &mut [0u8; 16]).err(),
        Some(DriverError::OutOfRange)
    );
}

#[test]
fn slab_bank_refuses_a_chunk_beyond_the_aperture() {
    // The minted slab ends past the controller's inbound-DMA aperture: the
    // grow is refused fail-closed and the unreachable slab is returned to
    // the host rather than leaked.
    let host = bank_test::MockSlabHost::new(0x1000);
    let mut bank = SlabBank::with_aperture(&host, 0x1040);
    assert_eq!(bank.grow(0x100).err(), Some(DriverError::OutOfRange));
    assert_eq!(host.free_count(), 1, "the refused slab was freed");

    // A slab wholly below the aperture is granted.
    let host = bank_test::MockSlabHost::new(0x1000);
    let mut bank = SlabBank::with_aperture(&host, 0x2000);
    assert!(bank.grow(0x100).is_ok());
}

#[test]
fn slab_bank_propagates_allocator_exhaustion() {
    let host = bank_test::MockSlabHost::new(0x1000);
    let mut bank = SlabBank::new(&host);
    host.fail.set(true);
    assert_eq!(bank.grow(64).err(), Some(DriverError::LengthOutOfRange));
}

#[test]
fn slab_bank_release_frees_the_chunk_and_stale_offsets_fail_closed() {
    let host = bank_test::MockSlabHost::new(0x1000);
    let mut bank = SlabBank::new(&host);
    let base = bank.grow(64).expect("chunk");
    bank.write(base, &[1u8; 4]).expect("write");

    bank.release(base).expect("release");
    assert_eq!(host.free_count(), 1, "the released chunk's slab was freed");

    // The released chunk's offsets map to nothing: every access through a
    // stale offset fails closed, and base offsets are never reused so a
    // later grow cannot alias it.
    assert_eq!(bank.phys_of(base).err(), Some(DriverError::OutOfRange));
    assert_eq!(
        bank.read(base, &mut [0u8; 4]).err(),
        Some(DriverError::OutOfRange)
    );
    assert_eq!(bank.release(base).err(), Some(DriverError::NotFound));
    let fresh = bank.grow(64).expect("fresh chunk");
    assert_ne!(fresh, base, "released bases are never reused");
}

#[test]
fn slab_bank_brackets_writes_and_reads_with_cache_maintenance() {
    use core::sync::atomic::Ordering;
    use slab_coherency_test_state as rec;

    // A bank whose host mints slabs carrying the recording coherency hook —
    // the metal shape where the BCM2711 PCIe master does not snoop the CPU
    // caches, so the bank must bracket every ring publish / event consume
    // with cache maintenance.
    let host = bank_test::MockSlabHost::new(0x1000);
    let mut bank = SlabBank::new(&host);
    host.coherency.set(Some(rec::record));
    let base = bank.grow(64).expect("chunk");

    // A write cleans the published range to memory *after* the CPU copy,
    // so a non-coherent master reads fresh bytes once the doorbell rings.
    bank.write(base + 8, &[0xAB; 4]).expect("write");
    assert_eq!(rec::CALLS.load(Ordering::SeqCst), 1);
    assert_eq!(rec::LAST_LEN.load(Ordering::SeqCst), 4);

    // A read invalidates the CPU's view of the range *before* the copy,
    // so a master's freshly written bytes are read from memory.
    let mut buf = [0u8; 2];
    bank.read(base + 16, &mut buf).expect("read");
    assert_eq!(rec::CALLS.load(Ordering::SeqCst), 2);
    assert_eq!(rec::LAST_LEN.load(Ordering::SeqCst), 2);
}

/// The 18-byte device descriptor fixture the model answers
/// `GET_DESCRIPTOR(device)` with (a generic boot keyboard).
const MOCK_DESCRIPTOR: [u8; 18] = [
    18, 0x01, 0x00, 0x02, 0x00, 0x00, 0x00, 0x40, 0x6D, 0x04, 0x77, 0xC0, 0x01, 0x00, 0x00, 0x00,
    0x00, 0x01,
];

/// The configuration descriptor fixture the model answers
/// `GET_DESCRIPTOR(configuration)` with: a 9-byte configuration header
/// (`bConfigurationValue` = 1) followed by one 9-byte interface
/// descriptor of the HID boot-keyboard class (`0x03_01_01`,
/// `bInterfaceNumber` = 0).
const MOCK_CONFIG_DESCRIPTOR: [u8; 25] = [
    // Configuration: bLength=9, type=2, wTotalLength=25, 1 interface,
    // bConfigurationValue=1, iConfiguration=0, bmAttributes=0xA0,
    // bMaxPower=50.
    0x09, 0x02, 0x19, 0x00, 0x01, 0x01, 0x00, 0xA0, 0x32, //
    // Interface: bLength=9, type=4, bInterfaceNumber=0, alt=0,
    // 1 endpoint, class=0x03 (HID), sub=0x01 (boot), protocol=0x01
    // (keyboard), iInterface=0.
    0x09, 0x04, 0x00, 0x00, 0x01, 0x03, 0x01, 0x01, 0x00, //
    // Endpoint: bLength=7, type=5, bEndpointAddress=0x81 (EP1 IN ->
    // DCI 3), bmAttributes=0x03 (interrupt), wMaxPacketSize=8,
    // bInterval=10 (frames, full-speed boot keyboard).
    0x07, 0x05, 0x81, 0x03, 0x08, 0x00, 0x0A,
];

/// As [`MOCK_CONFIG_DESCRIPTOR`], but the boot keyboard's interrupt-IN
/// endpoint is **endpoint 2** (`bEndpointAddress = 0x82` -> DCI 5), not
/// endpoint 1. The driver must read the endpoint descriptor and
/// configure / doorbell / drain DCI 5; the metal no-report bug was
/// hard-coding DCI 3.
const MOCK_CONFIG_DESCRIPTOR_EP2: [u8; 25] = [
    0x09, 0x02, 0x19, 0x00, 0x01, 0x01, 0x00, 0xA0, 0x32, //
    0x09, 0x04, 0x00, 0x00, 0x01, 0x03, 0x01, 0x01, 0x00, //
    // Endpoint: bEndpointAddress=0x82 (EP2 IN -> DCI 5).
    0x07, 0x05, 0x82, 0x03, 0x08, 0x00, 0x0A,
];

/// Device descriptor fixture for a USB **hub** (`bDeviceClass = 0x09`),
/// `idVendor:idProduct = 2109:3431` — the Pi 4B's onboard VIA Labs hub.
const MOCK_HUB_DESCRIPTOR: [u8; 18] = [
    18, 0x01, 0x00, 0x02, 0x09, 0x00, 0x00, 0x40, 0x09, 0x21, 0x31, 0x34, 0x01, 0x00, 0x00, 0x00,
    0x00, 0x01,
];

/// Device descriptor fixture for a **`SuperSpeed`** USB hub: bcdUSB 3.00,
/// `bMaxPacketSize0 = 9` (the exponent encoding of EP0's fixed 512, USB
/// 3.2 §9.6.1), `idVendor:idProduct = 0bda:5411` — the Realtek RTS5411
/// a Pi 4 multi-drive enclosure presents on the `SuperSpeed` root port.
const MOCK_SS_HUB_DESCRIPTOR: [u8; 18] = [
    18, 0x01, 0x00, 0x03, 0x09, 0x00, 0x00, 9, 0xDA, 0x0B, 0x11, 0x54, 0x01, 0x00, 0x00, 0x00,
    0x00, 0x01,
];

/// Device descriptor fixture for a **`SuperSpeed`** leaf device behind the
/// SS hub: bcdUSB 3.00 and `bMaxPacketSize0 = 9`. Addressing it at any
/// speed but `SuperSpeed` makes the engine's descriptor validation refuse
/// the exponent-encoded value, so a downstream-speed misdecode cannot
/// pass this fixture.
const MOCK_SS_DESCRIPTOR: [u8; 18] = [
    18, 0x01, 0x00, 0x03, 0x00, 0x00, 0x00, 9, 0x6D, 0x04, 0x77, 0xC0, 0x01, 0x00, 0x00, 0x00,
    0x00, 0x01,
];

/// Configuration descriptor fixture for the hub: one interface of the
/// hub class (`0x09_00_00`) with one interrupt-IN status-change endpoint
/// (USB 2.0 §11.12.3), so the engine arms the hub-hotplug watch.
const MOCK_HUB_CONFIG_DESCRIPTOR: [u8; 25] = [
    // Configuration: wTotalLength=25, 1 interface.
    0x09, 0x02, 0x19, 0x00, 0x01, 0x01, 0x00, 0xA0, 0x32, //
    // Interface: class=0x09 (hub), sub=0x00, protocol=0x00, 1 endpoint.
    0x09, 0x04, 0x00, 0x00, 0x01, 0x09, 0x00, 0x00, 0x00, //
    // Endpoint: bEndpointAddress=0x82 (EP2 IN -> DCI 5, distinct from a
    // downstream keyboard's DCI 3), interrupt, wMaxPacketSize=1 (the
    // port-change bitmap byte), bInterval=12.
    0x07, 0x05, 0x82, 0x03, 0x01, 0x00, 0x0C,
];

/// The device descriptor fixture for a mass-storage device (class in the
/// interface descriptor, vendor `0x0781` product `0x5567` — a generic
/// flash-disk identity).
const MOCK_MSD_DESCRIPTOR: [u8; 18] = [
    18, 0x01, 0x00, 0x02, 0x00, 0x00, 0x00, 0x40, 0x81, 0x07, 0x67, 0x55, 0x00, 0x01, 0x00, 0x00,
    0x00, 0x01,
];

/// The configuration descriptor fixture for the mass-storage device: one
/// interface of class `08:06:50` (mass storage, SCSI transparent, bulk-only
/// transport) with a bulk-IN endpoint 0x83 (EP3 IN → DCI 7) and a bulk-OUT
/// endpoint 0x04 (EP4 OUT → DCI 8) — deliberately not endpoints 1/2, so a
/// driver that assumes the endpoint numbers is caught.
const MOCK_MSD_CONFIG_DESCRIPTOR: [u8; 32] = [
    // Configuration: wTotalLength=32, 1 interface.
    0x09, 0x02, 0x20, 0x00, 0x01, 0x01, 0x00, 0x80, 0x32, //
    // Interface: class=0x08, subclass=0x06 (SCSI), protocol=0x50 (BOT).
    0x09, 0x04, 0x00, 0x00, 0x02, 0x08, 0x06, 0x50, 0x00, //
    // Endpoint: 0x83 bulk IN, wMaxPacketSize=512.
    0x07, 0x05, 0x83, 0x02, 0x00, 0x02, 0x00, //
    // Endpoint: 0x04 bulk OUT, wMaxPacketSize=512.
    0x07, 0x05, 0x04, 0x02, 0x00, 0x02, 0x00,
];

/// The device descriptor fixture for a HID boot **mouse** (class in the
/// interface descriptor, vendor `0x046D` product `0xC539` — a generic
/// three-button wheel mouse identity, deliberately distinct from the
/// keyboard fixture's product id so per-index identities are assertable).
const MOCK_MOUSE_DESCRIPTOR: [u8; 18] = [
    18, 0x01, 0x00, 0x02, 0x00, 0x00, 0x00, 0x40, 0x6D, 0x04, 0x39, 0xC5, 0x00, 0x01, 0x00, 0x00,
    0x00, 0x01,
];

/// The configuration descriptor fixture for the boot mouse: one interface
/// of class `0x03_01_02` (HID, boot, mouse) with an interrupt-IN endpoint 1
/// (DCI 3), `wMaxPacketSize` = 4 (buttons + X + Y + wheel).
const MOCK_MOUSE_CONFIG_DESCRIPTOR: [u8; 25] = [
    // Configuration: wTotalLength=25, 1 interface.
    0x09, 0x02, 0x19, 0x00, 0x01, 0x01, 0x00, 0xA0, 0x32, //
    // Interface: class=0x03 (HID), sub=0x01 (boot), protocol=0x02 (mouse).
    0x09, 0x04, 0x00, 0x00, 0x01, 0x03, 0x01, 0x02, 0x00, //
    // Endpoint: 0x81 interrupt IN, wMaxPacketSize=4, bInterval=10.
    0x07, 0x05, 0x81, 0x03, 0x04, 0x00, 0x0A,
];

/// The device descriptor fixture for a **composite** wireless
/// keyboard+mouse receiver (vendor `0x046D` product `0xC534` — a generic
/// unifying-receiver identity): one device whose single configuration
/// carries a boot-keyboard interface *and* a boot-mouse interface, the
/// adapter shape whose second function used to be invisible. Like the
/// real receiver it is a full-speed device with `bMaxPacketSize0` = 8
/// (byte 7), so any EP0 IN read longer than 8 bytes fails until the
/// driver re-evaluates the EP0 context to the honest size.
const MOCK_COMPOSITE_DESCRIPTOR: [u8; 18] = [
    18, 0x01, 0x00, 0x02, 0x00, 0x00, 0x00, 0x08, 0x6D, 0x04, 0x34, 0xC5, 0x00, 0x29, 0x00, 0x00,
    0x00, 0x01,
];

/// As [`MOCK_COMPOSITE_DESCRIPTOR`], but forging `bMaxPacketSize0` = 7 —
/// a value no full-speed device may report (USB 2.0 §5.5.3 allows only
/// 8/16/32/64) — so the driver must reject the device fail-closed rather
/// than program a nonsense EP0 context.
const MOCK_COMPOSITE_DESCRIPTOR_FORGED_EP0: [u8; 18] = [
    18, 0x01, 0x00, 0x02, 0x00, 0x00, 0x00, 0x07, 0x6D, 0x04, 0x34, 0xC5, 0x00, 0x29, 0x00, 0x00,
    0x00, 0x01,
];

/// The configuration descriptor for the composite receiver: interface 0 is
/// a boot keyboard (EP1 IN → DCI 3), interface 1 a boot mouse (EP2 IN →
/// DCI 5), each with a HID descriptor between the interface and endpoint
/// descriptors, followed by an **alternate setting** of interface 1 whose
/// EP3 endpoint must be skipped (only the default setting is served).
/// `wTotalLength` = 75 deliberately exceeds a 64-byte read, so the full
/// configuration must be fetched or the mouse interface is truncated away.
const MOCK_COMPOSITE_CONFIG_DESCRIPTOR: [u8; 75] = [
    // Configuration: wTotalLength=75, 2 interfaces, bConfigurationValue=1.
    0x09, 0x02, 0x4B, 0x00, 0x02, 0x01, 0x00, 0xA0, 0x32, //
    // Interface 0: class=0x03 (HID), sub=0x01 (boot), protocol=0x01
    // (keyboard).
    0x09, 0x04, 0x00, 0x00, 0x01, 0x03, 0x01, 0x01, 0x00, //
    // HID descriptor (type 0x21).
    0x09, 0x21, 0x11, 0x01, 0x00, 0x01, 0x22, 0x3F, 0x00, //
    // Endpoint: 0x81 interrupt IN, wMaxPacketSize=8, bInterval=10.
    0x07, 0x05, 0x81, 0x03, 0x08, 0x00, 0x0A, //
    // Interface 1: class=0x03 (HID), sub=0x01 (boot), protocol=0x02
    // (mouse).
    0x09, 0x04, 0x01, 0x00, 0x01, 0x03, 0x01, 0x02, 0x00, //
    // HID descriptor.
    0x09, 0x21, 0x11, 0x01, 0x00, 0x01, 0x22, 0x40, 0x00, //
    // Endpoint: 0x82 interrupt IN, wMaxPacketSize=8, bInterval=10.
    0x07, 0x05, 0x82, 0x03, 0x08, 0x00, 0x0A, //
    // Interface 1 **alternate setting 1**: its EP3 endpoint must be
    // skipped, never mistaken for the default setting's.
    0x09, 0x04, 0x01, 0x01, 0x01, 0x03, 0x01, 0x02, 0x00, //
    0x07, 0x05, 0x83, 0x03, 0x08, 0x00, 0x0A,
];

/// Register-level xHCI model: the capability block, `USBCMD`/`USBSTS`
/// halt/reset behaviour, four `PORTSC` ports, a doorbell write log,
/// and — when a shared DMA buffer is attached — an in-memory device
/// model that consumes the command/transfer rings and produces events
/// exactly as a controller with one attached HID device would.
///
/// The booleans mirror independent hardware bits and fault-injection
/// knobs, not a state machine — the `struct_excessive_bools` lint is
/// allowed here for the same reason as the `emmc2` `MockSdhci`.
#[allow(clippy::struct_excessive_bools)]
struct MockXhci {
    cap_dword0: u32,
    hcsparams1: u32,
    hccparams1: u32,
    dboff: u32,
    rtsoff: u32,
    usbcmd: u32,
    portsc: [u32; 4],
    /// `USBSTS` reads report Controller Not Ready until this many
    /// status reads have happened.
    cnr_reads: u32,
    /// `USBCMD` reads keep `HCRST` set for this many reads after a
    /// reset is requested (models the self-clearing bit).
    hcrst_reads: u32,
    /// When set, `HCRST` never self-clears (a stuck controller).
    hcrst_stuck: bool,
    /// When set, `USBSTS` reports Controller Not Ready forever.
    cnr_stuck: bool,
    /// When set, `USBSTS` reports a latched Host System Error until a
    /// host-controller reset clears it.
    hse_latched: bool,
    /// When set, `USBSTS` reports a latched Event Interrupt until a
    /// write-1-to-clear status write clears it.
    eint_latched: bool,
    /// When set, `USBSTS` reports a latched Port Change Detect until a
    /// write-1-to-clear status write clears it.
    pcd_latched: bool,
    /// When set, a status write is only made visible by the next
    /// `USBSTS` read, modelling a posted bridge write that must be
    /// flushed before the reset command.
    status_write_needs_read_flush: bool,
    pending_status_clear: u32,
    doorbells: Vec<(usize, u32)>,
    /// `PORTSC` reads report Port Reset in progress for this many
    /// reads after a reset write (models the self-clearing bit).
    port_reset_reads: u32,
    /// The port index a reset is in progress on.
    port_reset_port: usize,
    /// The shared DMA buffer, when the device model is attached.
    mem: Option<SharedMem>,
    // Captured DMA-programming registers.
    config: u32,
    dcbaap: [u32; 2],
    crcr: [u32; 2],
    erstsz: u32,
    erstba: [u32; 2],
    erdp: [u32; 2],
    /// Interrupter 0 management register (`IMAN`): IE/IP bits.
    iman: u32,
    /// Interrupter 0 moderation register (`IMOD`).
    imod: u32,
    /// Event Handler Busy (`ERDP.EHB`): the controller sets it when it
    /// asserts the interrupt and refuses to re-assert `IMAN.IP` for a later
    /// event while it is set; software clears it by writing `ERDP` with the
    /// EHB bit. Modelled so a regression can prove a zero-event interrupt
    /// still clears it (otherwise the controller goes silent — the metal
    /// keyboard bug).
    event_handler_busy: bool,
    // Device-model ring consumer / event producer state.
    cmd_index: usize,
    cmd_cycle: bool,
    ep0_base: u64,
    ep0_index: usize,
    ep0_cycle: bool,
    /// The slot whose EP0 ring is currently the live `ep0_base`/`ep0_index`/
    /// `ep0_cycle`. The engine keeps a hub and a downstream device addressed
    /// at once and switches the active control context between them; a
    /// control doorbell for a different slot saves the live ring state and
    /// loads that slot's, mirroring the DCBAA-indexed hardware.
    ep0_slot: u8,
    /// Saved per-slot EP0 ring `(base, index, cycle)`, indexed by slot id.
    ep0_saved: [(u64, usize, bool); 33],
    /// Per-slot EP0 Max Packet Size programmed by Address Device and
    /// re-evaluated by Evaluate Context, indexed by slot id. When it
    /// overstates the addressed device's real `bMaxPacketSize0`, a
    /// standard-descriptor IN stage delivers only one device-sized packet
    /// before the controller ends the TD short — the metal fault a
    /// full-speed wireless receiver with an 8-byte EP0 hits.
    ep0_max: [u16; 33],
    /// Evaluate Context commands executed, so a test can assert the EP0
    /// max-packet fix-up ran exactly when the descriptor demanded it.
    evaluate_context_count: usize,
    int_base: u64,
    int_index: usize,
    int_cycle: bool,
    event_index: usize,
    event_cycle: bool,
    /// Address of the most-recently-posted event-ring slot, so a test can
    /// model the non-coherent hazard where the controller's cycle bit is
    /// visible while the entry body has not yet reached RAM
    /// ([`Self::unland_last_event`] / [`Self::land_last_event`]).
    last_event_addr: u64,
    /// The real body of an event temporarily "un-landed" (body zeroed, cycle
    /// bit kept) to model that hazard; `land_last_event` restores it.
    unlanded_event: Option<Trb>,
    unlanded_addr: u64,
    // Device-model device state.
    next_slot: u8,
    active_slot: u8,
    addressed: bool,
    configured: bool,
    configuration: Option<u8>,
    protocol: Option<u8>,
    pending_setup: Option<[u8; 8]>,
    /// Pending IN data stage: TRB address, buffer, length, ISP.
    pending_data: Option<(u64, u64, u32, bool)>,
    pending_reports: VecDeque<Vec<u8>>,
    /// When set, class requests (`SET_PROTOCOL`) answer STALL — the
    /// optional-request case `control_optional` tolerates.
    stall_class_requests: bool,
    /// When set, class requests (`SET_PROTOCOL`) answer a non-STALL
    /// transaction error — a genuine fault `control_optional` must
    /// still surface.
    fault_class_requests: bool,
    /// When set, report completions forge a residual above the TRB
    /// length (a hostile controller claim).
    forge_report_residual: bool,
    /// When set, the **next** interrupt report posts this completion code
    /// (instead of Success/ShortPacket) and clears the knob — modelling a
    /// single odd transfer event the driver rejects per-report. The
    /// endpoint must still be re-armed so the following report is
    /// delivered (a single rejected report must never silence the
    /// keyboard).
    fault_one_report_completion: Option<CompletionCode>,
    /// When set, a `DisableSlot` command posts **no** completion event,
    /// modelling the metal hot-removal where the gone device's hub never
    /// lets the controller acknowledge the Disable Slot in time. The
    /// best-effort teardown must still free the slot locally so a re-plug
    /// re-enumerates.
    suppress_disable_completion: bool,
    /// A root-hub port (0-based) whose device only reports Current
    /// Connect Status once software writes Port Power — modelling a
    /// port-power-controlled controller (the VL805, `HCCPARAMS1`
    /// PPC = 1), where an unpowered port reads disconnected.
    latent_device_port: Option<usize>,
    /// `HCSPARAMS2` value the mock reports (the split Max Scratchpad
    /// Buffers fields). `0` (default) needs no scratchpad; a non-zero
    /// count models the VL805, which executes **no** command until
    /// software points `DCBAA[0]` at a programmed scratchpad array
    /// (xHCI §4.20).
    hcsparams2: u32,
    /// `PAGESIZE` value the mock reports (`1` → 4 KiB scratchpad pages).
    pagesize: u32,
    /// When non-zero, the attached device is a USB **hub** reporting
    /// this many downstream ports; its device/config descriptors switch
    /// to the hub fixtures (class `0x09`), mirroring the Pi 4B's onboard
    /// `2109:3431` VIA Labs hub.
    hub_ports: u8,
    /// The slot the root-attached hub currently occupies, recorded at its
    /// Address Device (route string `0`). A re-attached hub takes a fresh
    /// slot, and its downstream devices' transaction-translator
    /// coordinates must name *that* slot, never a hard-wired first one.
    root_hub_slot: u8,
    /// The root-hub port of the most recent root-attached Address Device
    /// (slot-context dword 1), so the fixture model can carry the hub on
    /// root port 1 and a plain leaf device on another root port at once.
    addressed_root_port: u8,
    /// The 1-based downstream hub port a device is attached to (`0` =
    /// none), with that device's `wPortStatus` value.
    hub_downstream_port: u8,
    hub_downstream_status: u16,
    /// Bitmask of downstream hub ports software has powered (bit `n-1`
    /// for port `n`); a downstream port reports a connected device only
    /// once powered, modelling a port-power-controlled hub.
    hub_powered: u32,
    /// When set, the class `GET_DESCRIPTOR(hub)` reply carries a wrong
    /// `bDescriptorType` — a forged/corrupt descriptor the driver must
    /// reject fail-closed.
    forge_hub_descriptor: bool,
    /// The next N class `GET_DESCRIPTOR(hub)` replies deliver
    /// configuration-descriptor-shaped bytes with a *successful* transfer
    /// — the RTS5411 metal signature where the exchange completes but the
    /// bytes are not a hub descriptor — then honest replies follow, so the
    /// driver's bounded retry is what rescues the attach.
    garble_hub_descriptor_replies: u8,
    /// The root-attached hub (and the leaf behind it) is a **`SuperSpeed`**
    /// device: the root port trains at protocol speed 4, the fixtures
    /// carry bcdUSB 3.00 / `bMaxPacketSize0 = 9`, the hub serves only the
    /// 0x2A SS hub descriptor (refusing a 0x29 request with a STALL, as real SS hubs
    /// do), and it accepts the `SET_HUB_DEPTH` request.
    superspeed_hub: bool,
    /// The `wValue` of the last hub-class `SET_HUB_DEPTH` received, so a
    /// test pins that an SS hub is told its tier depth before its ports
    /// are descended. `None` until the request arrives.
    hub_depth_set: Option<u8>,
    /// Whether the default control endpoint is halted. A control
    /// transfer that STALLs halts EP0 in xHCI (§4.8.3 / §4.10.2.4): the
    /// controller runs no further TRBs on it until software resets the
    /// endpoint, so a subsequent control transfer faults. This models
    /// that, catching code that reuses EP0 after a tolerated STALL.
    ep0_halted: bool,
    /// Command blocks delivered over the class ADSC control-OUT data
    /// stage (the CBI command channel).
    adsc_blocks: Vec<Vec<u8>>,
    /// When set, every downstream-port class `GET_STATUS` (USB 2.0
    /// §11.24.2.7) STALLs — modelling the metal failure where the
    /// hub-descriptor read succeeds but each per-port status read
    /// faults, so the bring-up diagnostic must surface the completion
    /// code.
    fault_hub_port_status: bool,
    /// When non-zero, every downstream-port class `GET_STATUS` posts a
    /// transfer event carrying this *raw* completion-code byte — used to
    /// model a controller-specific/reserved code the driver does not
    /// decode (the metal `completion_hex=0` was a code the diagnostic
    /// failed to record, not a true timeout).
    fault_hub_port_status_raw: u8,
    /// When non-zero, every downstream-port class `GET_STATUS` posts an
    /// event carrying this *raw TRB-type* (rather than a Transfer
    /// Event) — modelling an unexpected asynchronous controller event
    /// reaching the wait, which `await_event_for` rejects fast without
    /// recording a completion code (the metal `completion_hex=0` +
    /// fast-failure signature).
    fault_hub_port_status_evtype: u8,
    /// Bitmask of downstream hub ports software has reset (bit `n-1` for
    /// port `n`) via a class `SET_FEATURE(PORT_RESET)`; a reset port
    /// reports `PORT_STATUS_ENABLE` in its `wPortStatus`, the gate a
    /// downstream device must pass before it is addressed.
    hub_reset: u32,
    /// Set once an Address Device with a non-zero Route String has been
    /// processed: the active addressed device is now the **downstream**
    /// HID device (the keyboard behind the hub), so descriptor reads
    /// answer with the HID fixtures and the HID class requests succeed.
    downstream_active: bool,
    /// The downstream hub port the addressed device's Route String named,
    /// captured for the test to assert against.
    downstream_route_port: u8,
    /// Set once a Configure Endpoint that names only the slot context
    /// (Add flag `A0`) with the **Hub** bit set is processed: the parent
    /// hub has been marked a hub in its slot context, so the controller
    /// will schedule the split transactions a downstream device needs.
    /// Real hardware delivers no downstream interrupt transfer until
    /// this is done — the metal bug where the keyboard was addressed but
    /// never typed — so the mock gates [`Self::process_int_ring`] on it.
    hub_marked_as_hub: bool,
    /// The **Number of Ports** the hub-marking Configure Endpoint carried
    /// in the slot context (§6.2.2 dword 1), captured for assertions.
    hub_ctx_num_ports: u8,
    /// The **TT Think Time** the hub-marking Configure Endpoint carried
    /// in the slot context (§6.2.2 dword 2), captured for assertions.
    hub_ctx_tt_think_time: u8,
    /// The **Max ESIT Payload** the interrupt-IN Configure Endpoint
    /// carried in the endpoint context (§6.2.3.8 dword 4 bits 16:31).
    /// The xHCI periodic scheduler reserves no bandwidth for a periodic
    /// endpoint whose Max ESIT Payload is zero (§4.14.2), so real
    /// hardware delivers no interrupt transfer — the metal bug where the
    /// addressed keyboard never typed. The mock gates
    /// [`Self::process_int_ring`] on it being non-zero.
    int_max_esit: u32,
    /// The configuration-descriptor fixture answered for the keyboard
    /// (the non-hub device). A test can point this at a fixture whose
    /// interrupt endpoint is not endpoint 1 to prove the driver reads
    /// the endpoint's real DCI rather than assuming it.
    keyboard_config: &'static [u8],
    /// Device Context Index the interrupt-IN Configure Endpoint named,
    /// derived from its Add Context flags (§6.2.3) rather than assumed.
    /// The mock posts interrupt Transfer Events with it, so a keyboard
    /// whose interrupt endpoint is not endpoint 1 is serviced honestly
    /// (the metal no-report bug was the driver hard-coding DCI 3).
    int_dci: u8,
    /// The slot marked as a hub (the Configure Endpoint that raised the Hub
    /// bit), so a later endpoint-add on that slot is recognised as the hub's
    /// status-change endpoint rather than the downstream device's interrupt
    /// endpoint. `0` until a hub is marked.
    hub_slot_id: u8,
    /// The hub status-change endpoint's transfer-ring base / DCI / consumer
    /// state, set by the Configure Endpoint that adds it to the hub slot. The
    /// test posts a port-change report with [`Self::post_hub_status_change`].
    hub_int_base: u64,
    hub_int_dci: u8,
    hub_int_index: usize,
    hub_int_cycle: bool,
    /// `wPortChange` (USB 2.0 §11.24.2.7.2) the downstream-port `GET_STATUS`
    /// reports — the latched port changes (e.g. Connect Status Change). `0`
    /// = no change latched.
    hub_downstream_change: u16,
    /// When set, the attached (non-hub) device is a **mass-storage** device:
    /// the descriptor fixtures switch to the MSD pair (interface class
    /// `08:06:50` with the bulk endpoint pair) instead of the HID keyboard.
    msd_device: bool,
    /// A second downstream hub port carrying a mass-storage device, so a
    /// keyboard and a storage stick hang off the hub at once (`0` = none).
    /// It shares [`Self::hub_downstream_status`]; the change latch stays
    /// keyed to [`Self::hub_downstream_port`].
    msd_downstream_port: u8,
    /// A downstream hub port carrying a HID boot **mouse** (`0` = none):
    /// the addressed device on this port answers with the mouse fixtures
    /// and its interrupt endpoint is captured as the second HID endpoint
    /// ([`Self::int2_slot`]), so a keyboard and a mouse hang off the hub
    /// at once. It shares [`Self::hub_downstream_status`]; the change
    /// latch stays keyed to [`Self::hub_downstream_port`].
    mouse_downstream_port: u8,
    /// A downstream hub port carrying the **composite** keyboard+mouse
    /// receiver (`0` = none): the addressed device on this port answers
    /// with the composite fixtures, its first interrupt endpoint is
    /// captured as the primary HID endpoint (`int_*`) and its second — the
    /// mouse interface on the **same slot** — as [`Self::int2`]. It shares
    /// [`Self::hub_downstream_status`]; the change latch stays keyed to
    /// [`Self::hub_downstream_port`].
    composite_downstream_port: u8,
    /// When set, the composite receiver's device descriptor forges a
    /// `bMaxPacketSize0` no full-speed device may report
    /// ([`MOCK_COMPOSITE_DESCRIPTOR_FORGED_EP0`]), so its enumeration must
    /// fail closed without costing the other ports their service.
    forge_composite_ep0_max: bool,
    /// A downstream hub port whose device never reports
    /// `PORT_STATUS_ENABLE` after a reset (`0` = none) — a broken or
    /// half-seated device whose enumeration must fail without costing the
    /// other ports their service and without leaving the port's change
    /// latches set.
    fail_enable_downstream_port: u8,
    /// `GET_STATUS` reads on the watched downstream port that report the
    /// reset still in progress (connected, `PORT_STATUS_RESET` set, not
    /// yet enabled) before the port finally reports enabled — a slow hub
    /// that legitimately takes several polls to complete a downstream
    /// reset (`0` = the reset completes by the first read). Decremented
    /// per such read.
    slow_enable_status_reads: u32,
    /// The second configured HID interrupt endpoint (the mouse beside the
    /// keyboard), captured when the addressed device on
    /// [`Self::mouse_downstream_port`] has its interrupt endpoint
    /// configured. Its completions are posted with its own slot, so the
    /// engine's per-device demux is exercised.
    int2: MockInt,
    /// Scripted reports for the second HID endpoint, mirroring
    /// [`Self::pending_reports`].
    pending_reports2: VecDeque<Vec<u8>>,
    /// The xHCI slot whose interrupt-IN endpoint the `int_*` state models
    /// (recorded at Configure Endpoint), so its transfer events carry that
    /// slot even after a later device becomes the most recently addressed.
    int_slot: u8,
    /// As [`Self::int_slot`], for the bulk endpoint pair.
    bulk_slot: u8,
    /// The downstream hubs plugged into the root hub's ports — each a
    /// [`NestedHub`] with its own port bank, slot, and status-change
    /// endpoint, so a deep multi-hub fan-out is modelled faithfully.
    nested_hubs: Vec<NestedHub>,
    /// The full Route String of the most recent Address Device
    /// ([`Self::downstream_route_port`] keeps only the low nibble, for the
    /// single-tier assertions).
    downstream_route: u32,
    /// The bulk-IN endpoint model, captured from the two-endpoint (bulk
    /// pair) Configure Endpoint.
    bulk_in: MockBulk,
    /// The bulk-OUT endpoint model, as [`Self::bulk_in`].
    bulk_out: MockBulk,
    /// Scripted device responses for bulk-IN TDs, one consumed per TD; a TD
    /// with no queued response stays pending (the device has not produced
    /// data yet).
    bulk_in_responses: VecDeque<Vec<u8>>,
    /// Bytes each completed bulk-OUT TD delivered to the device.
    bulk_out_received: Vec<Vec<u8>>,
}

/// One direction of the mock's bulk endpoint model: the transfer ring's
/// base / consumer state, the DCI the Configure Endpoint named (`0` = not
/// configured), a one-shot STALL knob, and the recovery state machine.
struct MockBulk {
    base: u64,
    index: usize,
    cycle: bool,
    dci: u8,
    /// One-shot: the next serviced TD on this endpoint STALLs and halts it.
    stall_next: bool,
    /// Endpoint recovery state, modelling the xHCI order the silicon
    /// requires: `0` running, `1` halted (STALL posted), `2` Reset Endpoint
    /// seen, `3` Set TR Dequeue Pointer seen; a device-side
    /// `CLEAR_FEATURE(ENDPOINT_HALT)` completes the recovery (`3` → `0`).
    /// A halted endpoint's ring is not serviced, so a driver that skips or
    /// re-orders a recovery step fails loudly.
    halt: u8,
}

impl MockBulk {
    const fn new() -> Self {
        Self {
            base: 0,
            index: 0,
            cycle: true,
            dci: 0,
            stall_next: false,
            halt: 0,
        }
    }
}

/// A second modelled HID interrupt-IN endpoint (the mouse beside the
/// keyboard): the transfer ring's base / consumer state, the DCI the
/// Configure Endpoint named, and the slot it was configured on (`0` =
/// not configured).
struct MockInt {
    base: u64,
    index: usize,
    cycle: bool,
    dci: u8,
    slot: u8,
}

impl MockInt {
    const fn new() -> Self {
        Self {
            base: 0,
            index: 0,
            cycle: true,
            dci: 0,
            slot: 0,
        }
    }
}

/// One downstream hub of the device model — a hub plugged into a hub.
/// The root hub's port [`Self::root_port`] carries it: the addressed
/// device on that port answers with the hub fixtures, reporting
/// [`Self::ports`] downstream ports of its own; once addressed, hub-class
/// requests riding its EP0 are served from this hub's own bank, keyed by
/// [`Self::slot`]. Its root-hub-port `wPortStatus` reads connected
/// (high-speed) while [`Self::connected`], with [`Self::root_change`] as
/// the port's latched changes. The model holds any number of these at
/// once ([`MockXhci::nested_hubs`]), so a deep multi-hub fan-out is
/// exercised host-side.
struct NestedHub {
    /// The root-hub downstream port carrying this hub.
    root_port: u8,
    /// This hub's downstream port count (its hub class descriptor's
    /// `bNbrPorts`).
    ports: u8,
    /// The slot this hub was addressed on, captured at Address Device
    /// (`0` = not yet addressed).
    slot: u8,
    /// Whether this hub is physically present on its root-hub port.
    connected: bool,
    /// The root-hub port's latched `wPortChange` for this hub's port
    /// (the unplug/replug connect change).
    root_change: u16,
    /// Bitmask of this hub's downstream ports software has powered.
    powered: u32,
    /// Bitmask of this hub's downstream ports software has reset.
    reset: u32,
    /// This hub's downstream port carrying a keyboard (`0` = none), with
    /// that device's `wPortStatus` and latched `wPortChange`.
    downstream_port: u8,
    downstream_status: u16,
    downstream_change: u16,
    /// Set once a hub-topology Configure Endpoint marked this hub's slot
    /// (the root hub's marking is [`MockXhci::hub_marked_as_hub`]).
    marked: bool,
    /// This hub's interrupt-IN status-change endpoint, captured at
    /// Configure Endpoint on the marked slot; a change is posted with
    /// [`MockXhci::post_nested_hub_status_change`].
    int: MockInt,
}

impl NestedHub {
    /// A connected, empty-ported hub on the root hub's `root_port` with
    /// `ports` downstream ports of its own.
    const fn new(root_port: u8, ports: u8) -> Self {
        Self {
            root_port,
            ports,
            slot: 0,
            connected: true,
            root_change: 0,
            powered: 0,
            reset: 0,
            downstream_port: 0,
            downstream_status: 0,
            downstream_change: 0,
            marked: false,
            int: MockInt::new(),
        }
    }
}

impl MockXhci {
    // A flat field-initialiser list: every line is one register default or
    // model knob, which reads more clearly as one literal than split
    // across artificial helpers.
    #[allow(clippy::too_many_lines)]
    fn new() -> Self {
        Self {
            cap_dword0: 0x0110_0000 | MOCK_CAPLENGTH, // xHCI 1.1
            hcsparams1: 0x0400_0020,                  // 4 ports, 32 slots
            hccparams1: 0x0000_0005,                  // AC64 + CSZ
            dboff: MOCK_DBOFF,
            rtsoff: MOCK_RTSOFF,
            usbcmd: 0,
            portsc: [0; 4],
            cnr_reads: 0,
            hcrst_reads: 0,
            hcrst_stuck: false,
            cnr_stuck: false,
            hse_latched: false,
            eint_latched: false,
            pcd_latched: false,
            status_write_needs_read_flush: false,
            pending_status_clear: 0,
            doorbells: Vec::new(),
            port_reset_reads: 0,
            port_reset_port: 0,
            mem: None,
            config: 0,
            dcbaap: [0; 2],
            crcr: [0; 2],
            erstsz: 0,
            erstba: [0; 2],
            erdp: [0; 2],
            iman: 0,
            imod: 0,
            event_handler_busy: false,
            cmd_index: 0,
            cmd_cycle: true,
            ep0_base: 0,
            ep0_index: 0,
            ep0_cycle: true,
            ep0_slot: 0,
            ep0_saved: [(0, 0, true); 33],
            root_hub_slot: 1,
            addressed_root_port: 1,
            ep0_max: [0; 33],
            evaluate_context_count: 0,
            int_base: 0,
            int_index: 0,
            int_cycle: true,
            event_index: 0,
            event_cycle: true,
            last_event_addr: 0,
            unlanded_event: None,
            unlanded_addr: 0,
            next_slot: 1,
            active_slot: 0,
            addressed: false,
            configured: false,
            configuration: None,
            protocol: None,
            pending_setup: None,
            pending_data: None,
            pending_reports: VecDeque::new(),
            stall_class_requests: false,
            fault_class_requests: false,
            forge_report_residual: false,
            fault_one_report_completion: None,
            suppress_disable_completion: false,
            latent_device_port: None,
            hcsparams2: 0,
            pagesize: 0,
            hub_ports: 0,
            hub_downstream_port: 0,
            hub_downstream_status: 0,
            hub_powered: 0,
            forge_hub_descriptor: false,
            garble_hub_descriptor_replies: 0,
            superspeed_hub: false,
            hub_depth_set: None,
            ep0_halted: false,
            adsc_blocks: Vec::new(),
            fault_hub_port_status: false,
            fault_hub_port_status_raw: 0,
            fault_hub_port_status_evtype: 0,
            hub_reset: 0,
            downstream_active: false,
            downstream_route_port: 0,
            hub_marked_as_hub: false,
            hub_ctx_num_ports: 0,
            hub_ctx_tt_think_time: 0,
            int_max_esit: 0,
            keyboard_config: &MOCK_CONFIG_DESCRIPTOR,
            int_dci: 3,
            msd_device: false,
            msd_downstream_port: 0,
            mouse_downstream_port: 0,
            composite_downstream_port: 0,
            forge_composite_ep0_max: false,
            fail_enable_downstream_port: 0,
            slow_enable_status_reads: 0,
            int2: MockInt::new(),
            pending_reports2: VecDeque::new(),
            int_slot: 0,
            bulk_slot: 0,
            nested_hubs: Vec::new(),
            downstream_route: 0,
            bulk_in: MockBulk::new(),
            bulk_out: MockBulk::new(),
            bulk_in_responses: VecDeque::new(),
            bulk_out_received: Vec::new(),
            hub_slot_id: 0,
            hub_int_base: 0,
            hub_int_dci: 0,
            hub_int_index: 0,
            hub_int_cycle: true,
            hub_downstream_change: 0,
        }
    }

    /// A mock with the device model attached as a USB **hub** on
    /// root-hub port 1 (a high-speed device, enabled), reporting `ports`
    /// downstream ports with a high-speed device on downstream port
    /// `downstream`. The downstream port reports a connected device only
    /// once software powers it — mirroring the Pi 4B's onboard
    /// `2109:3431` hub and its keyboard.
    fn with_hub(mem: &SharedMem, ports: u8, downstream: u8) -> Self {
        let mut mock = Self::with_device(mem);
        mock.hub_ports = ports;
        mock.hub_downstream_port = downstream;
        // Current Connect Status (bit 0) | High-Speed Device (bit 10).
        mock.hub_downstream_status = (1 << 0) | (1 << 10);
        mock
    }

    /// As [`Self::with_hub`], but the hub tier is **`SuperSpeed`**: the
    /// root port trains at protocol speed 4 and the hub serves only the
    /// 12-byte 0x2A SS hub descriptor — the multi-drive-enclosure shape a
    /// Pi 4's USB3 root port presents. The downstream leaf is a
    /// `SuperSpeed` device whose exponent-encoded `bMaxPacketSize0` refuses
    /// any misdecoded (USB 2.0) downstream speed.
    fn with_ss_hub(mem: &SharedMem, ports: u8, downstream: u8) -> Self {
        let mut mock = Self::with_hub(mem, ports, downstream);
        mock.superspeed_hub = true;
        mock.portsc[0] =
            regs::PORTSC_CCS | regs::PORTSC_PED | regs::PORTSC_PP | (4 << regs::PORTSC_SPEED_SHIFT);
        // An SS hub's downstream `wPortStatus` reserves the USB 2.0 speed
        // bits: a connected device reports connect status alone.
        mock.hub_downstream_status = 1 << 0;
        mock
    }

    /// As [`Self::with_hub`], but with a **nested hub** on the root hub's
    /// downstream port 3: a high-speed hub reporting four downstream ports
    /// of its own, with a full-speed keyboard on its downstream port 2 —
    /// the hub-plugged-into-a-hub topology. The root hub carries no other
    /// device.
    fn with_nested_hub(mem: &SharedMem) -> Self {
        let mut mock = Self::with_hub(mem, 4, 0);
        let mut hub = NestedHub::new(3, 4);
        hub.downstream_port = 2;
        // Current Connect Status only: a full-speed keyboard, so its
        // transactions split through the nested hub's TT.
        hub.downstream_status = 1 << 0;
        mock.nested_hubs.push(hub);
        mock
    }

    /// As [`Self::with_hub`], but with `count` downstream hubs fanned out
    /// on the root hub's ports `1..=count`, each carrying a full-speed
    /// keyboard on its own downstream port 2 — the deep multi-hub fan-out
    /// of a real cascaded hub assembly with a leaf device behind every
    /// tier.
    fn with_hub_fanout(mem: &SharedMem, root_ports: u8, count: u8) -> Self {
        let mut mock = Self::with_hub(mem, root_ports, 0);
        for port in 1..=count {
            let mut hub = NestedHub::new(port, 4);
            hub.downstream_port = 2;
            // Current Connect Status only: a full-speed device, so its
            // transactions split through its own hub's TT.
            hub.downstream_status = 1 << 0;
            mock.nested_hubs.push(hub);
        }
        mock
    }

    /// Index of the nested hub carried on root-hub port `port`, if any.
    fn nested_by_root_port(&self, port: u8) -> Option<usize> {
        if port == 0 {
            return None;
        }
        self.nested_hubs.iter().position(|h| h.root_port == port)
    }

    /// Index of the nested hub addressed on `slot` (`0` never matches).
    fn nested_by_slot(&self, slot: u8) -> Option<usize> {
        if slot == 0 {
            return None;
        }
        self.nested_hubs.iter().position(|h| h.slot == slot)
    }

    /// A mock with the device model attached and a high-speed HID
    /// device connected and enabled on root-hub port 1.
    fn with_device(mem: &SharedMem) -> Self {
        let mut mock = Self::new();
        mock.mem = Some(Rc::clone(mem));
        mock.portsc[0] =
            regs::PORTSC_CCS | regs::PORTSC_PED | regs::PORTSC_PP | (3 << regs::PORTSC_SPEED_SHIFT);
        mock
    }

    /// As [`Self::with_device`], but the attached device is a mass-storage
    /// device (interface class `08:06:50` with the bulk endpoint pair).
    fn with_msd_device(mem: &SharedMem) -> Self {
        let mut mock = Self::with_device(mem);
        mock.msd_device = true;
        mock
    }

    /// As [`Self::with_device`], but the controller requires `count`
    /// page-sized scratchpad buffers (the VL805 needs 31) and reports a
    /// 4 KiB page size — and, modelling the real hardware, posts **no**
    /// command completion until software programs `DCBAA[0]`
    /// ([`Self::scratchpad_unprogrammed`]).
    fn with_device_scratchpad(mem: &SharedMem, count: u32) -> Self {
        let mut mock = Self::with_device(mem);
        // Split the count into the HCSPARAMS2 low (bits 31:27) and high
        // (bits 25:21) fields, matching `hcsparams2_max_scratchpad`.
        let lo = count & 0x1F;
        let hi = (count >> 5) & 0x1F;
        mock.hcsparams2 = (lo << 27) | (hi << 21);
        mock.pagesize = 1;
        mock
    }

    /// `true` while a scratchpad-requiring controller's `DCBAA[0]` (the
    /// scratchpad buffer array pointer) is still zero — it executes no
    /// command until software programs it (xHCI §4.20).
    fn scratchpad_unprogrammed(&self) -> bool {
        let dcbaa = Self::qword(self.dcbaap);
        if dcbaa == 0 {
            return true;
        }
        let entry = self.read_dwords(dcbaa, 2);
        entry[0] == 0 && entry[1] == 0
    }

    fn op(offset: usize) -> usize {
        MOCK_CAPLENGTH as usize + offset
    }

    fn ir0(offset: usize) -> usize {
        MOCK_RTSOFF as usize + regs::IR0_BASE + offset
    }

    /// Capture a write to an interrupter-0 register (the event-ring
    /// pointers and the interrupt-management/moderation registers),
    /// returning `true` if `offset` named one. Split out of `write32` to
    /// keep that dispatcher under the line bound.
    fn write_interrupter(&mut self, offset: usize, value: u32) -> bool {
        if offset == Self::ir0(regs::IR_ERSTSZ) {
            self.erstsz = value;
        } else if offset == Self::ir0(regs::IR_ERSTBA) {
            self.erstba[0] = value;
        } else if offset == Self::ir0(regs::IR_ERSTBA) + 4 {
            self.erstba[1] = value;
        } else if offset == Self::ir0(regs::IR_ERDP) {
            // EHB (bit 3) is write-1-to-clear; the dequeue pointer is the
            // upper bits. Clear Event Handler Busy when the write sets it and
            // store only the pointer, mirroring a read returning EHB low.
            if value & regs::ERDP_EHB != 0 {
                self.event_handler_busy = false;
            }
            self.erdp[0] = value & !regs::ERDP_EHB;
        } else if offset == Self::ir0(regs::IR_ERDP) + 4 {
            self.erdp[1] = value;
        } else if offset == Self::ir0(regs::IR_IMAN) {
            // IP (bit 0) is write-1-to-clear; IE (bit 1) is read/write.
            // Clear IP if the write has it set, then store IE.
            if value & regs::IMAN_IP != 0 {
                self.iman &= !regs::IMAN_IP;
            }
            self.iman = (self.iman & regs::IMAN_IP) | (value & regs::IMAN_IE);
        } else if offset == Self::ir0(regs::IR_IMOD) {
            self.imod = value;
        } else {
            return false;
        }
        true
    }

    fn qword(pair: [u32; 2]) -> u64 {
        (u64::from(pair[1]) << 32) | u64::from(pair[0])
    }

    /// Model the controller asserting interrupter 0: it sets Event Handler
    /// Busy and `IMAN.IP` (and the global `EINT` latch) — but **only while
    /// EHB is clear**. Once EHB is set the controller does not re-assert `IP`
    /// for a later event until software clears EHB with an `ERDP` write, so a
    /// driver that never clears EHB on a zero-event interrupt goes silent.
    fn assert_event_interrupt(&mut self) {
        if self.event_handler_busy {
            return;
        }
        self.event_handler_busy = true;
        self.iman |= regs::IMAN_IP;
        self.eint_latched = true;
    }

    // ---- in-memory device model -------------------------------------

    fn mem_offset(addr: u64) -> usize {
        usize::try_from(addr - MOCK_DMA_BASE).expect("device address inside the shared buffer")
    }

    fn read_trb_at(&self, addr: u64) -> Trb {
        let mem = self.mem.as_ref().expect("device model attached").borrow();
        let off = Self::mem_offset(addr);
        let mut image = [0u8; TRB_LEN];
        image.copy_from_slice(&mem[off..off + TRB_LEN]);
        Trb::from_bytes(image)
    }

    /// Read `len` bytes of shared memory at device-visible `addr` (the
    /// device side of a bulk-OUT transfer).
    fn read_mem(&self, addr: u64, len: usize) -> Vec<u8> {
        let mem = self.mem.as_ref().expect("device model attached").borrow();
        let offset = Self::mem_offset(addr);
        mem[offset..offset + len].to_vec()
    }

    fn write_mem(&self, addr: u64, bytes: &[u8]) {
        let mut mem = self
            .mem
            .as_ref()
            .expect("device model attached")
            .borrow_mut();
        let off = Self::mem_offset(addr);
        mem[off..off + bytes.len()].copy_from_slice(bytes);
    }

    fn read_dwords(&self, addr: u64, count: usize) -> Vec<u32> {
        let mem = self.mem.as_ref().expect("device model attached").borrow();
        let off = Self::mem_offset(addr);
        (0..count)
            .map(|i| {
                u32::from_le_bytes([
                    mem[off + i * 4],
                    mem[off + i * 4 + 1],
                    mem[off + i * 4 + 2],
                    mem[off + i * 4 + 3],
                ])
            })
            .collect()
    }

    /// Produce one event TRB into the event segment named by the ERST.
    fn post_event(&mut self, mut event: Trb) {
        let erst = Self::qword(self.erstba);
        let entry = self.read_dwords(erst, 4);
        let segment = (u64::from(entry[1]) << 32) | u64::from(entry[0]);
        let len = usize::try_from(entry[2]).expect("segment length");
        event.control &= !CONTROL_CYCLE;
        if self.event_cycle {
            event.control |= CONTROL_CYCLE;
        }
        let addr = segment + (self.event_index * TRB_LEN) as u64;
        self.write_mem(addr, &event.to_bytes());
        self.last_event_addr = addr;
        self.event_index += 1;
        if self.event_index == len {
            self.event_index = 0;
            self.event_cycle = !self.event_cycle;
        }
    }

    /// Model the non-coherent BCM2711/VL805 hazard where the controller has
    /// advanced its event-ring enqueue and set the new entry's cycle bit, but
    /// the entry's 16-byte body has not yet reached RAM: zero the body of the
    /// most-recently-posted event while keeping its cycle bit, so the consumer
    /// sees a cycle-owned but all-zero (type 0) entry. [`Self::land_last_event`]
    /// restores the real body.
    fn unland_last_event(&mut self) {
        let addr = self.last_event_addr;
        let real = self.read_trb_at(addr);
        let zeroed = Trb {
            parameter: 0,
            status: 0,
            control: real.control & CONTROL_CYCLE,
        };
        self.write_mem(addr, &zeroed.to_bytes());
        self.unlanded_event = Some(real);
        self.unlanded_addr = addr;
    }

    /// Land the real body of the event previously hidden by
    /// [`Self::unland_last_event`], preserving the cycle bit already published.
    fn land_last_event(&mut self) {
        let real = self.unlanded_event.take().expect("an event was un-landed");
        self.write_mem(self.unlanded_addr, &real.to_bytes());
    }

    fn post_command_completion(&mut self, command_addr: u64, code: CompletionCode, slot: u8) {
        self.post_event(Trb {
            parameter: command_addr,
            status: u32::from(code.as_u8()) << 24,
            control: (u32::from(TrbType::CommandCompletion.as_u8()) << 10)
                | trb::control_slot(slot),
        });
    }

    fn post_transfer_event(&mut self, trb_addr: u64, code: CompletionCode, dci: u8, residual: u32) {
        self.post_transfer_event_raw(trb_addr, code.as_u8(), dci, residual);
    }

    /// Post a transfer event explicitly addressed to `slot`, so a test can
    /// model a *trailing* completion the controller posts for a slot the
    /// engine has already freed (after a hot-removal Disable Slot) — which no
    /// longer matches any live endpoint.
    fn post_transfer_event_for_slot(
        &mut self,
        trb_addr: u64,
        code: CompletionCode,
        dci: u8,
        residual: u32,
        slot: u8,
    ) {
        self.post_event(Trb {
            parameter: trb_addr,
            status: (u32::from(code.as_u8()) << 24) | residual,
            control: (u32::from(TrbType::TransferEvent.as_u8()) << 10)
                | (u32::from(dci) << 16)
                | trb::control_slot(slot),
        });
    }

    /// Post a transfer event carrying a *raw* completion-code byte — so
    /// a test can model a controller-specific or reserved code the
    /// driver's [`CompletionCode`] enum does not model (e.g. xHCI code
    /// `7`, Resource Error), which `await_event_for`'s decode rejects.
    fn post_transfer_event_raw(&mut self, trb_addr: u64, code: u8, dci: u8, residual: u32) {
        // A control-endpoint (DCI 1) transfer event belongs to the slot whose
        // EP0 ring is currently live (`ep0_slot`) — the engine keeps a hub and
        // its downstream devices addressed at once and switches the active
        // control context between them. Endpoint completions belong to the
        // slot whose Configure Endpoint installed the endpoint (`bulk_slot` /
        // `int_slot`), so two concurrently served devices' events carry their
        // own slots; anything else falls back to the most-recently-addressed
        // device slot.
        let slot = if dci == 1 {
            self.ep0_slot
        } else if self.bulk_slot != 0 && (dci == self.bulk_in.dci || dci == self.bulk_out.dci) {
            self.bulk_slot
        } else if self.int_slot != 0 && dci == self.int_dci {
            self.int_slot
        } else {
            self.active_slot
        };
        self.post_event(Trb {
            parameter: trb_addr,
            status: (u32::from(code) << 24) | residual,
            control: (u32::from(TrbType::TransferEvent.as_u8()) << 10)
                | (u32::from(dci) << 16)
                | trb::control_slot(slot),
        });
    }

    /// Post an event carrying an arbitrary *raw* TRB-type (control bits
    /// 15:10) at `trb_addr` — so a test can model an unexpected
    /// asynchronous controller event reaching a transfer/command wait,
    /// which `await_event_for` rejects as an unhandled type.
    fn post_event_raw_type(&mut self, trb_addr: u64, type_raw: u8) {
        self.post_event(Trb {
            parameter: trb_addr,
            status: u32::from(CompletionCode::Success.as_u8()) << 24,
            control: (u32::from(type_raw) << 10) | trb::control_slot(self.active_slot),
        });
    }

    /// Walk one producer ring from `(index, cycle)`, returning the next
    /// owned TRB and its address, following (and re-cycling over) the
    /// wrap Link TRB exactly as a consumer would (§4.9.2).
    fn next_owned(&self, base: u64, index: &mut usize, cycle: &mut bool) -> Option<(u64, Trb)> {
        loop {
            let addr = base + (*index * TRB_LEN) as u64;
            let trb = self.read_trb_at(addr);
            if trb.cycle() != *cycle {
                return None;
            }
            if trb.trb_type() == Ok(TrbType::Link) {
                if trb.control & trb::CONTROL_LINK_TOGGLE != 0 {
                    *cycle = !*cycle;
                }
                *index = 0;
                continue;
            }
            *index += 1;
            return Some((addr, trb));
        }
    }

    fn process_command_ring(&mut self) {
        // A controller that requires scratchpad buffers does not execute
        // any command until software points `DCBAA[0]` at the scratchpad
        // array (xHCI §4.20) — the VL805's metal `stage=2 completion=0`.
        if regs::hcsparams2_max_scratchpad(self.hcsparams2) > 0 && self.scratchpad_unprogrammed() {
            return;
        }
        let base = Self::qword(self.crcr) & !0x3F;
        loop {
            let (mut index, mut cycle) = (self.cmd_index, self.cmd_cycle);
            let Some((addr, trb)) = self.next_owned(base, &mut index, &mut cycle) else {
                return;
            };
            self.cmd_index = index;
            self.cmd_cycle = cycle;
            match trb.trb_type() {
                Ok(TrbType::EnableSlot) => {
                    let slot = self.next_slot;
                    self.next_slot += 1;
                    self.active_slot = slot;
                    self.post_command_completion(addr, CompletionCode::Success, slot);
                }
                Ok(TrbType::AddressDevice) => {
                    let code = self.handle_address_device(trb.parameter);
                    self.post_command_completion(addr, code, trb.slot_id());
                }
                Ok(TrbType::DisableSlot) => {
                    // Free a device slot on hot-removal (xHCI §6.4.3.3); the
                    // mock just acknowledges it (the engine clears its own
                    // per-device state and DCBAA entry). When
                    // `suppress_disable_completion` is set the controller posts
                    // no completion at all, modelling the metal hot-removal
                    // where the gone device's hub never lets the Disable Slot
                    // be acknowledged.
                    if !self.suppress_disable_completion {
                        self.post_command_completion(addr, CompletionCode::Success, trb.slot_id());
                    }
                }
                Ok(TrbType::ConfigureEndpoint) => {
                    let code = self.handle_configure_endpoint(trb.parameter, trb.slot_id());
                    self.post_command_completion(addr, code, trb.slot_id());
                }
                Ok(TrbType::EvaluateContext) => {
                    let code = self.handle_evaluate_context(trb.parameter, trb.slot_id());
                    self.post_command_completion(addr, code, trb.slot_id());
                }
                Ok(TrbType::ResetEndpoint) => {
                    // Clears the controller-side halt (§4.6.8): the first
                    // step of the required recovery order. A halted default
                    // control endpoint resumes here too (§4.10.2.4).
                    let dci = trb.endpoint_id();
                    if dci == 1 && trb.slot_id() == self.ep0_slot {
                        self.ep0_halted = false;
                    }
                    if dci == self.bulk_in.dci && self.bulk_in.halt == 1 {
                        self.bulk_in.halt = 2;
                    }
                    if dci == self.bulk_out.dci && self.bulk_out.halt == 1 {
                        self.bulk_out.halt = 2;
                    }
                    self.post_command_completion(addr, CompletionCode::Success, trb.slot_id());
                }
                Ok(TrbType::SetTrDequeuePointer) => {
                    // Repositions the endpoint's ring (§4.6.10): honoured
                    // only after a Reset Endpoint cleared the halt.
                    let dci = trb.endpoint_id();
                    let base = trb.parameter & !0xF;
                    let cycle = trb.parameter & 1 != 0;
                    if dci == 1 {
                        // The default control endpoint: the engine's EP0
                        // stall recovery rebuilds the ring at its base and
                        // repoints the dequeue; follow it like hardware so
                        // later control transfers stay in step.
                        let slot = trb.slot_id();
                        if slot == self.ep0_slot {
                            self.ep0_base = base;
                            self.ep0_index = 0;
                            self.ep0_cycle = cycle;
                        } else if usize::from(slot) < self.ep0_saved.len() {
                            self.ep0_saved[usize::from(slot)] = (base, 0, cycle);
                        }
                    }
                    if dci == self.bulk_in.dci {
                        self.bulk_in.base = base;
                        self.bulk_in.index = 0;
                        self.bulk_in.cycle = cycle;
                        if self.bulk_in.halt == 2 {
                            self.bulk_in.halt = 3;
                        }
                    }
                    if dci == self.bulk_out.dci {
                        self.bulk_out.base = base;
                        self.bulk_out.index = 0;
                        self.bulk_out.cycle = cycle;
                        if self.bulk_out.halt == 2 {
                            self.bulk_out.halt = 3;
                        }
                    }
                    self.post_command_completion(addr, CompletionCode::Success, trb.slot_id());
                }
                Ok(TrbType::NoOpCommand) => {
                    self.post_command_completion(addr, CompletionCode::Success, 0);
                }
                _ => {
                    self.post_command_completion(addr, CompletionCode::TrbError, 0);
                }
            }
        }
    }

    /// Read a transfer-ring dequeue pointer out of the endpoint context
    /// at `ctx_addr` (dwords 2/3, DCS masked off).
    fn ep_ctx_dequeue(&self, ctx_addr: u64) -> u64 {
        let dwords = self.read_dwords(ctx_addr, 4);
        ((u64::from(dwords[3]) << 32) | u64::from(dwords[2])) & !0xF
    }

    fn handle_address_device(&mut self, input_ctx: u64) -> CompletionCode {
        let control = self.read_dwords(input_ctx, 2);
        // Add flags must name the slot context and EP0 (A0 | A1).
        if control[1] & 0b11 != 0b11 {
            return CompletionCode::TrbError;
        }
        // Slot context (the context after the input control context):
        // dword 0 Route String (bits 0:19) + Speed (bits 20:23), dword 2
        // TT Hub Slot ID (bits 0:7) + TT Port Number (bits 8:15).
        let slot_ctx = self.read_dwords(input_ctx + MOCK_CTX_SIZE as u64, 3);
        let route_string = slot_ctx[0] & 0x000F_FFFF;
        let speed = (slot_ctx[0] >> 20) & 0xF;
        let tt_hub_slot = (slot_ctx[2] & 0xFF) as u8;
        let tt_port = ((slot_ctx[2] >> 8) & 0xFF) as u8;
        if route_string != 0 {
            // A device downstream of a hub: validate the Route String
            // and, for a full/low-speed device, the transaction-translator
            // coordinates the driver must program (xHCI §6.2.2 / §8.9). A
            // wrong topology faults Address Device, so the host test proves
            // the driver programmed them — the root hub occupies slot 1.
            let route_port = (route_string & 0xF) as u8;
            let nested_hub = if route_string <= 0xF {
                self.nested_by_root_port(route_port)
            } else {
                None
            };
            let is_nested_hub = nested_hub.is_some();
            let nested_child = self.nested_hubs.iter().position(|h| {
                h.downstream_port != 0
                    && route_string
                        == (u32::from(h.root_port) | (u32::from(h.downstream_port) << 4))
            });
            let is_nested_child = nested_child.is_some();
            let single_tier = route_string <= 0xF && !is_nested_hub;
            let scripted = is_nested_hub
                || is_nested_child
                || (single_tier
                    && (route_port == self.hub_downstream_port
                        || (self.msd_downstream_port != 0
                            && route_port == self.msd_downstream_port)
                        || (self.mouse_downstream_port != 0
                            && route_port == self.mouse_downstream_port)
                        || (self.composite_downstream_port != 0
                            && route_port == self.composite_downstream_port)));
            if !scripted {
                return CompletionCode::TrbError;
            }
            let needs_tt = speed == 1 || speed == 2;
            // A full/low-speed device splits through the transaction
            // translator of the nearest **high-speed** hub above it: the
            // nested hub for its own child, else the root hub (slot 1).
            let (want_hub, want_port) = if needs_tt {
                if let Some(i) = nested_child {
                    (
                        self.nested_hubs[i].slot,
                        self.nested_hubs[i].downstream_port,
                    )
                } else {
                    (self.root_hub_slot, route_port)
                }
            } else {
                (0, 0)
            };
            if tt_hub_slot != want_hub || tt_port != want_port {
                return CompletionCode::TrbError;
            }
            if let Some(i) = nested_hub {
                self.nested_hubs[i].slot = self.active_slot;
            }
            self.downstream_active = true;
            self.downstream_route = route_string;
            // The single-tier assertions read the low nibble; a nested
            // route is identified by the full route string instead.
            self.downstream_route_port = if single_tier { route_port } else { 0 };
        }
        if route_string == 0 {
            // The addressed device sits directly on a root port: the fixture
            // answering standard EP0 requests is the root-attached device
            // again (e.g. the hub assembly re-attached after a teardown),
            // not a previously addressed downstream device.
            self.downstream_active = false;
            self.downstream_route = 0;
            self.downstream_route_port = 0;
            self.addressed_root_port = ((slot_ctx[1] >> 16) & 0xFF) as u8;
            if self.hub_ports > 0 && self.addressed_root_port == 1 {
                // The root-attached hub now lives on this slot; its
                // downstream devices' TT coordinates must name it.
                self.root_hub_slot = self.active_slot;
            }
        }
        // Save the previously-live slot's EP0 ring progress before this slot
        // becomes the live control context, so switching back to it (e.g. the
        // hub after a downstream device is addressed) resumes where it left
        // off rather than re-reading consumed TRBs.
        let prev = usize::from(self.ep0_slot);
        if prev < self.ep0_saved.len() {
            self.ep0_saved[prev] = (self.ep0_base, self.ep0_index, self.ep0_cycle);
        }
        self.ep0_base = self.ep_ctx_dequeue(input_ctx + 2 * MOCK_CTX_SIZE as u64);
        self.ep0_index = 0;
        self.ep0_cycle = true;
        // Capture the EP0 Max Packet Size the driver programmed (§6.2.3
        // dword 1 bits 31:16): when it overstates the device's real
        // `bMaxPacketSize0`, descriptor reads deliver one device packet.
        let ep0_ctx = self.read_dwords(input_ctx + 2 * MOCK_CTX_SIZE as u64, 2);
        let s = usize::from(self.active_slot);
        if s < self.ep0_max.len() {
            self.ep0_max[s] = u16::try_from(ep0_ctx[1] >> 16).expect("16-bit field");
        }
        // This slot's EP0 ring becomes the live control context; record it so
        // a later doorbell for another slot can switch away and back.
        self.ep0_slot = self.active_slot;
        let s = usize::from(self.active_slot);
        if s < self.ep0_saved.len() {
            self.ep0_saved[s] = (self.ep0_base, 0, true);
        }
        self.addressed = true;
        CompletionCode::Success
    }

    /// Evaluate Context (xHCI §4.6.7): re-evaluate the EP0 Max Packet
    /// Size the input context carries. Only the A1 add flag is legal for
    /// the max-packet fix-up, and — unlike Address Device — the live EP0
    /// ring cursor is deliberately untouched: the controller evaluates
    /// just the named field (§6.2.3.3), never repositioning the ring.
    fn handle_evaluate_context(&mut self, input_ctx: u64, slot: u8) -> CompletionCode {
        let control = self.read_dwords(input_ctx, 2);
        if control[1] != 0b10 {
            return CompletionCode::TrbError;
        }
        let ep0_ctx = self.read_dwords(input_ctx + 2 * MOCK_CTX_SIZE as u64, 2);
        let s = usize::from(slot);
        if s >= self.ep0_max.len() {
            return CompletionCode::TrbError;
        }
        self.ep0_max[s] = u16::try_from(ep0_ctx[1] >> 16).expect("16-bit field");
        self.evaluate_context_count += 1;
        CompletionCode::Success
    }

    fn handle_configure_endpoint(&mut self, input_ctx: u64, slot: u8) -> CompletionCode {
        let control = self.read_dwords(input_ctx, 2);
        let add = control[1];
        // A Configure Endpoint that adds any endpoint (an A(dci) flag
        // beyond the slot-context A0) is the HID endpoint setup; one that
        // names only the slot context (A0 alone) is the hub-topology
        // update that marks the parent hub as a hub.
        let endpoint_adds = add & !0b1;
        // Two endpoint adds are the bulk endpoint pair (mass storage): read
        // each context's Endpoint Type field (§6.2.3 dword 1 bits 3:5) and
        // capture its ring; anything but one bulk-IN + one bulk-OUT is a
        // malformed configure.
        if endpoint_adds.count_ones() == 2 {
            if add & 0b1 == 0 {
                return CompletionCode::TrbError;
            }
            let mut bits = endpoint_adds;
            let mut in_seen = false;
            let mut out_seen = false;
            while bits != 0 {
                let dci = bits.trailing_zeros();
                bits &= bits - 1;
                let ep_ctx_off = input_ctx + (1 + u64::from(dci)) * MOCK_CTX_SIZE as u64;
                let ctx = self.read_dwords(ep_ctx_off, 4);
                let ep_type = (ctx[1] >> 3) & 0x7;
                let dequeue = self.ep_ctx_dequeue(ep_ctx_off);
                match ep_type {
                    // Bulk IN.
                    6 => {
                        self.bulk_in.dci = u8::try_from(dci).expect("DCI fits a byte");
                        self.bulk_in.base = dequeue;
                        self.bulk_in.index = 0;
                        self.bulk_in.cycle = true;
                        in_seen = true;
                    }
                    // Bulk OUT.
                    2 => {
                        self.bulk_out.dci = u8::try_from(dci).expect("DCI fits a byte");
                        self.bulk_out.base = dequeue;
                        self.bulk_out.index = 0;
                        self.bulk_out.cycle = true;
                        out_seen = true;
                    }
                    _ => return CompletionCode::TrbError,
                }
            }
            if !(in_seen && out_seen) {
                return CompletionCode::TrbError;
            }
            self.bulk_slot = slot;
            self.configured = true;
            return CompletionCode::Success;
        }
        if endpoint_adds != 0 {
            // A HID endpoint Configure Endpoint names the slot context
            // (A0) and exactly one endpoint (A(dci)). The DCI is read
            // from the add flags rather than assumed, so a keyboard whose
            // interrupt endpoint is not endpoint 1 is configured at its
            // real DCI (the metal no-report bug was hard-coding DCI 3).
            if add & 0b1 == 0 || endpoint_adds & (endpoint_adds - 1) != 0 {
                return CompletionCode::TrbError;
            }
            let dci = endpoint_adds.trailing_zeros();
            // An endpoint added to a slot already marked a hub is that hub's
            // interrupt-IN status-change endpoint, recorded separately so it
            // does not clobber a downstream device's interrupt endpoint state.
            if self.hub_marked_as_hub && slot == self.hub_slot_id {
                self.hub_int_dci = u8::try_from(dci).expect("DCI fits a byte");
                self.hub_int_base =
                    self.ep_ctx_dequeue(input_ctx + (1 + u64::from(dci)) * MOCK_CTX_SIZE as u64);
                self.hub_int_index = 0;
                self.hub_int_cycle = true;
                return CompletionCode::Success;
            }
            if self.capture_nested_hub_int_endpoint(input_ctx, dci, slot) {
                return CompletionCode::Success;
            }
            // The *second* HID interrupt endpoint is recorded in the second
            // endpoint model: the composite receiver's mouse interface
            // (configured after its keyboard interface on the **same**
            // slot), or a separate mouse device's endpoint (keyed by its
            // own downstream port and slot) — so two HID endpoints are
            // serviced concurrently and their completions carry the right
            // slot and DCI.
            let composite_second = self.composite_downstream_port != 0
                && self.downstream_route_port == self.composite_downstream_port
                && self.int_slot == slot;
            let mouse_device = self.mouse_downstream_port != 0
                && self.downstream_route_port == self.mouse_downstream_port;
            if composite_second || mouse_device {
                self.capture_second_int_endpoint(input_ctx, dci, slot);
                return CompletionCode::Success;
            }
            self.int_dci = u8::try_from(dci).expect("DCI fits a byte");
            let ep_ctx_off = input_ctx + (1 + u64::from(dci)) * MOCK_CTX_SIZE as u64;
            let int_ctx = self.read_dwords(ep_ctx_off, 5);
            // Max ESIT Payload Lo (§6.2.3.8 dword 4 bits 16:31): the
            // periodic scheduler reserves no bandwidth when it is zero.
            self.int_max_esit = (int_ctx[4] >> 16) & 0xFFFF;
            self.int_base = self.ep_ctx_dequeue(ep_ctx_off);
            self.int_index = 0;
            self.int_cycle = true;
            self.int_slot = slot;
            self.configured = true;
            return CompletionCode::Success;
        }
        // Hub-topology update (xHCI §6.2.2): the slot context add flag
        // must be set and its Hub bit (dword 0 bit 26) raised — the
        // controller would not route or split transactions to a
        // downstream device otherwise, which is the metal bug where a
        // keyboard behind the hub was addressed but never reported.
        if add & 0b1 == 0 {
            return CompletionCode::TrbError;
        }
        let slot_ctx = self.read_dwords(input_ctx + MOCK_CTX_SIZE as u64, 3);
        if slot_ctx[0] & (1 << 26) == 0 {
            return CompletionCode::TrbError;
        }
        // A nested hub is marked on its own slot; the root hub's marking
        // (and its captured context fields) stay intact beside it.
        if let Some(i) = self.nested_by_slot(slot) {
            self.nested_hubs[i].marked = true;
            return CompletionCode::Success;
        }
        self.hub_marked_as_hub = true;
        self.hub_slot_id = slot;
        self.hub_ctx_num_ports = ((slot_ctx[1] >> 24) & 0xFF) as u8;
        self.hub_ctx_tt_think_time = ((slot_ctx[2] >> 16) & 0b11) as u8;
        CompletionCode::Success
    }

    /// Record an interrupt-IN endpoint added to an already-marked nested
    /// hub's slot as that hub's status-change endpoint, returning `true`
    /// when `slot` named one — recorded separately so it never clobbers a
    /// downstream device's interrupt endpoint state.
    fn capture_nested_hub_int_endpoint(&mut self, input_ctx: u64, dci: u32, slot: u8) -> bool {
        let Some(i) = self.nested_by_slot(slot) else {
            return false;
        };
        if !self.nested_hubs[i].marked {
            return false;
        }
        let base = self.ep_ctx_dequeue(input_ctx + (1 + u64::from(dci)) * MOCK_CTX_SIZE as u64);
        let hub = &mut self.nested_hubs[i];
        hub.int.dci = u8::try_from(dci).expect("DCI fits a byte");
        hub.int.base = base;
        hub.int.index = 0;
        hub.int.cycle = true;
        hub.int.slot = slot;
        true
    }

    /// Record an interrupt-IN endpoint into the second endpoint model
    /// ([`Self::int2`]) at Configure Endpoint time — the shared capture for
    /// a composite receiver's second interface and for a separate mouse
    /// device's endpoint.
    fn capture_second_int_endpoint(&mut self, input_ctx: u64, dci: u32, slot: u8) {
        self.int2.dci = u8::try_from(dci).expect("DCI fits a byte");
        self.int2.base =
            self.ep_ctx_dequeue(input_ctx + (1 + u64::from(dci)) * MOCK_CTX_SIZE as u64);
        self.int2.index = 0;
        self.int2.cycle = true;
        self.int2.slot = slot;
        self.configured = true;
    }

    fn process_ep0_ring(&mut self) {
        loop {
            let (mut index, mut cycle) = (self.ep0_index, self.ep0_cycle);
            let base = self.ep0_base;
            let Some((addr, trb)) = self.next_owned(base, &mut index, &mut cycle) else {
                return;
            };
            self.ep0_index = index;
            self.ep0_cycle = cycle;
            match trb.trb_type() {
                Ok(TrbType::SetupStage) => {
                    self.pending_setup = Some(trb.parameter.to_le_bytes());
                }
                Ok(TrbType::DataStage) => {
                    self.pending_data = Some((
                        addr,
                        trb.parameter,
                        trb.status & 0x1_FFFF,
                        trb.control & trb::CONTROL_ISP != 0,
                    ));
                }
                Ok(TrbType::StatusStage) => self.execute_control(addr),
                _ => self.post_transfer_event(addr, CompletionCode::TrbError, 1, 0),
            }
        }
    }

    /// Write `source` into the assembled IN data stage and post a
    /// short-packet event when the device under-fills the TRB — the
    /// shared `GET_DESCRIPTOR` / `GET_STATUS` reply path. Returns
    /// `false` (after posting a `TrbError`) when no data stage was
    /// assembled.
    fn deliver_in_data(
        &mut self,
        data: Option<(u64, u64, u32, bool)>,
        source: &[u8],
        requested_len: usize,
        status_addr: u64,
    ) -> bool {
        let Some((data_addr, buffer, len, isp)) = data else {
            self.post_transfer_event(status_addr, CompletionCode::TrbError, 1, 0);
            return false;
        };
        let requested = usize::min(len as usize, requested_len);
        let supplied = usize::min(requested, source.len());
        self.write_mem(buffer, &source[..supplied]);
        let residual = len - u32::try_from(supplied).expect("reply fits");
        if residual > 0 && isp {
            self.post_transfer_event(data_addr, CompletionCode::ShortPacket, 1, residual);
        }
        true
    }

    /// The `GET_DESCRIPTOR(device | configuration)` fixture to answer with:
    /// the hub fixtures (class `0x09`) while the addressed device is the
    /// hub, the HID keyboard fixtures once a downstream device has been
    /// addressed (a non-zero Route String set `downstream_active`).
    fn descriptor_fixture(&self, desc_type: u8) -> &'static [u8] {
        // A nested hub answers with the hub fixtures too: it is a hub
        // one tier down, identified by its full route string.
        let is_nested_hub_addressed = self.downstream_active
            && self.downstream_route <= 0xF
            && self
                .nested_by_root_port((self.downstream_route & 0xF) as u8)
                .is_some();
        // The hub fixture sits on root port 1 (the Pi 4's onboard-hub
        // shape); a root device addressed on any other port is a plain
        // leaf, so a hub tier and a directly-attached device coexist.
        let is_hub_device =
            (self.hub_ports > 0 && !self.downstream_active && self.addressed_root_port == 1)
                || is_nested_hub_addressed;
        // The device kind is the *addressed* device's: the global flag for a
        // single-device fixture, or the per-port kind when a storage stick
        // is scripted beside the keyboard on its own downstream port.
        let is_msd = self.msd_device
            || (self.msd_downstream_port != 0
                && self.downstream_route_port == self.msd_downstream_port);
        let is_mouse = self.mouse_downstream_port != 0
            && self.downstream_route_port == self.mouse_downstream_port;
        let is_composite = self.composite_downstream_port != 0
            && self.downstream_route_port == self.composite_downstream_port;
        match (desc_type, is_hub_device) {
            (0x01, false) if is_msd => &MOCK_MSD_DESCRIPTOR,
            (0x01, false) if is_mouse => &MOCK_MOUSE_DESCRIPTOR,
            (0x01, false) if is_composite && self.forge_composite_ep0_max => {
                &MOCK_COMPOSITE_DESCRIPTOR_FORGED_EP0
            }
            (0x01, false) if is_composite => &MOCK_COMPOSITE_DESCRIPTOR,
            (0x01, false) if self.superspeed_hub => &MOCK_SS_DESCRIPTOR,
            (0x01, false) => &MOCK_DESCRIPTOR,
            (0x01, true) if self.superspeed_hub => &MOCK_SS_HUB_DESCRIPTOR,
            (0x01, true) => &MOCK_HUB_DESCRIPTOR,
            (_, false) if is_msd => &MOCK_MSD_CONFIG_DESCRIPTOR,
            (_, false) if is_mouse => &MOCK_MOUSE_CONFIG_DESCRIPTOR,
            (_, false) if is_composite => &MOCK_COMPOSITE_CONFIG_DESCRIPTOR,
            (_, false) => self.keyboard_config,
            (_, true) => &MOCK_HUB_CONFIG_DESCRIPTOR,
        }
    }

    /// The bytes a standard `GET_DESCRIPTOR` delivers: the addressed
    /// device's fixture, capped to one device-sized packet when the
    /// programmed EP0 Max Packet Size does not match the device's real
    /// `bMaxPacketSize0` — the controller sees the first undersized packet
    /// as a short packet and ends the TD (the metal fault of reading a
    /// full descriptor from a full-speed device with an 8-byte EP0 while
    /// the context still assumes 64).
    fn standard_descriptor_reply(&self, desc_type: u8) -> &'static [u8] {
        let source = self.descriptor_fixture(desc_type);
        let device = self.descriptor_fixture(0x01);
        // A `SuperSpeed` descriptor (bcdUSB >= 3.00) encodes EP0's fixed 512
        // as the exponent 9; the packet-size model compares in bytes.
        let device_ep0 = if device[3] >= 0x03 {
            1usize << device[7]
        } else {
            usize::from(device[7])
        };
        let programmed = usize::from(self.ep0_max[usize::from(self.ep0_slot)]);
        if programmed != device_ep0 && device_ep0 < source.len() {
            &source[..device_ep0]
        } else {
            source
        }
    }

    /// Answer a class `GET_DESCRIPTOR(hub)` (USB 2.0 §11.24.2.5 / USB 3.2
    /// §10.16.2.4): `bDescLength`, `bDescriptorType`, `bNbrPorts`, then a
    /// minimal tail. The reply is the addressed hub's — the nested hub
    /// reports its own port count when the request rides its EP0. A hub
    /// serves only its own protocol's descriptor type: a `SuperSpeed` hub
    /// STALLs a 0x29 request and a USB 2.0 hub STALLs a 0x2A one, exactly
    /// as real hardware refuses the foreign type. A garbled reply models
    /// the RTS5411 metal failure: the transfer completes successfully but
    /// the bytes are configuration-descriptor-shaped, not a hub
    /// descriptor; once the budget is spent the honest reply follows.
    /// Returns [`Self::deliver_in_data`]'s verdict.
    fn execute_get_hub_descriptor(
        &mut self,
        requested_type: u8,
        data: Option<(u64, u64, u32, bool)>,
        w_length: usize,
        status_addr: u64,
    ) -> bool {
        let own_type = if self.superspeed_hub { 0x2A } else { 0x29 };
        if requested_type != own_type {
            self.ep0_halted = true;
            self.post_transfer_event(status_addr, CompletionCode::StallError, 1, 0);
            return false;
        }
        if self.garble_hub_descriptor_replies > 0 {
            self.garble_hub_descriptor_replies -= 1;
            let stale = [0x09u8, 0x02, 0x29, 0x00, 0x01, 0x01, 0x00, 0xA0, 0x32];
            return self.deliver_in_data(data, &stale, w_length, status_addr);
        }
        let desc_type = if self.forge_hub_descriptor {
            0x00
        } else {
            own_type
        };
        let ports = match self.nested_by_slot(self.ep0_slot) {
            Some(i) => self.nested_hubs[i].ports,
            None => self.hub_ports,
        };
        let hub_desc = if self.superspeed_hub {
            // The fixed 12-byte SS hub descriptor (USB 3.2 §10.15.2.1).
            [12u8, desc_type, ports, 0x00, 0x00, 0x32, 0x00, 0xFF]
        } else {
            [9u8, desc_type, ports, 0x00, 0x00, 0x32, 0x00, 0xFF]
        };
        self.deliver_in_data(data, &hub_desc, w_length, status_addr)
    }

    /// Execute the assembled control TD, posting its transfer events.
    fn execute_control(&mut self, status_addr: u64) {
        let Some(setup) = self.pending_setup.take() else {
            self.post_transfer_event(status_addr, CompletionCode::TrbError, 1, 0);
            return;
        };
        // A halted EP0 runs no further transfers until reset (xHCI
        // §4.10.2.4); model that as a transaction error rather than a
        // valid completion.
        if self.ep0_halted {
            self.pending_data.take();
            self.post_transfer_event(status_addr, CompletionCode::UsbTransactionError, 1, 0);
            return;
        }
        let data = self.pending_data.take();
        let w_length = usize::from(u16::from_le_bytes([setup[6], setup[7]]));
        match (setup[0], setup[1]) {
            // GET_DESCRIPTOR(device | configuration); a hub answers with
            // the hub fixtures (class 0x09), a keyboard with the HID ones.
            (0x80, 0x06) if setup[3] == 0x01 || setup[3] == 0x02 => {
                let source = self.standard_descriptor_reply(setup[3]);
                if !self.deliver_in_data(data, source, w_length, status_addr) {
                    return;
                }
            }
            // Class GET_DESCRIPTOR(hub) (USB 2.0 §11.24.2.5 / USB 3.2
            // §10.16.2.4) — the hub serves only its own protocol's type.
            (0xA0, 0x06) if setup[3] == 0x29 || setup[3] == 0x2A => {
                if !self.execute_get_hub_descriptor(setup[3], data, w_length, status_addr) {
                    return;
                }
            }
            // Hub class SET_HUB_DEPTH (USB 3.2 §10.16.2.7): defined only
            // for a `SuperSpeed` hub — a USB 2.0 hub STALLs it (the default
            // arm below). Records the depth so a test pins it was set.
            (0x20, 0x0C) if self.superspeed_hub => {
                self.hub_depth_set = Some(setup[2]);
            }
            // Class SET_FEATURE on a downstream port (USB 2.0 §11.24.2.13),
            // served from the addressed hub's bank: the nested hub's when
            // the request rides its EP0, else the root hub's.
            (0x23, 0x03) => self.execute_set_port_feature(setup[2], setup[4]),
            // Class GET_STATUS on a downstream port (USB 2.0 §11.24.2.7):
            // the connected downstream port reports its status once
            // powered, every other port reads disconnected. A scripted
            // fault answers with its one fault event and nothing else — a
            // real controller posts a single event for a faulted transfer,
            // never a trailing status success.
            (0xA3, 0x00) => {
                let faulted = self.fault_hub_port_status
                    || self.fault_hub_port_status_raw != 0
                    || self.fault_hub_port_status_evtype != 0;
                self.execute_get_port_status(setup[4], data, w_length, status_addr);
                if faulted {
                    return;
                }
            }
            // Class CLEAR_FEATURE on a downstream port (USB 2.0 §11.24.2.2):
            // clear *only* the latched change the feature selector names
            // (C_PORT_CONNECTION=16 .. C_PORT_RESET=20 → wPortChange bits 0..4),
            // mirroring real hardware. A driver that clears only the connect
            // change leaves the reset change (bit 4) latched and the port
            // permanently flagged, so the watch keeps re-firing.
            (0x23, 0x01) => self.execute_clear_port_feature(setup[2], setup[4]),
            // Standard CLEAR_FEATURE(ENDPOINT_HALT) on an endpoint (USB 2.0
            // §9.4.1): the device-side half of a bulk halt recovery.
            (0x02, 0x01) if setup[2] == 0x00 => {
                // The request must reach the *device's* own control
                // endpoint. One wrongly issued to the hub's EP0 (the resting
                // active control context) is a mistargeted recovery: the hub
                // has no such endpoint, so it STALLs — loudly, exactly as
                // real hardware would.
                if self.hub_slot_id != 0 && self.ep0_slot == self.hub_slot_id {
                    self.ep0_halted = true;
                    self.post_transfer_event(status_addr, CompletionCode::StallError, 1, 0);
                    return;
                }
                let number = setup[4] & 0x0F;
                let dci = if setup[4] & 0x80 != 0 {
                    number * 2 + 1
                } else {
                    number * 2
                };
                // The device-side clear completes the recovery only after
                // the controller-side Reset Endpoint + Set TR Dequeue
                // Pointer ran, mirroring the order the silicon requires.
                if dci == self.bulk_in.dci && self.bulk_in.halt == 3 {
                    self.bulk_in.halt = 0;
                }
                if dci == self.bulk_out.dci && self.bulk_out.halt == 3 {
                    self.bulk_out.halt = 0;
                }
            }
            // SET_CONFIGURATION
            (0x00, 0x09) => self.configuration = Some(setup[2]),
            // Class ADSC (a control-OUT data stage — the CBI command
            // channel): capture the delivered command block.
            (0x21, 0x00) => {
                let Some((_, buffer, len, _)) = data else {
                    self.post_transfer_event(status_addr, CompletionCode::TrbError, 1, 0);
                    return;
                };
                let block = self.read_mem(buffer, len as usize);
                self.adsc_blocks.push(block);
            }
            // SET_PROTOCOL (HID class)
            (0x21, 0x0B) => {
                if self.fault_class_requests {
                    self.post_transfer_event(
                        status_addr,
                        CompletionCode::UsbTransactionError,
                        1,
                        0,
                    );
                    return;
                }
                // A hub is not a HID device, so it STALLs this HID class
                // request — and a STALL halts EP0, exactly the metal
                // failure that breaks a following hub-descriptor read. The
                // downstream device *is* a HID keyboard, so once it is
                // addressed the request succeeds.
                if self.stall_class_requests || (self.hub_ports > 0 && !self.downstream_active) {
                    self.ep0_halted = true;
                    self.post_transfer_event(status_addr, CompletionCode::StallError, 1, 0);
                    return;
                }
                self.protocol = Some(setup[2]);
            }
            _ => {
                self.ep0_halted = true;
                self.post_transfer_event(status_addr, CompletionCode::StallError, 1, 0);
                return;
            }
        }
        self.post_transfer_event(status_addr, CompletionCode::Success, 1, 0);
    }

    fn process_int_ring(&mut self) {
        // A device addressed downstream of the hub receives interrupt
        // transfers only once the controller has been told its parent is
        // a hub (the Hub bit in the hub's slot context, set by a
        // Configure Endpoint). Real hardware never schedules the split
        // transactions otherwise, so the mock delivers no report — the
        // metal bug where the keyboard was addressed but never typed.
        if self.downstream_active && !self.hub_marked_as_hub {
            return;
        }
        // The periodic scheduler reserves no bandwidth for an interrupt
        // endpoint whose Max ESIT Payload is zero (§4.14.2), so the
        // controller services it never and the device delivers no report
        // — the metal bug where the addressed keyboard never typed. A
        // configured interrupt endpoint always carries a non-zero payload
        // once `ep_ctx_dwords` programs it.
        if self.configured && self.int_max_esit == 0 {
            return;
        }
        while let Some(report) = self.pending_reports.front().cloned() {
            let (mut index, mut cycle) = (self.int_index, self.int_cycle);
            let base = self.int_base;
            let Some((addr, trb)) = self.next_owned(base, &mut index, &mut cycle) else {
                return;
            };
            if trb.trb_type() != Ok(TrbType::Normal) {
                return;
            }
            self.int_index = index;
            self.int_cycle = cycle;
            self.pending_reports.pop_front();
            self.write_mem(trb.parameter, &report);
            let residual = if self.forge_report_residual {
                trb.status + 1
            } else {
                trb.status - u32::try_from(report.len()).expect("report fits")
            };
            let code = if let Some(bad) = self.fault_one_report_completion.take() {
                // A single odd completion the driver rejects per-report;
                // consumed once so the following report is normal.
                bad
            } else if residual > 0 {
                CompletionCode::ShortPacket
            } else {
                CompletionCode::Success
            };
            self.post_transfer_event(addr, code, self.int_dci, residual);
        }
    }

    /// Consume the second HID endpoint's queued interrupt TDs, answering
    /// each from [`Self::pending_reports2`] — [`Self::process_int_ring`] for
    /// the mouse beside the keyboard. Completions are posted with the
    /// endpoint's own slot ([`Self::int2_slot`]), so the engine's
    /// per-device slot+DCI demux is exercised.
    fn process_int2_ring(&mut self) {
        if self.int2.slot == 0 {
            return;
        }
        if self.downstream_active && !self.hub_marked_as_hub {
            return;
        }
        while let Some(report) = self.pending_reports2.front().cloned() {
            let (mut index, mut cycle) = (self.int2.index, self.int2.cycle);
            let base = self.int2.base;
            let Some((addr, trb)) = self.next_owned(base, &mut index, &mut cycle) else {
                return;
            };
            if trb.trb_type() != Ok(TrbType::Normal) {
                return;
            }
            self.int2.index = index;
            self.int2.cycle = cycle;
            self.pending_reports2.pop_front();
            self.write_mem(trb.parameter, &report);
            let residual = trb.status - u32::try_from(report.len()).expect("report fits");
            let code = if residual > 0 {
                CompletionCode::ShortPacket
            } else {
                CompletionCode::Success
            };
            self.post_transfer_event_for_slot(addr, code, self.int2.dci, residual, self.int2.slot);
        }
    }

    /// Consume queued bulk-IN TDs, answering each from the scripted
    /// [`Self::bulk_in_responses`] (one response per TD; a TD with no queued
    /// response stays pending). A halted endpoint is not serviced; the
    /// one-shot stall knob halts it and posts a `StallError` for the TD it
    /// consumed.
    fn process_bulk_in_ring(&mut self) {
        loop {
            if self.bulk_in.halt != 0 {
                return;
            }
            let (mut index, mut cycle) = (self.bulk_in.index, self.bulk_in.cycle);
            let base = self.bulk_in.base;
            let Some((addr, trb)) = self.next_owned(base, &mut index, &mut cycle) else {
                return;
            };
            if trb.trb_type() != Ok(TrbType::Normal) {
                return;
            }
            let len = trb.status & 0x1_FFFF;
            if self.bulk_in.stall_next {
                self.bulk_in.stall_next = false;
                self.bulk_in.halt = 1;
                self.bulk_in.index = index;
                self.bulk_in.cycle = cycle;
                self.post_transfer_event(addr, CompletionCode::StallError, self.bulk_in.dci, len);
                return;
            }
            let Some(response) = self.bulk_in_responses.pop_front() else {
                return;
            };
            self.bulk_in.index = index;
            self.bulk_in.cycle = cycle;
            let supplied = usize::min(response.len(), len as usize);
            self.write_mem(trb.parameter, &response[..supplied]);
            let residual = len - u32::try_from(supplied).expect("response fits");
            let code = if residual > 0 {
                CompletionCode::ShortPacket
            } else {
                CompletionCode::Success
            };
            self.post_transfer_event(addr, code, self.bulk_in.dci, residual);
        }
    }

    /// Consume queued bulk-OUT TDs, capturing each TD's bytes in
    /// [`Self::bulk_out_received`]. As [`Self::process_bulk_in_ring`] for
    /// the halt/stall behaviour.
    fn process_bulk_out_ring(&mut self) {
        loop {
            if self.bulk_out.halt != 0 {
                return;
            }
            let (mut index, mut cycle) = (self.bulk_out.index, self.bulk_out.cycle);
            let base = self.bulk_out.base;
            let Some((addr, trb)) = self.next_owned(base, &mut index, &mut cycle) else {
                return;
            };
            if trb.trb_type() != Ok(TrbType::Normal) {
                return;
            }
            self.bulk_out.index = index;
            self.bulk_out.cycle = cycle;
            let len = trb.status & 0x1_FFFF;
            if self.bulk_out.stall_next {
                self.bulk_out.stall_next = false;
                self.bulk_out.halt = 1;
                self.post_transfer_event(addr, CompletionCode::StallError, self.bulk_out.dci, len);
                return;
            }
            let bytes = self.read_mem(trb.parameter, len as usize);
            self.bulk_out_received.push(bytes);
            self.post_transfer_event(addr, CompletionCode::Success, self.bulk_out.dci, 0);
        }
    }

    /// Deliver one hub status-change report: write `bitmap` (the port-change
    /// bitmap, USB 2.0 §11.12.4) into the armed status-change transfer's
    /// buffer and post its completion on the hub slot's status-change
    /// endpoint, so the engine's `next_hub_change` wakes and services it.
    ///
    /// Mirrors [`Self::process_int_ring`] for the hub's interrupt-IN
    /// status-change endpoint; the event carries the hub's slot id and DCI so
    /// the engine routes it as a hub completion, never a keyboard report.
    fn post_hub_status_change(&mut self, bitmap: &[u8]) {
        let (mut index, mut cycle) = (self.hub_int_index, self.hub_int_cycle);
        let base = self.hub_int_base;
        let Some((addr, trb)) = self.next_owned(base, &mut index, &mut cycle) else {
            return;
        };
        if trb.trb_type() != Ok(TrbType::Normal) {
            return;
        }
        self.hub_int_index = index;
        self.hub_int_cycle = cycle;
        self.write_mem(trb.parameter, bitmap);
        let residual = trb.status - u32::try_from(bitmap.len()).expect("bitmap fits");
        let code = if residual > 0 {
            CompletionCode::ShortPacket
        } else {
            CompletionCode::Success
        };
        self.post_event(Trb {
            parameter: addr,
            status: (u32::from(code.as_u8()) << 24) | residual,
            control: (u32::from(TrbType::TransferEvent.as_u8()) << 10)
                | (u32::from(self.hub_int_dci) << 16)
                | trb::control_slot(self.hub_slot_id),
        });
    }

    /// Execute a class `SET_FEATURE` on downstream hub `port` (USB 2.0
    /// §11.24.2.13), served from the addressed hub's bank: `PORT_POWER`
    /// (8) marks the port powered; `PORT_RESET` (4) marks it reset and —
    /// like real hardware — latches the Reset-change bit (wPortChange bit
    /// 4) so the driver must clear it as well as the connect change or the
    /// port stays flagged forever.
    fn execute_set_port_feature(&mut self, feature: u8, port: u8) {
        if port < 1 {
            return;
        }
        let bit = 1 << (u32::from(port) - 1);
        if let Some(i) = self.nested_by_slot(self.ep0_slot) {
            let hub = &mut self.nested_hubs[i];
            match feature {
                8 => hub.powered |= bit,
                4 => {
                    hub.reset |= bit;
                    if port == hub.downstream_port {
                        hub.downstream_change |= 1 << 4;
                    }
                }
                _ => {}
            }
            return;
        }
        match feature {
            8 => self.hub_powered |= bit,
            4 => {
                self.hub_reset |= bit;
                if port == self.hub_downstream_port {
                    self.hub_downstream_change |= 1 << 4;
                }
                if let Some(i) = self.nested_by_root_port(port) {
                    self.nested_hubs[i].root_change |= 1 << 4;
                }
            }
            _ => {}
        }
    }

    /// Execute a class `CLEAR_FEATURE` on downstream hub `port` (USB 2.0
    /// §11.24.2.2): clear *only* the latched change the feature selector
    /// names (`C_PORT_CONNECTION`=16 .. `C_PORT_RESET`=20 → wPortChange
    /// bits 0..4), in the addressed hub's bank, mirroring real hardware. A
    /// driver that clears only the connect change leaves the reset change
    /// (bit 4) latched and the port permanently flagged, so the watch
    /// keeps re-firing.
    fn execute_clear_port_feature(&mut self, feature: u8, port: u8) {
        if !(16..=20).contains(&feature) {
            return;
        }
        let bit = 1u16 << (feature - 16);
        if let Some(i) = self.nested_by_slot(self.ep0_slot) {
            let hub = &mut self.nested_hubs[i];
            if port == hub.downstream_port {
                hub.downstream_change &= !bit;
            }
        } else if let Some(i) = self.nested_by_root_port(port) {
            self.nested_hubs[i].root_change &= !bit;
        } else {
            self.hub_downstream_change &= !bit;
        }
    }

    /// Deliver one status-change report from the **nested** hub carried on
    /// root-hub port `root_port`, as [`Self::post_hub_status_change`] does
    /// for the root hub: write `bitmap` into that hub's armed status-change
    /// transfer and post its completion with that hub's slot and DCI.
    fn post_nested_hub_status_change(&mut self, root_port: u8, bitmap: &[u8]) {
        let Some(i) = self.nested_by_root_port(root_port) else {
            return;
        };
        let (mut index, mut cycle) = (self.nested_hubs[i].int.index, self.nested_hubs[i].int.cycle);
        let base = self.nested_hubs[i].int.base;
        let Some((addr, trb)) = self.next_owned(base, &mut index, &mut cycle) else {
            return;
        };
        if trb.trb_type() != Ok(TrbType::Normal) {
            return;
        }
        self.nested_hubs[i].int.index = index;
        self.nested_hubs[i].int.cycle = cycle;
        self.write_mem(trb.parameter, bitmap);
        let residual = trb.status - u32::try_from(bitmap.len()).expect("bitmap fits");
        let code = if residual > 0 {
            CompletionCode::ShortPacket
        } else {
            CompletionCode::Success
        };
        self.post_event(Trb {
            parameter: addr,
            status: (u32::from(code.as_u8()) << 24) | residual,
            control: (u32::from(TrbType::TransferEvent.as_u8()) << 10)
                | (u32::from(self.nested_hubs[i].int.dci) << 16)
                | trb::control_slot(self.nested_hubs[i].slot),
        });
    }

    /// Execute a class `GET_STATUS` on downstream hub `port` (USB 2.0
    /// §11.24.2.7): honour the fault knobs, then reply with the port's
    /// `wPortStatus` (connect/speed once powered, plus enabled once reset) and
    /// its latched `wPortChange`. Served from the addressed hub's bank
    /// (nested vs root), keyed by the EP0 slot the request rode.
    fn execute_get_port_status(
        &mut self,
        port: u8,
        data: Option<(u64, u64, u32, bool)>,
        w_length: usize,
        status_addr: u64,
    ) {
        if self.fault_hub_port_status {
            self.post_transfer_event(status_addr, CompletionCode::StallError, 1, 0);
            return;
        }
        if self.fault_hub_port_status_raw != 0 {
            self.post_transfer_event_raw(status_addr, self.fault_hub_port_status_raw, 1, 0);
            return;
        }
        if self.fault_hub_port_status_evtype != 0 {
            self.post_event_raw_type(status_addr, self.fault_hub_port_status_evtype);
            return;
        }
        let bit = if port >= 1 {
            1 << (u32::from(port) - 1)
        } else {
            0
        };
        // The addressed hub's bank: a nested hub's ports when the request
        // rides its EP0, else the root hub's.
        let (w_status, change) = if let Some(i) = self.nested_by_slot(self.ep0_slot) {
            let hub = &self.nested_hubs[i];
            let powered = port >= 1 && hub.powered & bit != 0;
            let w_status = if powered && port == hub.downstream_port {
                let enabled = if hub.reset & bit != 0 { 1 << 1 } else { 0 };
                hub.downstream_status | enabled
            } else {
                0
            };
            let change = if port == hub.downstream_port {
                hub.downstream_change
            } else {
                0
            };
            (w_status, change)
        } else if let Some(i) = self.nested_by_root_port(port) {
            // The root-hub port carrying a nested hub itself: connected
            // high-speed while present, with its own latched changes.
            let hub = &self.nested_hubs[i];
            let powered = self.hub_powered & bit != 0;
            let w_status = if powered && hub.connected {
                let enabled = if self.hub_reset & bit != 0 { 1 << 1 } else { 0 };
                (1 << 0) | (1 << 10) | enabled
            } else {
                0
            };
            (w_status, hub.root_change)
        } else {
            let powered = port >= 1 && self.hub_powered & bit != 0;
            let is_device_port = port == self.hub_downstream_port
                || (self.msd_downstream_port != 0 && port == self.msd_downstream_port)
                || (self.mouse_downstream_port != 0 && port == self.mouse_downstream_port)
                || (self.composite_downstream_port != 0 && port == self.composite_downstream_port);
            let w_status = if powered && is_device_port {
                // A slow hub keeps reporting the reset in progress for the
                // scripted number of reads before the port enables, so the
                // engine's reset-completion poll is exercised.
                if self.slow_enable_status_reads > 0
                    && self.hub_reset & bit != 0
                    && port == self.hub_downstream_port
                {
                    self.slow_enable_status_reads -= 1;
                    self.hub_downstream_status | (1 << 4)
                } else {
                    // Once the port has been reset it reports enabled
                    // (PORT_STATUS_ENABLE, bit 1) in addition to its connect/speed
                    // bits — unless the port's device is scripted to never enable
                    // (a broken or half-seated device).
                    let enabled =
                        if self.hub_reset & bit != 0 && port != self.fail_enable_downstream_port {
                            1 << 1
                        } else {
                            0
                        };
                    self.hub_downstream_status | enabled
                }
            } else {
                0
            };
            // The latched `wPortChange` (e.g. Connect Status Change) is
            // reported for the watched downstream port, so the hub-hotplug
            // path can confirm and clear it.
            let change = if port == self.hub_downstream_port {
                self.hub_downstream_change
            } else {
                0
            };
            (w_status, change)
        };
        let status_bytes = w_status.to_le_bytes();
        let change_bytes = change.to_le_bytes();
        let reply = [
            status_bytes[0],
            status_bytes[1],
            change_bytes[0],
            change_bytes[1],
        ];
        self.deliver_in_data(data, &reply, w_length, status_addr);
    }

    /// Reset the device-model ring consumer positions and per-slot state, as a
    /// Host Controller Reset does on real hardware (xHCI §4.2): every slot,
    /// ring dequeue position, and addressed/configured state is cleared, so a
    /// re-bring-up re-programs the rings and re-enumerates from scratch rather
    /// than reading a ring from a stale dequeue position.
    fn reset_device_model(&mut self) {
        self.cmd_index = 0;
        self.cmd_cycle = true;
        self.ep0_index = 0;
        self.ep0_cycle = true;
        self.ep0_slot = 0;
        self.ep0_saved = [(0, 0, true); 33];
        self.ep0_max = [0; 33];
        self.int_index = 0;
        self.int_cycle = true;
        self.event_index = 0;
        self.event_cycle = true;
        self.next_slot = 1;
        self.active_slot = 0;
        self.addressed = false;
        self.configured = false;
        self.downstream_active = false;
        self.downstream_route_port = 0;
        self.hub_marked_as_hub = false;
        self.hub_slot_id = 0;
        self.hub_int_base = 0;
        self.hub_int_dci = 0;
        self.hub_reset = 0;
        self.hub_powered = 0;
        self.bulk_in = MockBulk::new();
        self.bulk_out = MockBulk::new();
        self.int_slot = 0;
        self.bulk_slot = 0;
        self.int2 = MockInt::new();
        self.downstream_route = 0;
        for hub in &mut self.nested_hubs {
            hub.slot = 0;
            hub.powered = 0;
            hub.reset = 0;
            hub.marked = false;
            hub.int = MockInt::new();
        }
    }
}

impl XhciHost for MockXhci {
    fn read32(&mut self, offset: usize) -> Result<u32, DriverError> {
        if offset >= MOCK_WINDOW_LEN {
            return Err(DriverError::DeviceFault);
        }
        if offset == regs::CAPLENGTH_HCIVERSION {
            return Ok(self.cap_dword0);
        }
        if offset == regs::HCSPARAMS1 {
            return Ok(self.hcsparams1);
        }
        if offset == regs::HCSPARAMS2 {
            return Ok(self.hcsparams2);
        }
        if offset == regs::HCCPARAMS1 {
            return Ok(self.hccparams1);
        }
        if offset == Self::op(regs::PAGESIZE) {
            return Ok(self.pagesize);
        }
        if offset == regs::DBOFF {
            return Ok(self.dboff);
        }
        if offset == regs::RTSOFF {
            return Ok(self.rtsoff);
        }
        if offset == Self::op(regs::USBCMD) {
            if self.hcrst_reads > 0 && !self.hcrst_stuck {
                self.hcrst_reads -= 1;
                if self.hcrst_reads == 0 {
                    self.usbcmd &= !regs::USBCMD_HCRST;
                }
            }
            return Ok(self.usbcmd);
        }
        if offset == Self::op(regs::USBSTS) {
            if self.pending_status_clear & regs::USBSTS_HSE != 0 {
                self.hse_latched = false;
            }
            if self.pending_status_clear & regs::USBSTS_EINT != 0 {
                self.eint_latched = false;
            }
            if self.pending_status_clear & regs::USBSTS_PCD != 0 {
                self.pcd_latched = false;
            }
            self.pending_status_clear = 0;
            let mut status = 0;
            if self.cnr_stuck || self.cnr_reads > 0 {
                self.cnr_reads = self.cnr_reads.saturating_sub(1);
                status |= regs::USBSTS_CNR;
            }
            if self.usbcmd & regs::USBCMD_RUN == 0 {
                status |= regs::USBSTS_HCH;
            }
            if self.hse_latched {
                status |= regs::USBSTS_HSE;
            }
            if self.eint_latched {
                status |= regs::USBSTS_EINT;
            }
            if self.pcd_latched {
                status |= regs::USBSTS_PCD;
            }
            return Ok(status);
        }
        if offset == Self::op(regs::CONFIG) {
            return Ok(self.config);
        }
        if offset == Self::ir0(regs::IR_IMAN) {
            return Ok(self.iman);
        }
        if offset == Self::ir0(regs::IR_IMOD) {
            return Ok(self.imod);
        }
        if offset == Self::ir0(regs::IR_ERSTSZ) {
            return Ok(self.erstsz);
        }
        if offset == Self::ir0(regs::IR_ERDP) {
            return Ok(self.erdp[0]);
        }
        if offset == Self::ir0(regs::IR_ERDP) + 4 {
            return Ok(self.erdp[1]);
        }
        let portsc_base = Self::op(regs::PORTSC_BASE);
        for port in 0..self.portsc.len() {
            if offset == portsc_base + port * regs::PORTSC_STRIDE {
                if port == self.port_reset_port && self.port_reset_reads > 0 {
                    self.port_reset_reads -= 1;
                    if self.port_reset_reads == 0 {
                        self.portsc[port] &= !regs::PORTSC_PR;
                    }
                }
                return Ok(self.portsc[port]);
            }
        }
        Ok(0)
    }

    fn write32(&mut self, offset: usize, value: u32) -> Result<(), DriverError> {
        if offset >= MOCK_WINDOW_LEN {
            return Err(DriverError::DeviceFault);
        }
        if offset == Self::op(regs::USBCMD) {
            self.usbcmd = value;
            if value & regs::USBCMD_HCRST != 0 {
                // A real reset clears the operational state and the
                // self-clearing bit a few reads later.
                self.hcrst_reads = 3;
                self.hcrst_stuck |= self.hse_latched || self.pcd_latched;
                self.cnr_reads = 0;
                self.reset_device_model();
            }
            return Ok(());
        }
        if offset == Self::op(regs::USBSTS) {
            let clear = value & (regs::USBSTS_HSE | regs::USBSTS_EINT | regs::USBSTS_PCD);
            if self.status_write_needs_read_flush {
                self.pending_status_clear |= clear;
            } else {
                if clear & regs::USBSTS_HSE != 0 {
                    self.hse_latched = false;
                }
                if clear & regs::USBSTS_EINT != 0 {
                    self.eint_latched = false;
                }
                if clear & regs::USBSTS_PCD != 0 {
                    self.pcd_latched = false;
                }
            }
            return Ok(());
        }
        if offset == Self::op(regs::CONFIG) {
            self.config = value;
            return Ok(());
        }
        if offset == Self::op(regs::DCBAAP) {
            self.dcbaap[0] = value;
            return Ok(());
        }
        if offset == Self::op(regs::DCBAAP) + 4 {
            self.dcbaap[1] = value;
            return Ok(());
        }
        if offset == Self::op(regs::CRCR) {
            self.crcr[0] = value;
            return Ok(());
        }
        if offset == Self::op(regs::CRCR) + 4 {
            self.crcr[1] = value;
            return Ok(());
        }
        if self.write_interrupter(offset, value) {
            return Ok(());
        }
        let portsc_base = Self::op(regs::PORTSC_BASE);
        for port in 0..self.portsc.len() {
            if offset == portsc_base + port * regs::PORTSC_STRIDE {
                if value & regs::PORTSC_PP != 0 {
                    // Port Power latches sticky, as on a controller whose
                    // ports software powers on (xHCI 1.2 §5.4.8).
                    self.portsc[port] |= regs::PORTSC_PP;
                    // A port-power-controlled controller (PPC = 1) only
                    // reports a device once the port is powered: a latent
                    // device asserts Current Connect Status here.
                    if self.latent_device_port == Some(port) {
                        self.portsc[port] |= regs::PORTSC_CCS | (3 << regs::PORTSC_SPEED_SHIFT);
                    }
                }
                if value & regs::PORTSC_PR != 0 {
                    // A reset re-enables a connected port; PR reads as
                    // in-progress for a couple of polls.
                    self.portsc[port] |= regs::PORTSC_PED | regs::PORTSC_PR;
                    self.port_reset_reads = 2;
                    self.port_reset_port = port;
                }
                if value & regs::PORTSC_CSC != 0 {
                    // Connect Status Change is write-1-to-clear (xHCI 1.2
                    // §5.4.8): the root-port scan consumes the latch.
                    self.portsc[port] &= !regs::PORTSC_CSC;
                }
                return Ok(());
            }
        }
        let db_base = MOCK_DBOFF as usize;
        if offset >= db_base && offset < db_base + 256 * 4 {
            self.doorbells.push((offset - db_base, value));
            if self.mem.is_some() && self.usbcmd & regs::USBCMD_RUN != 0 {
                self.ring_doorbell_model((offset - db_base) / 4, value);
            }
            return Ok(());
        }
        Ok(())
    }
}

impl MockXhci {
    /// Service a doorbell write at slot `index` with target `value` (the DCI,
    /// or `0` for the command ring), driving the matching ring's device model.
    fn ring_doorbell_model(&mut self, index: usize, value: u32) {
        match (index, value) {
            (0, 0) => self.process_command_ring(),
            (_, 1) => {
                // Switch the live EP0 ring to the rung slot's, like the
                // DCBAA-indexed hardware: save the current slot's ring state
                // and load the rung slot's.
                if index < self.ep0_saved.len() && u8::try_from(index) != Ok(self.ep0_slot) {
                    let cur = usize::from(self.ep0_slot);
                    if cur < self.ep0_saved.len() {
                        self.ep0_saved[cur] = (self.ep0_base, self.ep0_index, self.ep0_cycle);
                    }
                    let (base, idx, cycle) = self.ep0_saved[index];
                    self.ep0_base = base;
                    self.ep0_index = idx;
                    self.ep0_cycle = cycle;
                    self.ep0_slot = u8::try_from(index).unwrap_or(0);
                }
                self.process_ep0_ring();
            }
            (_, value) if self.bulk_in.dci != 0 && value == u32::from(self.bulk_in.dci) => {
                self.process_bulk_in_ring();
            }
            (_, value) if self.bulk_out.dci != 0 && value == u32::from(self.bulk_out.dci) => {
                self.process_bulk_out_ring();
            }
            (index, value)
                if self.int2.slot != 0
                    && index == usize::from(self.int2.slot)
                    && value == u32::from(self.int2.dci) =>
            {
                self.process_int2_ring();
            }
            (_, 3) => self.process_int_ring(),
            _ => {}
        }
    }
}

#[test]
fn open_parses_capability_block() {
    let xhci = Xhci::open(MockXhci::new()).expect("bring-up succeeds");
    assert_eq!(xhci.hci_version(), 0x0110);
    assert_eq!(xhci.max_slots(), 32);
    assert_eq!(xhci.max_ports(), 4);
    assert!(xhci.ac64());
    assert!(xhci.csz());
    assert_eq!(xhci.runtime_base(), MOCK_RTSOFF as usize);
}

#[test]
fn open_waits_for_controller_ready() {
    let mut mock = MockXhci::new();
    mock.cnr_reads = 5;
    assert!(Xhci::open(mock).is_ok());
}

#[test]
fn open_resets_a_halted_controller_with_pre_reset_cnr_and_hse() {
    let mut mock = MockXhci::new();
    mock.cnr_reads = 128;
    mock.hse_latched = true;
    mock.pcd_latched = true;
    let mut xhci = Xhci::open_with_budget(mock, 16).expect("reset clears stale pre-reset status");

    let status = xhci.host.read32(MockXhci::op(regs::USBSTS)).unwrap();
    assert_eq!(
        status & (regs::USBSTS_CNR | regs::USBSTS_HSE | regs::USBSTS_PCD),
        0
    );
}

#[test]
fn open_flushes_pre_reset_status_clear_before_hcrst() {
    let mut mock = MockXhci::new();
    mock.hse_latched = true;
    mock.pcd_latched = true;
    mock.status_write_needs_read_flush = true;

    let mut xhci = Xhci::open_with_budget(mock, 16).expect("status clear is flushed before reset");
    let usbcmd = xhci.host.read32(MockXhci::op(regs::USBCMD)).unwrap();
    let usbsts = xhci.host.read32(MockXhci::op(regs::USBSTS)).unwrap();

    assert_eq!(usbcmd & regs::USBCMD_HCRST, 0);
    assert_eq!(usbsts & (regs::USBSTS_HSE | regs::USBSTS_PCD), 0);
}

#[test]
fn open_halts_a_running_controller_and_resets() {
    let mut mock = MockXhci::new();
    mock.usbcmd = regs::USBCMD_RUN;
    let xhci = Xhci::open(mock).expect("bring-up succeeds");
    // After open the controller was reset: Run/Stop and HCRST clear.
    let mut xhci = xhci;
    let usbcmd = xhci.host.read32(MockXhci::op(regs::USBCMD)).unwrap();
    assert_eq!(usbcmd & (regs::USBCMD_RUN | regs::USBCMD_HCRST), 0);
}

#[test]
fn open_rejects_absent_controller() {
    // An unmapped/absent device reads all-ones.
    let mut mock = MockXhci::new();
    mock.cap_dword0 = u32::MAX;
    assert_eq!(Xhci::open(mock).err(), Some(DriverError::DeviceFault));
}

#[test]
fn open_rejects_implausible_capability_block() {
    let mut mock = MockXhci::new();
    mock.cap_dword0 = 0x0110_0000 | 0x10; // CAPLENGTH below minimum
    assert_eq!(Xhci::open(mock).err(), Some(DriverError::DeviceFault));

    let mut mock = MockXhci::new();
    mock.cap_dword0 = 0x0080_0000 | MOCK_CAPLENGTH; // pre-0.90 version
    assert_eq!(Xhci::open(mock).err(), Some(DriverError::DeviceFault));

    let mut mock = MockXhci::new();
    mock.hcsparams1 = 0x0400_0000; // zero MaxSlots
    assert_eq!(Xhci::open(mock).err(), Some(DriverError::DeviceFault));

    let mut mock = MockXhci::new();
    mock.hcsparams1 = 0x0000_0020; // zero MaxPorts
    assert_eq!(Xhci::open(mock).err(), Some(DriverError::DeviceFault));

    let mut mock = MockXhci::new();
    mock.dboff = 0;
    assert_eq!(Xhci::open(mock).err(), Some(DriverError::DeviceFault));

    let mut mock = MockXhci::new();
    mock.rtsoff = 0;
    assert_eq!(Xhci::open(mock).err(), Some(DriverError::DeviceFault));
}

#[test]
fn open_fails_closed_when_never_ready() {
    let mut mock = MockXhci::new();
    mock.cnr_stuck = true;
    assert_eq!(
        Xhci::open_with_budget(mock, 16).err(),
        Some(DriverError::DeviceFault)
    );
}

#[test]
fn open_fails_closed_when_reset_sticks() {
    let mut mock = MockXhci::new();
    mock.hcrst_stuck = true;
    assert_eq!(
        Xhci::open_with_budget(mock, 16).err(),
        Some(DriverError::DeviceFault)
    );
}

#[test]
fn open_diagnostic_reports_the_stuck_reset_stage() {
    let mut mock = MockXhci::new();
    mock.hcrst_stuck = true;
    let Err(err) = Xhci::open_diagnostic_with_budget(mock, 16) else {
        panic!("reset must time out")
    };

    assert_eq!(err.error, DriverError::DeviceFault);
    assert_eq!(err.stage, XhciOpenStage::ResetSelfClear);
    assert_eq!(err.registers.usbcmd, Some(regs::USBCMD_HCRST));
    assert_eq!(err.registers.usbsts, Some(regs::USBSTS_HCH));
}

#[test]
fn port_status_decodes_portsc() {
    let mut mock = MockXhci::new();
    // Port 2: connected, enabled, powered, high speed (3), CSC.
    mock.portsc[1] = regs::PORTSC_CCS
        | regs::PORTSC_PED
        | regs::PORTSC_PP
        | regs::PORTSC_CSC
        | (3 << regs::PORTSC_SPEED_SHIFT);
    let mut xhci = Xhci::open(mock).expect("bring-up succeeds");
    let status = xhci.port_status(2).expect("port in range");
    assert!(status.connected());
    assert!(status.enabled());
    assert!(status.powered());
    assert!(status.connect_changed());
    assert!(!status.resetting());
    assert_eq!(status.speed(), 3);
    let empty = xhci.port_status(1).expect("port in range");
    assert!(!empty.connected());
    assert_eq!(empty.speed(), 0);
}

#[test]
fn port_status_rejects_out_of_range_ports() {
    let mut xhci = Xhci::open(MockXhci::new()).expect("bring-up succeeds");
    assert_eq!(xhci.port_status(0), Err(DriverError::OutOfRange));
    assert_eq!(xhci.port_status(5), Err(DriverError::OutOfRange));
}

#[test]
fn doorbells_are_bounds_checked() {
    let mut xhci = Xhci::open(MockXhci::new()).expect("bring-up succeeds");
    xhci.ring_doorbell(0, 0).expect("command doorbell");
    xhci.ring_doorbell(1, 1).expect("device doorbell");
    xhci.ring_doorbell(32, 31).expect("last slot doorbell");
    assert_eq!(xhci.ring_doorbell(33, 1), Err(DriverError::OutOfRange));
    assert_eq!(xhci.ring_doorbell(0, 1), Err(DriverError::OutOfRange));
    assert_eq!(xhci.ring_doorbell(1, 0), Err(DriverError::OutOfRange));
    assert_eq!(xhci.ring_doorbell(1, 32), Err(DriverError::OutOfRange));
    assert_eq!(
        xhci.host.doorbells,
        alloc::vec![(0, 0), (4, 1), (32 * 4, 31)]
    );
}

#[test]
fn trb_type_round_trips_and_fails_closed() {
    for ty in [
        TrbType::Normal,
        TrbType::SetupStage,
        TrbType::DataStage,
        TrbType::StatusStage,
        TrbType::Link,
        TrbType::NoOp,
        TrbType::EnableSlot,
        TrbType::AddressDevice,
        TrbType::ConfigureEndpoint,
        TrbType::NoOpCommand,
        TrbType::TransferEvent,
        TrbType::CommandCompletion,
        TrbType::PortStatusChange,
    ] {
        assert_eq!(TrbType::from_raw(u32::from(ty.as_u8())), Ok(ty));
        assert_eq!(Trb::new(ty, 0, 0, 0).trb_type(), Ok(ty));
    }
    assert_eq!(TrbType::from_raw(0), Err(DriverError::OutOfRange));
    assert_eq!(TrbType::from_raw(63), Err(DriverError::OutOfRange));
}

#[test]
fn event_trb_fields_decode_and_fail_closed() {
    let event = Trb {
        parameter: 0xDEAD_BEEF,
        status: (u32::from(CompletionCode::ShortPacket.as_u8()) << 24) | 5,
        control: (7 << 24) | (u32::from(TrbType::TransferEvent.as_u8()) << 10),
    };
    assert_eq!(event.completion_code(), Ok(CompletionCode::ShortPacket));
    assert_eq!(event.slot_id(), 7);
    let forged = Trb {
        status: 200 << 24,
        ..event
    };
    assert_eq!(forged.completion_code(), Err(DriverError::OutOfRange));
}

/// Apply a [`ring::PushOutcome`](super::ring::PushOutcome) to a local
/// TRB array, standing in for the DMA-memory owner.
fn apply(trbs: &mut [Trb], ring: &ProducerRing, outcome: &super::ring::PushOutcome) {
    trbs[outcome.slot] = outcome.trb;
    if let Some(link) = outcome.link {
        trbs[ring.link_slot()] = link;
    }
}

#[test]
fn producer_ring_rejects_tiny_rings() {
    assert!(matches!(
        ProducerRing::new(2, 0x1000),
        Err(DriverError::LengthOutOfRange)
    ));
}

#[test]
fn producer_ring_stamps_cycle_and_reports_addresses() {
    let mut trbs = [Trb::ZERO; 4];
    let (mut ring, link) = ProducerRing::new(4, 0x1000).expect("ring fits");
    trbs[ring.link_slot()] = link;
    let a = ring.push(Trb::new(TrbType::NoOpCommand, 0, 0, 0)).unwrap();
    apply(&mut trbs, &ring, &a);
    let b = ring.push(Trb::new(TrbType::NoOpCommand, 0, 0, 0)).unwrap();
    apply(&mut trbs, &ring, &b);
    assert_eq!(a.address, 0x1000);
    assert_eq!(b.address, 0x1000 + TRB_LEN as u64);
    assert_eq!(ring.in_flight(), 2);
    // First-pass TRBs carry cycle 1; the link TRB is still unpublished.
    assert!(trbs[0].cycle());
    assert!(trbs[1].cycle());
    assert_eq!(trbs[3].trb_type(), Ok(TrbType::Link));
    assert!(!trbs[3].cycle());
}

#[test]
fn producer_ring_rejects_caller_owned_fields() {
    let (mut ring, _link) = ProducerRing::new(4, 0x1000).expect("ring fits");
    assert!(matches!(
        ring.push(Trb::new(TrbType::NoOpCommand, 0, 0, CONTROL_CYCLE)),
        Err(DriverError::OutOfRange)
    ));
    assert!(matches!(
        ring.push(Trb::new(TrbType::Link, 0x1000, 0, 0)),
        Err(DriverError::OutOfRange)
    ));
}

#[test]
fn producer_ring_full_fails_closed_and_retire_reopens() {
    let (mut ring, _link) = ProducerRing::new(4, 0x1000).expect("ring fits");
    let no_op = Trb::new(TrbType::NoOpCommand, 0, 0, 0);
    ring.push(no_op).expect("slot 0");
    ring.push(no_op).expect("slot 1");
    assert!(matches!(ring.push(no_op), Err(DriverError::Busy)));
    ring.retire_one().expect("one completion");
    ring.push(no_op).expect("freed slot");
    assert_eq!(ring.retire_one(), Ok(()));
    assert_eq!(ring.retire_one(), Ok(()));
    assert_eq!(ring.retire_one(), Err(DriverError::OutOfRange));
}

#[test]
fn producer_ring_wrap_publishes_link_and_toggles_cycle() {
    let mut trbs = [Trb::ZERO; 4];
    let (mut ring, link) = ProducerRing::new(4, 0x1000).expect("ring fits");
    trbs[ring.link_slot()] = link;
    let no_op = Trb::new(TrbType::NoOpCommand, 0, 0, 0);
    let a = ring.push(no_op).expect("slot 0");
    apply(&mut trbs, &ring, &a);
    assert!(a.link.is_none());
    let b = ring.push(no_op).expect("slot 1");
    apply(&mut trbs, &ring, &b);
    ring.retire_one().expect("completion 0");
    ring.retire_one().expect("completion 1");
    // Third push lands in slot 2 — the last data slot — re-publishing
    // the link TRB under cycle 1 and toggling the producer to cycle 0.
    let c = ring.push(no_op).expect("slot 2 wraps");
    apply(&mut trbs, &ring, &c);
    assert_eq!(c.address, 0x1000 + 2 * TRB_LEN as u64);
    assert!(c.link.is_some(), "wrap re-publishes the link TRB");
    // Fourth push lands back in slot 0 under the toggled cycle.
    let d = ring.push(no_op).expect("slot 0 second pass");
    apply(&mut trbs, &ring, &d);
    assert_eq!(d.address, 0x1000);
    assert!(trbs[3].cycle(), "link TRB published under cycle 1");
    assert_eq!(trbs[3].trb_type(), Ok(TrbType::Link));
    assert!(trbs[2].cycle(), "first-pass TRB carries cycle 1");
    assert!(!trbs[0].cycle(), "second-pass TRB carries cycle 0");
}

#[test]
fn event_cursor_rejects_empty_segment() {
    assert!(matches!(
        EventRingCursor::new(0),
        Err(DriverError::LengthOutOfRange)
    ));
}

#[test]
fn event_cursor_consumes_matching_cycle_only() {
    let mut segment = [Trb::ZERO; 3];
    let mut cursor = EventRingCursor::new(3).expect("segment fits");
    // Nothing produced yet: every slot carries cycle 0, cursor wants 1.
    assert_eq!(cursor.pop(&segment), Ok(None));
    segment[0] = Trb::new(
        TrbType::CommandCompletion,
        0x1000,
        u32::from(CompletionCode::Success.as_u8()) << 24,
        CONTROL_CYCLE,
    );
    let event = cursor.pop(&segment).expect("read ok").expect("one event");
    assert_eq!(event.trb_type(), Ok(TrbType::CommandCompletion));
    assert_eq!(cursor.dequeue_index(), 1);
    assert_eq!(cursor.pop(&segment), Ok(None));
}

#[test]
fn event_cursor_owned_peeks_without_advancing() {
    // `owned` reports producer ownership by the cycle bit alone and must not
    // advance the cursor — `poll_event` relies on this to read the cycle, then
    // `dma_rmb`, then re-read and `pop` the entry body (the torn-read fix for
    // non-coherent DMA).
    let mut segment = [Trb::ZERO; 3];
    let mut cursor = EventRingCursor::new(3).expect("segment fits");
    assert_eq!(cursor.owned(&segment), Ok(false), "nothing produced yet");
    segment[0] = Trb::new(
        TrbType::CommandCompletion,
        0x1000,
        u32::from(CompletionCode::Success.as_u8()) << 24,
        CONTROL_CYCLE,
    );
    assert_eq!(cursor.owned(&segment), Ok(true), "producer owns slot 0 now");
    // Peeking twice still does not advance: a following `pop` consumes it.
    assert_eq!(cursor.owned(&segment), Ok(true));
    assert_eq!(cursor.dequeue_index(), 0, "peek left the cursor put");
    assert!(cursor.pop(&segment).unwrap().is_some());
    assert_eq!(cursor.dequeue_index(), 1);
    // A wrong-length segment is rejected like `pop`.
    assert_eq!(
        cursor.owned(&[Trb::ZERO; 4]),
        Err(DriverError::LengthOutOfRange)
    );
}

#[test]
fn event_cursor_wraps_and_toggles_expectation() {
    let mut segment = [Trb::ZERO; 2];
    let mut cursor = EventRingCursor::new(2).expect("segment fits");
    let event = |cycle: bool| Trb {
        parameter: 0,
        status: u32::from(CompletionCode::Success.as_u8()) << 24,
        control: (u32::from(TrbType::PortStatusChange.as_u8()) << 10)
            | if cycle { CONTROL_CYCLE } else { 0 },
    };
    segment[0] = event(true);
    segment[1] = event(true);
    assert!(cursor.pop(&segment).unwrap().is_some());
    assert!(cursor.pop(&segment).unwrap().is_some());
    assert_eq!(cursor.dequeue_index(), 0);
    // Second pass: the controller now produces with cycle 0; stale
    // first-pass TRBs (cycle 1) must not be re-consumed.
    assert_eq!(cursor.pop(&segment), Ok(None));
    segment[0] = event(false);
    assert!(cursor.pop(&segment).unwrap().is_some());
}

#[test]
fn event_cursor_rejects_wrong_segment() {
    let segment = [Trb::ZERO; 3];
    let mut cursor = EventRingCursor::new(4).expect("cursor fits");
    assert_eq!(cursor.pop(&segment), Err(DriverError::LengthOutOfRange));
}

#[test]
fn trb_bytes_round_trip() {
    let trb = Trb {
        parameter: 0x1122_3344_5566_7788,
        status: 0xAABB_CCDD,
        control: 0x0102_0304,
    };
    assert_eq!(Trb::from_bytes(trb.to_bytes()), trb);
    assert_eq!(trb.to_bytes()[0], 0x88, "little-endian on the ring");
}

#[test]
fn transfer_event_field_helpers() {
    let event = Trb {
        parameter: 0x2000,
        status: (u32::from(CompletionCode::ShortPacket.as_u8()) << 24) | 5,
        control: (u32::from(TrbType::TransferEvent.as_u8()) << 10)
            | (3 << 16)
            | trb::control_slot(7),
    };
    assert_eq!(event.endpoint_id(), 3);
    assert_eq!(event.transfer_residual(), 5);
    assert_eq!(event.slot_id(), 7);
}

#[test]
fn device_descriptor_decode_fails_closed() {
    let descriptor = DeviceDescriptor::decode(&MOCK_DESCRIPTOR).expect("fixture decodes");
    assert_eq!(descriptor.vendor_id, 0x046D);
    assert_eq!(descriptor.product_id, 0xC077);
    assert_eq!(descriptor.device_class, 0);
    assert_eq!(descriptor.num_configurations, 1);

    let mut short_length = MOCK_DESCRIPTOR;
    short_length[0] = 17;
    assert_eq!(
        DeviceDescriptor::decode(&short_length),
        Err(DriverError::BadMagic)
    );
    let mut wrong_type = MOCK_DESCRIPTOR;
    wrong_type[1] = 0x02;
    assert_eq!(
        DeviceDescriptor::decode(&wrong_type),
        Err(DriverError::BadMagic)
    );
    let mut no_configs = MOCK_DESCRIPTOR;
    no_configs[17] = 0;
    assert_eq!(
        DeviceDescriptor::decode(&no_configs),
        Err(DriverError::BadMagic)
    );
}

#[test]
fn dma_program_rejects_unaligned_addresses() {
    let aligned = DmaProgram {
        dcbaap: 0x1000,
        command_ring: 0x1040,
        erst: 0x1080,
        event_segment: 0x10C0,
    };
    assert!(aligned.is_plausible());
    assert!(!DmaProgram {
        dcbaap: 0,
        ..aligned
    }
    .is_plausible());
    assert!(!DmaProgram {
        command_ring: 0x1044,
        ..aligned
    }
    .is_plausible());
    let mut xhci = Xhci::open(MockXhci::new()).expect("bring-up succeeds");
    assert_eq!(
        xhci.start(
            &DmaProgram {
                erst: 0x1004,
                ..aligned
            },
            16,
        ),
        Err(DriverError::OutOfRange)
    );
}

/// Deterministic engine event-wait for the tests: a fake microsecond clock
/// that advances by the parked budget on every wait, so a completion that
/// never arrives reaches the wall-clock deadline in one park — the tests
/// never spin and never sleep. The wait count lets a regression assert the
/// engine *parked* rather than polled.
struct TestWait {
    now_us: core::cell::Cell<u64>,
    waits: core::cell::Cell<u32>,
}

impl TestWait {
    /// Leak one for the test process, satisfying the engine's borrow without
    /// bookkeeping (the mock-host `'static` storage strategy).
    fn leaked() -> &'static TestWait {
        alloc::boxed::Box::leak(alloc::boxed::Box::new(TestWait {
            now_us: core::cell::Cell::new(0),
            waits: core::cell::Cell::new(0),
        }))
    }
}

impl EventWait for TestWait {
    fn now_us(&self) -> u64 {
        self.now_us.get()
    }

    fn wait_us(&self, budget_us: u64) {
        self.waits.set(self.waits.get() + 1);
        self.now_us
            .set(self.now_us.get().saturating_add(budget_us.max(1)));
    }
}

/// Open the mock controller and start the engine over the shared
/// buffer.
fn started_device(mock: MockXhci, mem: &SharedMem) -> UsbDevice<'static, MockXhci, MockDma> {
    started_device_with_wait(mock, mem, TestWait::leaked())
}

/// [`started_device`] with a caller-held [`TestWait`], so a regression can
/// observe the engine's parking behaviour.
fn started_device_with_wait(
    mock: MockXhci,
    mem: &SharedMem,
    wait: &'static TestWait,
) -> UsbDevice<'static, MockXhci, MockDma> {
    let xhci = Xhci::open(mock).expect("bring-up succeeds");
    let dma = MockDma::new(Rc::clone(mem), MOCK_DMA_BASE);
    UsbDevice::start(xhci, dma, wait, 4096).expect("engine starts")
}

fn arm_report_request(device: &mut UsbDevice<'_, MockXhci, MockDma>) {
    arm_report_request_for(device, 0);
}

/// [`arm_report_request`] for the served device at `index` (a leaf behind
/// a hub sits above the hub's own entry).
fn arm_report_request_for(device: &mut UsbDevice<'_, MockXhci, MockDma>, index: usize) {
    let mut buf = [0u8; REPORT_LEN];
    assert_eq!(
        device.next_report(index, &mut buf),
        Ok(None),
        "a class report request arms one interrupt-IN transfer and then parks"
    );
}

/// Enumerate and install the root-attached hub on root port 1 **without**
/// descending it, returning its hub-table index — the harness flow for
/// tests that drive the downstream ports' power/reset/class requests
/// themselves rather than letting the walk attach everything.
fn install_root_hub_on_port_1(device: &mut UsbDevice<'_, MockXhci, MockDma>) -> usize {
    match device.attach_root_on_port(1) {
        Ok(AttachOutcome::Hub(hub)) => hub,
        other => panic!("the root hub enumerates and installs: {other:?}"),
    }
}

/// Enumerate and serve the directly-attached leaf device on root-hub
/// `port`, returning its device-table index.
fn attach_root_device(
    device: &mut UsbDevice<'_, MockXhci, MockDma>,
    port: u8,
) -> Result<usize, DriverError> {
    match device.attach_root_on_port(port)? {
        AttachOutcome::Device(index) => Ok(index),
        // These callers attach leaf devices only; a hub here is a harness bug.
        AttachOutcome::Hub(_) => Err(DriverError::BadMagic),
    }
}

/// The downstream-attach step the hub-descent tests drive after
/// [`install_root_hub_on_port_1`]: attach the
/// leaf device on `hub`'s `port` and arm that hub's status-change watch.
fn attach_and_watch(
    device: &mut UsbDevice<'_, MockXhci, MockDma>,
    hub: usize,
    port: u8,
    speed: u8,
) -> Result<usize, DriverError> {
    let outcome = device.attach_downstream_device(hub, port, speed, &TestDelay::default())?;
    device.configure_hub_watch(hub)?;
    match outcome {
        AttachOutcome::Device(index) => Ok(index),
        // These tests attach leaf devices only; a hub here is a harness bug.
        AttachOutcome::Hub(_) => Err(DriverError::BadMagic),
    }
}

/// Enumerate the root hub on root port 1, install it, and ready its
/// downstream `port`: power, reset, and read the post-reset status the
/// attach decision needs. Returns the root hub's table index and the
/// port's `wPortStatus`.
fn install_hub_and_ready_port(
    device: &mut UsbDevice<'_, MockXhci, MockDma>,
    port: u8,
) -> (usize, u16) {
    let hub = install_root_hub_on_port_1(device);
    device
        .power_hub_port(hub, port)
        .expect("power the downstream port");
    device
        .reset_hub_port(hub, port)
        .expect("reset the downstream port");
    let status = device
        .hub_port_status(hub, port)
        .expect("status after reset");
    (hub, status)
}

#[test]
fn usb_device_start_programs_dma_and_runs() {
    let mem = shared_mem();
    let mut device = started_device(MockXhci::with_device(&mem), &mem);
    let mock = device.host_mut();
    assert_eq!(mock.usbcmd & regs::USBCMD_RUN, regs::USBCMD_RUN);
    assert_eq!(mock.config, 32, "all reported slots enabled");
    assert_eq!(MockXhci::qword(mock.dcbaap), MOCK_DMA_BASE);
    assert_eq!(
        MockXhci::qword(mock.crcr) & u64::from(regs::CRCR_RCS),
        1,
        "command ring starts at consumer cycle state 1"
    );
    assert_eq!(mock.erstsz, 1);
    // The single ERST entry names the event segment the initial ERDP
    // points at, sized in TRBs.
    let entry = mock.read_dwords(MockXhci::qword(mock.erstba), 4);
    let segment = (u64::from(entry[1]) << 32) | u64::from(entry[0]);
    assert_eq!(segment, MockXhci::qword(mock.erdp));
    assert_eq!(entry[2] as usize, RING_TRBS);
}

#[test]
fn usb_device_start_rejects_bad_regions() {
    let mem = shared_mem();
    let xhci = Xhci::open(MockXhci::with_device(&mem)).expect("bring-up succeeds");
    let misaligned = MockDma::new(Rc::clone(&mem), MOCK_DMA_BASE + 4);
    assert!(matches!(
        UsbDevice::start(xhci, misaligned, TestWait::leaked(), 4096).err(),
        Some(DriverError::OutOfRange)
    ));

    let tiny = Rc::new(RefCell::new(alloc::vec![0u8; 256]));
    let xhci = Xhci::open(MockXhci::with_device(&tiny)).expect("bring-up succeeds");
    let small = MockDma::new(Rc::clone(&tiny), MOCK_DMA_BASE);
    assert!(matches!(
        UsbDevice::start(xhci, small, TestWait::leaked(), 4096).err(),
        Some(DriverError::LengthOutOfRange)
    ));
}

#[test]
fn hcsparams2_decodes_the_vl805_scratchpad_count() {
    // VL805 datasheet HCSPARAMS2 default `FC000031h` → 31 scratchpad
    // buffers (low field bits 31:27 = 0x1F, high field bits 25:21 = 0).
    assert_eq!(regs::hcsparams2_max_scratchpad(0xFC00_0031), 31);
    // A high-field-only value combines into the 10-bit count.
    assert_eq!(regs::hcsparams2_max_scratchpad(1 << 21), 32);
    // No scratchpad required.
    assert_eq!(regs::hcsparams2_max_scratchpad(0), 0);
}

#[test]
fn pagesize_decodes_the_lowest_supported_page() {
    // Bit 0 → 4 KiB (the VL805's page); a higher bit → its `2^(n+12)`.
    assert_eq!(regs::pagesize_bytes(1), 4096);
    assert_eq!(regs::pagesize_bytes(1 << 4), 1 << 16);
    // An unset register reports no size, so the caller fails closed.
    assert_eq!(regs::pagesize_bytes(0), 0);
}

#[test]
fn start_reserves_scratchpad_and_programs_dcbaa0() {
    // A VL805-shaped controller: 31 page-sized scratchpad buffers, and
    // no command completes until software points `DCBAA[0]` at the
    // scratchpad array (xHCI §4.20). Before this fix the very first
    // Enable Slot produced no completion event (the Pi 4 metal
    // `4126 stage=2 completion=0`); now `start` reserves the buffers, so
    // the command ring runs and enumeration completes.
    let mem = shared_mem();
    let xhci = Xhci::open(MockXhci::with_device_scratchpad(&mem, 31)).expect("bring-up succeeds");
    assert_eq!(xhci.max_scratchpad_buffers(), 31);
    assert_eq!(xhci.page_size(), 4096);
    let dma = MockDma::new(Rc::clone(&mem), MOCK_DMA_BASE);
    let mut device = UsbDevice::start(xhci, dma, TestWait::leaked(), 4096)
        .expect("engine starts with scratchpad");

    // `DCBAA[0]` now points at a non-zero scratchpad pointer array...
    let dcbaa_base = MockXhci::qword(device.host_mut().dcbaap);
    let array = device.host_mut().read_dwords(dcbaa_base, 2);
    let array_ptr = (u64::from(array[1]) << 32) | u64::from(array[0]);
    assert_ne!(array_ptr, 0, "DCBAA[0] points at the scratchpad array");
    // ...whose first entry is a non-zero, page-aligned scratchpad buffer.
    let entry = device.host_mut().read_dwords(array_ptr, 2);
    let page0 = (u64::from(entry[1]) << 32) | u64::from(entry[0]);
    assert_ne!(page0, 0, "scratchpad array entry 0 points at a buffer");
    assert_eq!(page0 % 4096, 0, "scratchpad buffers are page-aligned");

    // And a command actually completes now: enumeration runs end to end.
    let index = attach_root_device(&mut device, 1)
        .expect("enumeration completes once the scratchpad is reserved");
    let identity = device.device_identity(index).expect("identity captured");
    assert_eq!(identity.vendor_id, 0x046D);
}

#[test]
fn start_stalls_without_scratchpad_on_a_controller_that_needs_it() {
    // The same VL805-shaped controller, but the engine is denied a region
    // large enough to reserve the 31 scratchpad pages: `start` fails
    // closed (`LengthOutOfRange`) rather than running a controller whose
    // `DCBAA[0]` it could not program.
    let small: SharedMem = Rc::new(RefCell::new(alloc::vec![0u8; 0x4000]));
    let xhci = Xhci::open(MockXhci::with_device_scratchpad(&small, 31)).expect("bring-up succeeds");
    let dma = MockDma::new(Rc::clone(&small), MOCK_DMA_BASE);
    assert_eq!(
        UsbDevice::start(xhci, dma, TestWait::leaked(), 4096).err(),
        Some(DriverError::LengthOutOfRange)
    );
}

#[test]
fn root_attach_full_chain() {
    let mem = shared_mem();
    let mut device = started_device(MockXhci::with_device(&mem), &mem);
    let index = attach_root_device(&mut device, 1).expect("enumeration succeeds");
    let identity = device.device_identity(index).expect("identity captured");
    assert_eq!(identity.vendor_id, 0x046D);
    assert_eq!(identity.product_id, 0xC077);
    assert_eq!(device.raw_device_slot(0), 1);
    let mock = device.host_mut();
    assert!(mock.addressed, "Address Device reached the model");
    assert!(mock.configured, "Configure Endpoint reached the model");
    assert_eq!(mock.configuration, Some(1), "SET_CONFIGURATION(1) issued");
    assert_eq!(mock.protocol, Some(0), "SET_PROTOCOL selected boot");
}

#[test]
fn root_attach_resets_a_disabled_port() {
    let mem = shared_mem();
    let mut mock = MockXhci::with_device(&mem);
    // Connected but not yet enabled: the USB2 shape before a reset.
    mock.portsc[0] &= !regs::PORTSC_PED;
    let mut device = started_device(mock, &mem);
    attach_root_device(&mut device, 1).expect("reset then enumeration");
    let mock = device.host_mut();
    assert_ne!(mock.portsc[0] & regs::PORTSC_PED, 0, "port re-enabled");
    assert!(mock.configured);
}

#[test]
fn root_attach_fails_closed_on_an_empty_port() {
    let mem = shared_mem();
    let mut device = started_device(MockXhci::with_device(&mem), &mem);
    assert_eq!(
        device.attach_root_on_port(2).err(),
        Some(DriverError::DeviceFault)
    );
    assert_eq!(
        device.attach_root_on_port(0).err(),
        Some(DriverError::OutOfRange)
    );
}

#[test]
fn root_attach_twice_on_one_port_is_refused() {
    let mem = shared_mem();
    let mut device = started_device(MockXhci::with_device(&mem), &mem);
    attach_root_device(&mut device, 1).expect("first enumeration");
    assert_eq!(
        device.attach_root_on_port(1).err(),
        Some(DriverError::Busy),
        "the port already carries a served attachment"
    );
}

#[test]
fn bring_up_serves_the_populated_port() {
    // `with_device` connects a device on root-hub port 1 and leaves the
    // others empty; the walk enumerates port 1 and lands on slot 1.
    let mem = shared_mem();
    let mut device = started_device(MockXhci::with_device(&mem), &mem);
    device
        .bring_up(&TestDelay::default())
        .expect("port 1 is connected");
    let identity = device.device_identity(0).expect("identity captured");
    assert_eq!(identity.vendor_id, 0x046D);
    assert_eq!(device.raw_device_slot(0), 1);
    assert!(device.host_mut().configured);
}

#[test]
fn bring_up_serves_nothing_on_an_empty_root_hub() {
    // No port reports a connected device: the walk comes up serving
    // nothing (a first-class state — the first connect arrives through
    // the root-port scan) rather than guessing a port or failing.
    let mem = shared_mem();
    let mut mock = MockXhci::with_device(&mem);
    mock.portsc[0] &= !regs::PORTSC_CCS;
    let mut device = started_device(mock, &mem);
    device
        .bring_up(&TestDelay::default())
        .expect("an empty controller comes up serving nothing");
    assert!(!device.any_device_live(), "no device was enumerated");
}

#[test]
fn set_port_power_asserts_pp_and_rejects_a_bad_port() {
    // A port-power-controlled controller reports a port unpowered after
    // the open-time Host Controller Reset; `set_port_power` asserts `PP`
    // (xHCI 1.2 §4.19.1.1 / §5.4.8).
    let mut mock = MockXhci::new();
    mock.portsc[0] = 0;
    let mut xhci = Xhci::open(mock).expect("bring-up succeeds");
    assert_eq!(xhci.port_status(1).unwrap().raw() & regs::PORTSC_PP, 0);
    xhci.set_port_power(1).expect("port 1 powers on");
    assert_ne!(xhci.port_status(1).unwrap().raw() & regs::PORTSC_PP, 0);
    // Idempotent on an already-powered port; out-of-range fails closed.
    xhci.set_port_power(1)
        .expect("powering an on port is a no-op");
    assert_eq!(xhci.set_port_power(0), Err(DriverError::OutOfRange));
    assert_eq!(xhci.set_port_power(99), Err(DriverError::OutOfRange));
}

#[test]
fn bring_up_powers_every_root_port() {
    // The walk must power on every reported port before reading connect
    // status, or a port-power-controlled controller hides attached
    // devices. Start with all ports unpowered (the post-reset shape) and
    // confirm each carries `PP` afterwards.
    let mem = shared_mem();
    let mut mock = MockXhci::with_device(&mem);
    for port in 0..mock.portsc.len() {
        mock.portsc[port] &= !regs::PORTSC_PP;
    }
    let mut device = started_device(mock, &mem);
    device
        .bring_up(&TestDelay::default())
        .expect("port 1 is connected once powered");
    let mock = device.host_mut();
    for port in 0..mock.portsc.len() {
        assert_ne!(
            mock.portsc[port] & regs::PORTSC_PP,
            0,
            "root-hub port {port} was powered on"
        );
    }
}

#[test]
fn bring_up_connects_a_port_only_after_power() {
    // Model the VL805: the device reports no Current Connect Status until
    // software powers the port. A walk that read connect status without
    // first asserting `PP` (the old behaviour) would find nothing; the
    // power-then-debounce scan brings the device up.
    let mem = shared_mem();
    let mut mock = MockXhci::with_device(&mem);
    mock.portsc[0] = 0;
    mock.latent_device_port = Some(0);
    let mut device = started_device(mock, &mem);
    device
        .bring_up(&TestDelay::default())
        .expect("the device appears once its port is powered");
    let identity = device.device_identity(0).expect("identity captured");
    assert_eq!(identity.vendor_id, 0x046D);
    assert_eq!(device.raw_device_slot(0), 1);
    assert!(device.host_mut().configured);
}

#[test]
fn root_port_status_raw_reports_each_port_and_rejects_a_bad_port() {
    // The diagnostic accessor walks every reported port and fails closed
    // on an out-of-range port.
    let mem = shared_mem();
    let mut device = started_device(MockXhci::with_device(&mem), &mem);
    assert_eq!(device.root_port_count(), 4);
    let raw = device.root_port_status_raw(1).expect("port 1 reads");
    assert_ne!(raw & regs::PORTSC_CCS, 0, "port 1 has the connected device");
    assert_eq!(device.root_port_status_raw(0), Err(DriverError::OutOfRange));
    assert_eq!(
        device.root_port_status_raw(99),
        Err(DriverError::OutOfRange)
    );
}

#[test]
fn root_attach_tolerates_a_stalled_set_protocol() {
    // `SET_PROTOCOL(boot)` is optional (HID 1.11 §7.2.6): a device that
    // does not implement it STALLs, which is a protocol stall the
    // default control endpoint recovers from. The Pi 4 VL805 keyboard
    // does exactly this (metal `4126 stage=8 completion=6`); the engine
    // must absorb it and finish enumeration rather than aborting an
    // otherwise-usable keyboard, leaving the device in its default
    // protocol (the mock therefore never records a selected protocol).
    let mem = shared_mem();
    let mut mock = MockXhci::with_device(&mem);
    mock.stall_class_requests = true;
    let mut device = started_device(mock, &mem);
    let index = attach_root_device(&mut device, 1).expect("a stalled SET_PROTOCOL is tolerated");
    let identity = device.device_identity(index).expect("identity captured");
    assert_eq!(identity.vendor_id, 0x046D);
    assert_eq!(device.enum_stage(), EnumStage::Configured);
    // The STALL was observed (the diagnostic preserves it) but absorbed.
    assert_eq!(
        device.last_completion_code(),
        CompletionCode::StallError.as_u8()
    );
    assert_eq!(
        device.host_mut().protocol,
        None,
        "the stalled request selected no protocol"
    );
}

#[test]
fn root_attach_records_the_configured_stage_on_success() {
    // A clean enumeration walks the breadcrumb to `Configured`, and the
    // last completion observed is the SET_PROTOCOL status stage's
    // Success — the fault-localising diagnostic reads a healthy run.
    let mem = shared_mem();
    let mut device = started_device(MockXhci::with_device(&mem), &mem);
    assert_eq!(device.enum_stage(), EnumStage::Scan);
    attach_root_device(&mut device, 1).expect("enumeration succeeds");
    assert_eq!(device.enum_stage(), EnumStage::Configured);
    assert_eq!(
        device.last_completion_code(),
        CompletionCode::Success.as_u8()
    );
}

#[test]
fn root_attach_fails_closed_on_a_non_stall_class_fault() {
    // A STALL on the optional SET_PROTOCOL is tolerated, but a *genuine*
    // class-request fault (here a USB transaction error) is not optional
    // — it still fails closed, leaving the breadcrumb
    // at exactly that step with the raw completion code so a metal
    // capture pins the faulting xHCI operation.
    let mem = shared_mem();
    let mut mock = MockXhci::with_device(&mem);
    mock.fault_class_requests = true;
    let mut device = started_device(mock, &mem);
    assert_eq!(
        device.attach_root_on_port(1).err(),
        Some(DriverError::DeviceFault)
    );
    assert_eq!(device.enum_stage(), EnumStage::SetProtocol);
    assert_eq!(
        device.last_completion_code(),
        CompletionCode::UsbTransactionError.as_u8()
    );
}

#[test]
fn root_attach_recognises_a_hub_via_the_device_class() {
    // The Pi 4B's onboard 2109:3431 VIA Labs hub enumerates on root-hub
    // port 1; the keyboard hangs off it, so the attach must recognise
    // the enumerated device is a hub (bDeviceClass 0x09) and install it
    // rather than serving it as a leaf (metal `4102 vendor=2109
    // product=3431`).
    let mem = shared_mem();
    let mut device = started_device(MockXhci::with_hub(&mem, 4, 2), &mem);
    assert_eq!(
        device.attach_root_on_port(1),
        Ok(AttachOutcome::Hub(0)),
        "device class 0x09 is recognised and installed as a hub"
    );
    assert!(!device.any_device_live(), "a hub is never a served leaf");
}

#[test]
fn enumerating_a_hub_leaves_ep0_usable_for_the_hub_descriptor() {
    // A hub is not a HID device: issuing the HID `SET_PROTOCOL(boot)` to
    // it STALLs, and an xHCI STALL halts the control endpoint, so a
    // following hub-descriptor read on EP0 faults (the metal `reading
    // the hub descriptor failed err=device_fault`). The bring-up must
    // therefore not send `SET_PROTOCOL` to a non-HID interface; this
    // asserts the hub never selects a protocol, EP0 stays unhalted, and
    // the hub-descriptor read succeeds. It fails before that gate.
    let mem = shared_mem();
    let mut device = started_device(MockXhci::with_hub(&mem, 4, 2), &mem);
    let _hub = install_root_hub_on_port_1(&mut device);

    assert_eq!(
        device.host_mut().protocol,
        None,
        "a hub is not sent the HID SET_PROTOCOL request"
    );
    assert!(
        !device.host_mut().ep0_halted,
        "EP0 is never STALL-halted enumerating a hub"
    );
    assert_eq!(
        device
            .hub_num_ports()
            .expect("hub descriptor read succeeds"),
        4,
        "the hub-descriptor read runs on a usable EP0"
    );
}

#[test]
fn hub_discovery_finds_the_downstream_device() {
    // After the hub enumerates, reading its descriptor reports the
    // downstream port count, and — once every downstream port is
    // powered — GET_STATUS reports the keyboard's port connected at its
    // speed, while an unpopulated port reads disconnected.
    let mem = shared_mem();
    let mut device = started_device(MockXhci::with_hub(&mem, 4, 2), &mem);
    let hub = install_root_hub_on_port_1(&mut device);

    assert_eq!(device.hub_num_ports().expect("hub descriptor read"), 4);
    for port in 1..=4 {
        device
            .power_hub_port(hub, port)
            .expect("power the downstream port");
    }
    let status = device
        .hub_port_status(hub, 2)
        .expect("downstream port status");
    assert!(
        hub_port_connected(status),
        "the keyboard's port is connected"
    );
    assert_eq!(
        hub_port_speed(status),
        3,
        "the downstream device is high-speed"
    );

    let empty = device
        .hub_port_status(hub, 1)
        .expect("downstream port status");
    assert!(
        !hub_port_connected(empty),
        "an unpopulated downstream port reads disconnected"
    );
}

#[test]
fn hub_port_reads_disconnected_until_powered() {
    // A port-power-controlled hub reports a downstream port
    // disconnected until software sets PORT_POWER (USB 2.0 §11.11), so
    // an unpowered scan finds nothing — mirroring the root-hub path.
    let mem = shared_mem();
    let mut device = started_device(MockXhci::with_hub(&mem, 4, 2), &mem);
    let hub = install_root_hub_on_port_1(&mut device);

    let before = device
        .hub_port_status(hub, 2)
        .expect("downstream port status");
    assert!(
        !hub_port_connected(before),
        "the downstream port reads disconnected before power"
    );
    device
        .power_hub_port(hub, 2)
        .expect("power the downstream port");
    let after = device
        .hub_port_status(hub, 2)
        .expect("downstream port status");
    assert!(
        hub_port_connected(after),
        "the downstream port connects once powered"
    );
}

#[test]
fn enumerating_a_hub_does_not_arm_its_interrupt_endpoint() {
    // A hub has an interrupt status-change endpoint, but this engine
    // never reads it — a hub's downstream ports are polled over EP0
    // hub-class GET_STATUS. Arming it (as the keyboard path does) makes
    // a real hub deliver asynchronous status-change reports that
    // interleave with — and fail — those EP0 control transfers: the
    // controller posts a transfer event for the interrupt TRB, whose
    // pointer is not in the control wait's watch list, so the wait
    // rejects it (REJECT_ADDRESS_MISMATCH) and the faulted transfer
    // leaves the ring wedged (the metal `4127` all-ones `0xffff` reads
    // with `completion=0xd`/`reject=2` on the first ports and no event
    // at all on the rest). Model the hub with a status-change report
    // queued on its interrupt endpoint: because the bring-up never
    // configures or doorbells that endpoint for a hub, the report is
    // never delivered, no async event contaminates EP0, and every
    // hub-class read still succeeds. Fails before the fix (the first
    // hub-class read trips the mismatch); passes after.
    //
    // The interrupt-IN endpoint's doorbell value is `DCI_INTERRUPT_IN`
    // (3, a private const); the control/command doorbells use 1/0.
    const DCI_INTERRUPT_IN_DB: u32 = 3;
    let mem = shared_mem();
    let mut mock = MockXhci::with_hub(&mem, 4, 2);
    mock.pending_reports.push_back(alloc::vec![0x02]);
    let mut device = started_device(mock, &mem);
    let hub = install_root_hub_on_port_1(&mut device);

    assert!(
        !device
            .host_mut()
            .doorbells
            .iter()
            .any(|&(_, value)| value == DCI_INTERRUPT_IN_DB),
        "a hub's interrupt-IN endpoint is never doorbelled"
    );

    assert_eq!(
        device
            .hub_num_ports()
            .expect("hub descriptor read succeeds"),
        4,
    );
    for port in 1..=4 {
        device
            .power_hub_port(hub, port)
            .expect("power the downstream port");
    }
    let status = device
        .hub_port_status(hub, 2)
        .expect("downstream port status read succeeds despite the queued report");
    assert!(
        hub_port_connected(status),
        "the keyboard's downstream port is connected"
    );
}

#[test]
fn enumerate_downstream_hid_addresses_a_full_speed_keyboard_through_the_hub() {
    // The Pi 4B metal case: the onboard 2109:3431 hub enumerates on slot
    // 1, and a *full-speed* keyboard hangs off a downstream port (the
    // metal `4127` capture: connected, no speed bit → full speed). Reach
    // it on a second xHCI slot whose slot context carries the Route
    // String (the downstream port) and — because a full-speed device
    // behind a high-speed hub must split its transactions — the TT Hub
    // Slot ID (the hub's slot) and TT Port Number (xHCI §6.2.2 / §8.9).
    // The mock faults Address Device unless those are programmed exactly,
    // so reaching the keyboard descriptor proves the driver built them.
    let mem = shared_mem();
    let mut mock = MockXhci::with_hub(&mem, 4, 4);
    // A full-speed downstream device: Current Connect Status only, no
    // High-Speed bit (the metal `wstatus 0x0101` after power: connect +
    // power, no speed bit).
    mock.hub_downstream_status = 1 << 0;
    let mut device = started_device(mock, &mem);

    let root_hub = install_root_hub_on_port_1(&mut device);
    // The resting control context is the freshly installed hub, so the
    // active slot is the hub's own.
    let hub_slot = device.active_slot();

    // Bring the keyboard's downstream port up: power, reset, confirm
    // enabled (the caller owns these wall-clock delays on metal).
    device
        .power_hub_port(root_hub, 4)
        .expect("power the downstream port");
    assert!(
        hub_port_connected(device.hub_port_status(root_hub, 4).expect("status")),
        "the keyboard's port is connected once powered"
    );
    device
        .reset_hub_port(root_hub, 4)
        .expect("reset the downstream port");
    let status = device
        .hub_port_status(root_hub, 4)
        .expect("status after reset");
    assert!(
        hub_port_enabled(status),
        "the downstream port is enabled after reset"
    );
    let speed = hub_port_speed(status);
    assert_eq!(speed, 1, "the keyboard reports full speed behind the hub");

    let keyboard = attach_and_watch(&mut device, root_hub, 4, speed)
        .expect("the keyboard behind the hub is addressed and configured");
    let identity = device
        .device_identity(keyboard)
        .expect("the downstream device is served, not another hub");
    assert_eq!(identity.vendor_id, 0x046D);
    assert_eq!(identity.product_id, 0xC077);

    // The keyboard occupies a *second* slot, distinct from the hub's,
    // and the engine is now pointed at it.
    let kbd_slot = device.raw_device_slot(keyboard);
    assert_ne!(kbd_slot, hub_slot, "the keyboard gets its own slot");
    assert_eq!(kbd_slot, 2);

    // The mock validated and recorded the Route String it was addressed
    // with — the hub's downstream port.
    assert_eq!(device.host_mut().downstream_route_port, 4);

    // The keyboard's HID interface is captured for the hardware-tree
    // child node, and a class report request drains after the controller
    // completes it.
    let node = device
        .describe_device(keyboard, 0, 1)
        .expect("the keyboard describes a child node");
    assert_eq!(node.class(), Some(rustos_abi::HwDeviceClass::Input));
    arm_report_request_for(&mut device, keyboard);
    device
        .host_mut()
        .pending_reports
        .push_back(alloc::vec![0x00, 0x00, 0x04, 0x00, 0x00, 0x00, 0x00, 0x00,]);
    device.host_mut().process_int_ring();
    let mut buf = [0u8; REPORT_LEN];
    let len = device
        .next_report(keyboard, &mut buf)
        .expect("a report drains")
        .expect("a report is available");
    assert_eq!(len, REPORT_LEN);
    assert_eq!(buf[2], 0x04, "the 'a' keycode reaches the report buffer");
}

/// A deterministic [`Delay`] for the host tests: counts `delay_us`
/// invocations and advances a synthetic monotonic clock, so a test asserts
/// the hub settle windows were honoured without sleeping (no flaky tests).
#[derive(Default)]
struct TestDelay {
    calls: core::cell::Cell<u32>,
    now: core::cell::Cell<u64>,
}

impl Delay for TestDelay {
    fn delay_us(&self, us: u32) {
        self.calls.set(self.calls.get() + 1);
        self.now.set(self.now.get() + u64::from(us));
    }

    fn now_us(&self) -> u64 {
        self.now.get()
    }
}

#[test]
fn bring_up_keyboard_returns_a_directly_attached_keyboard() {
    // A keyboard wired straight to a root-hub port (no intervening hub):
    // the orchestration enumerates the first connected port and, because
    // the device is not a hub, returns it without touching the clock.
    let mem = shared_mem();
    let mut device = started_device(MockXhci::with_device(&mem), &mem);
    let delay = TestDelay::default();

    device
        .bring_up(&delay)
        .expect("the directly-attached keyboard enumerates");
    let descriptor = device
        .device_identity(0)
        .expect("a directly-attached keyboard must enumerate now");
    assert!(device.device_live(0), "the enumerated device is live");
    assert_eq!(descriptor.vendor_id, 0x046D);
    assert_eq!(
        delay.calls.get(),
        0,
        "no hub tier means no settle window is waited"
    );

    // Its boot report drains after the class side asks for one.
    arm_report_request(&mut device);
    device
        .host_mut()
        .pending_reports
        .push_back(alloc::vec![0x00, 0x00, 0x04, 0x00, 0x00, 0x00, 0x00, 0x00]);
    device.host_mut().process_int_ring();
    let mut buf = [0u8; REPORT_LEN];
    let len = device
        .next_report(0, &mut buf)
        .expect("a report drains")
        .expect("a report is available");
    assert_eq!(len, REPORT_LEN);
    assert_eq!(buf[2], 0x04, "the 'a' keycode reaches the report buffer");
}

#[test]
fn bring_up_keyboard_descends_through_a_hub_to_the_keyboard() {
    // The Pi 4B metal topology: the onboard hub enumerates on the root
    // port and a full-speed keyboard hangs off a downstream port. The
    // orchestration recognises the hub, powers its ports, waits the
    // power-on-good window, resets the connected port, waits reset
    // recovery, and addresses the keyboard on a second slot — without the
    // caller naming a port (discovered, not guessed).
    let mem = shared_mem();
    let mut mock = MockXhci::with_hub(&mem, 4, 4);
    // Full-speed downstream device (the metal `wstatus` case: connect, no
    // high-speed bit), so its transactions split through the hub's TT.
    mock.hub_downstream_status = 1 << 0;
    let mut device = started_device(mock, &mem);
    let delay = TestDelay::default();

    device
        .bring_up(&delay)
        .expect("the keyboard behind the onboard hub is reached");
    // Entry 0's region carries the root hub's contexts (a hub claims its
    // entry exactly as a nested hub does), so the first leaf device takes
    // index 1.
    let keyboard = device
        .device_identity(1)
        .expect("a connected downstream keyboard must enumerate now");
    assert_eq!(keyboard.vendor_id, 0x046D);
    assert_eq!(keyboard.product_id, 0xC077);
    // Descended one tier: the keyboard sits on a second xHCI slot,
    // addressed through the hub's downstream port 4.
    assert_eq!(
        device.raw_device_slot(1),
        2,
        "the keyboard gets its own slot"
    );
    assert_eq!(device.host_mut().downstream_route_port, 4);
    // Every hardware settle window was honoured exactly once: power-on-good,
    // one reset-completion poll interval (the hub reports the port enabled
    // on the first read), then the TRSTRCY reset-recovery settle.
    assert_eq!(delay.calls.get(), 3);

    // With the hub marked and the endpoint configured, a requested report drains.
    arm_report_request_for(&mut device, 1);
    device
        .host_mut()
        .pending_reports
        .push_back(alloc::vec![0x00, 0x00, 0x04, 0x00, 0x00, 0x00, 0x00, 0x00]);
    device.host_mut().process_int_ring();
    let mut buf = [0u8; REPORT_LEN];
    let len = device
        .next_report(1, &mut buf)
        .expect("a report drains")
        .expect("a report is available");
    assert_eq!(len, REPORT_LEN);
    assert_eq!(buf[2], 0x04);
}

#[test]
fn bring_up_keyboard_arms_the_hub_watch_when_no_downstream_device_is_present() {
    // The root device is the onboard hub, but no downstream port has a
    // device yet (a cold boot with the keyboard unplugged). Bring-up must
    // NOT fail: the controller comes up, the hub's status-change watch is
    // armed, and `AwaitingDevice` is returned so the HCD waits for the first
    // connect event rather than failing closed.
    let mem = shared_mem();
    let mut mock = MockXhci::with_hub(&mem, 4, 4);
    // No connect bit, so every downstream port reads disconnected even
    // after it is powered.
    mock.hub_downstream_status = 0;
    let mut device = started_device(mock, &mem);
    let delay = TestDelay::default();

    device
        .bring_up(&delay)
        .expect("bring-up leaves the controller serving");
    assert!(
        !device.any_device_live(),
        "a hub with nothing attached downstream comes up awaiting a device"
    );
    assert!(
        device.hub_watch_active(),
        "the hub status-change watch is armed so the first connect is delivered event-driven"
    );
    assert!(
        !device.device_live(0),
        "no HID device is live until one connects downstream"
    );
    // The power-on-good window was waited once; the reset-recovery wait is
    // never reached because no connected port is found.
    assert_eq!(delay.calls.get(), 1);
}

#[test]
fn bring_up_keyboard_then_a_downstream_connect_enumerates_a_fresh_keyboard() {
    // The cold-boot hot-plug path: the controller comes up with the onboard
    // hub present but no downstream device (`AwaitingDevice`, watch armed),
    // then a keyboard is plugged into a downstream port. A hub status-change
    // report drives `next_hub_change` to enumerate it as a brand-new device,
    // exactly as a re-attach would, and the keyboard's reports then drain.
    let mem = shared_mem();
    let mut mock = MockXhci::with_hub(&mem, 4, 4);
    mock.hub_downstream_status = 0; // nothing attached downstream at boot
    let mut device = started_device(mock, &mem);
    let delay = TestDelay::default();

    device
        .bring_up(&delay)
        .expect("bring-up leaves the controller serving");
    assert!(
        !device.any_device_live(),
        "cold boot with no downstream device comes up awaiting one"
    );
    assert!(device.hub_watch_active());

    // A full-speed keyboard is now plugged into downstream port 4: the hub
    // latches a connect change and posts a status-change report naming that
    // port (bit 4 of the change bitmap).
    device.host_mut().hub_downstream_status = 1 << 0;
    device.host_mut().hub_downstream_change = PORT_CHANGE_CONNECTION;
    device.host_mut().post_hub_status_change(&[1 << 4]);

    let index = match device
        .next_hub_change(&delay)
        .expect("the status-change report is serviced")
    {
        HubEvent::Attached(index) => index,
        other => panic!("a downstream connect must enumerate a device, got {other:?}"),
    };
    let identity = device
        .device_identity(index)
        .expect("the downstream device is the served keyboard");
    assert_eq!(identity.vendor_id, 0x046D);
    assert!(
        device.device_live(index),
        "the freshly-attached keyboard is now live"
    );

    // Keystrokes flow over the freshly-enumerated slot.
    arm_report_request_for(&mut device, index);
    device
        .host_mut()
        .pending_reports
        .push_back(alloc::vec![0x00, 0x00, 0x04, 0x00, 0x00, 0x00, 0x00, 0x00]);
    device.host_mut().process_int_ring();
    let mut buf = [0u8; REPORT_LEN];
    let len = device
        .next_report(index, &mut buf)
        .expect("a report drains")
        .expect("a report is available after the cold-boot attach");
    assert_eq!(len, REPORT_LEN);
    assert_eq!(buf[2], 0x04, "the 'a' keycode reaches the report buffer");
}

#[test]
fn addressing_a_downstream_keyboard_marks_the_parent_hub_as_a_hub() {
    // The metal regression: the keyboard behind the onboard hub was
    // addressed (`4128`) but never delivered a report, because the hub's
    // slot context was left with the Hub bit clear, so the controller
    // never scheduled the full-speed keyboard's split transactions. The
    // fix issues a Configure Endpoint over the hub's slot that sets the
    // Hub bit, Number of Ports, and TT Think Time before addressing the
    // device behind it. The mock requires the Hub bit on that command and
    // delivers no downstream interrupt report until it is set, so this
    // test fails before the fix (no report) and passes after.
    let mem = shared_mem();
    let mut mock = MockXhci::with_hub(&mem, 4, 4);
    // A full-speed downstream keyboard (the metal case): its interrupt
    // transfers must be split through the hub's TT.
    mock.hub_downstream_status = 1 << 0;
    let mut device = started_device(mock, &mem);

    let (hub, status) = install_hub_and_ready_port(&mut device, 4);
    let keyboard = attach_and_watch(&mut device, hub, 4, hub_port_speed(status))
        .expect("the keyboard behind the hub is addressed");

    // The parent hub was marked a hub with its real port count, the
    // precondition for the controller to route/split to the keyboard.
    assert!(
        device.host_mut().hub_marked_as_hub,
        "the hub's slot context gets the Hub bit before the downstream device is addressed"
    );
    assert_eq!(
        device.host_mut().hub_ctx_num_ports,
        4,
        "the hub's downstream port count reaches the slot context"
    );

    // With the hub marked, a requested report now drains — keystrokes flow.
    arm_report_request_for(&mut device, keyboard);
    device
        .host_mut()
        .pending_reports
        .push_back(alloc::vec![0x00, 0x00, 0x04, 0x00, 0x00, 0x00, 0x00, 0x00,]);
    device.host_mut().process_int_ring();
    let mut buf = [0u8; REPORT_LEN];
    let len = device
        .next_report(keyboard, &mut buf)
        .expect("a report drains")
        .expect("a report is available once the hub is marked");
    assert_eq!(len, REPORT_LEN);
    assert_eq!(buf[2], 0x04);
}

#[test]
fn the_downstream_interrupt_endpoint_carries_a_nonzero_max_esit_payload() {
    // The metal regression: the full-speed keyboard behind the onboard
    // hub was addressed (`4128`) and the hub marked, yet typing produced
    // nothing and the poll-loop heartbeat (`4131`) climbed with
    // `events=0` — the controller serviced the interrupt endpoint never.
    // Root cause: the endpoint context left Max ESIT Payload zero
    // (§6.2.3.8 dword 4 bits 16:31), so the periodic scheduler reserved
    // no bandwidth for the split transactions (§4.14.2). The fix
    // programs Max ESIT Payload = the max packet size for a periodic
    // endpoint. The mock now delivers no interrupt report while it is
    // zero, so this test fails before the fix (no report drains, and the
    // payload assertion fails) and passes after.
    let mem = shared_mem();
    let mut mock = MockXhci::with_hub(&mem, 4, 4);
    mock.hub_downstream_status = 1 << 0; // full-speed downstream device
    let mut device = started_device(mock, &mem);

    let (hub, status) = install_hub_and_ready_port(&mut device, 4);
    let keyboard = attach_and_watch(&mut device, hub, 4, hub_port_speed(status))
        .expect("the keyboard behind the hub is addressed");

    assert_ne!(
        device.host_mut().int_max_esit,
        0,
        "the interrupt-IN endpoint context carries a non-zero Max ESIT \
         Payload so the periodic scheduler reserves bandwidth for it"
    );

    // And, with bandwidth reserved, a requested report actually drains.
    arm_report_request_for(&mut device, keyboard);
    device
        .host_mut()
        .pending_reports
        .push_back(alloc::vec![0x00, 0x00, 0x04, 0x00, 0x00, 0x00, 0x00, 0x00,]);
    device.host_mut().process_int_ring();
    let mut buf = [0u8; REPORT_LEN];
    let len = device
        .next_report(keyboard, &mut buf)
        .expect("a report drains")
        .expect("a report is available once the endpoint has bandwidth");
    assert_eq!(len, REPORT_LEN);
    assert_eq!(buf[2], 0x04);
}

#[test]
fn downstream_keyboard_is_serviced_on_its_descriptor_reported_endpoint() {
    // The metal regression after every prior fix: the keyboard behind
    // the onboard hub was addressed (`4128`) and the hub marked, the
    // interrupt endpoint carried a non-zero Max ESIT Payload, yet typing
    // produced nothing and the poll loop spun with `events=0`. Root
    // cause: the driver hard-coded the interrupt endpoint as endpoint 1
    // (DCI 3); a keyboard whose interrupt-IN endpoint is elsewhere left
    // the controller polling — and the doorbell ringing — the wrong DCI,
    // so it scheduled the real endpoint never.
    //
    // This keyboard reports its interrupt-IN endpoint as **endpoint 2**
    // (DCI 5). The fix reads the endpoint descriptor and configures,
    // doorbells, and drains DCI 5. The mock derives the configured DCI
    // from the Configure Endpoint add flags and posts interrupt events
    // with it, so before the fix the report would arrive on DCI 3 (which
    // the driver no longer expects) — the report does not drain — and
    // after the fix it drains on DCI 5.
    let mem = shared_mem();
    let mut mock = MockXhci::with_hub(&mem, 4, 4);
    mock.hub_downstream_status = 1 << 0; // full-speed downstream device
    mock.keyboard_config = &MOCK_CONFIG_DESCRIPTOR_EP2;
    let mut device = started_device(mock, &mem);

    let (hub, status) = install_hub_and_ready_port(&mut device, 4);
    let keyboard = attach_and_watch(&mut device, hub, 4, hub_port_speed(status))
        .expect("the keyboard behind the hub is addressed on its real endpoint");
    assert!(device.device_live(keyboard));

    // The Configure Endpoint named DCI 5 (endpoint 2 IN), read from the
    // endpoint descriptor — not the assumed DCI 3.
    assert_eq!(
        device.host_mut().int_dci,
        5,
        "the interrupt endpoint is configured at the descriptor-reported DCI 5"
    );

    // A requested report drains: the controller services DCI 5 and the
    // driver accepts the Transfer Event for that endpoint id.
    arm_report_request_for(&mut device, keyboard);
    device
        .host_mut()
        .pending_reports
        .push_back(alloc::vec![0x00, 0x00, 0x04, 0x00, 0x00, 0x00, 0x00, 0x00,]);
    device.host_mut().process_int_ring();
    let mut buf = [0u8; REPORT_LEN];
    let len = device
        .next_report(keyboard, &mut buf)
        .expect("a report drains")
        .expect("a report is available on the endpoint the keyboard actually uses");
    assert_eq!(len, REPORT_LEN);
    assert_eq!(buf[2], 0x04, "the 'a' keycode reaches the report buffer");
}

#[test]
fn enumerate_downstream_hid_omits_the_tt_for_a_high_speed_device() {
    // A high-speed device behind a high-speed hub needs no transaction
    // translator: its slot context's TT fields stay zero (xHCI §6.2.2).
    // The mock faults Address Device if a TT is programmed for a
    // high-speed device, so success proves the driver omits it.
    let mem = shared_mem();
    // `with_hub` defaults the downstream device to high speed.
    let mock = MockXhci::with_hub(&mem, 4, 3);
    let mut device = started_device(mock, &mem);

    let (hub, status) = install_hub_and_ready_port(&mut device, 3);
    assert_eq!(hub_port_speed(status), 3, "high-speed downstream device");

    let keyboard = attach_and_watch(&mut device, hub, 3, hub_port_speed(status))
        .expect("a high-speed downstream HID device is addressed without a TT");
    assert!(device.device_live(keyboard));
    assert_eq!(device.host_mut().downstream_route_port, 3);
}

#[test]
fn attach_downstream_before_a_hub_is_installed_fails_closed() {
    // Addressing a downstream device requires a live installed hub (its
    // slot is the route's root and its TT hub). Without one the call
    // fails closed rather than addressing a device at a guessed topology.
    let mem = shared_mem();
    let mock = MockXhci::with_hub(&mem, 4, 4);
    let mut device = started_device(mock, &mem);
    assert_eq!(
        attach_and_watch(&mut device, 0, 4, 1),
        Err(DriverError::DeviceFault),
    );
}

#[test]
fn a_forged_hub_descriptor_fails_the_attach_closed() {
    // A hub descriptor with the wrong bDescriptorType is forged/corrupt
    // and rejected fail-closed: the attach's hub install reads the
    // topology from it, so the whole attach refuses rather than serving
    // a hub whose port count is a guess.
    let mem = shared_mem();
    let mut mock = MockXhci::with_hub(&mem, 4, 2);
    mock.forge_hub_descriptor = true;
    let mut device = started_device(mock, &mem);
    assert_eq!(
        device.attach_root_on_port(1).err(),
        Some(DriverError::BadMagic)
    );
}

#[test]
fn a_garbled_hub_descriptor_reply_is_retried_and_the_attach_succeeds() {
    // The Pi 4 metal failure: a multi-drive enclosure's RTS5411 hub
    // enumerated cleanly, then answered the hub-descriptor read with a
    // *successful* transfer whose bytes were not a hub descriptor, and
    // the whole tier behind it was refused. Production stacks retry this
    // exchange; this pins that one garbled reply costs nothing — the
    // bounded retry reads the honest descriptor and the hub installs.
    let mem = shared_mem();
    let mut mock = MockXhci::with_hub(&mem, 4, 2);
    mock.garble_hub_descriptor_replies = 1;
    let mut device = started_device(mock, &mem);
    assert!(
        matches!(device.attach_root_on_port(1), Ok(AttachOutcome::Hub(_))),
        "one garbled hub-descriptor reply is retried, never fatal"
    );
}

#[test]
fn a_superspeed_hub_installs_with_its_own_descriptor_and_hub_depth() {
    // The Pi 4 metal failure on the `SuperSpeed` root port: the enclosure's
    // RTS5411 is an SS hub there — it serves only the 12-byte 0x2A hub
    // descriptor and refuses the USB 2.0 0x29 request the engine used to
    // send, so the whole tier was skipped at boot and on hot-plug alike
    // (BadMagic at EnumStage::Configured while the USB 2.0 port worked).
    // This pins the `SuperSpeed` path end to end: the descriptor read at
    // the hub's own type, the mandatory SET_HUB_DEPTH told the tier depth
    // (0 for a root-attached hub), and the downstream device addressed as
    // `SuperSpeed` — its exponent-encoded bMaxPacketSize0 refuses any
    // misdecoded USB 2.0 port speed.
    let mem = shared_mem();
    let mock = MockXhci::with_ss_hub(&mem, 4, 2);
    let mut device = started_device(mock, &mem);
    assert!(
        matches!(
            device.attach_root_port(1, &TestDelay::default()),
            Ok(AttachOutcome::Hub(_))
        ),
        "an SS hub installs through its own 0x2A descriptor"
    );
    assert_eq!(
        device.host_mut().hub_depth_set,
        Some(0),
        "a root-attached SS hub is told tier depth 0 before its ports serve"
    );
    let fault = device.last_attach_fault();
    assert_eq!(
        device.host_mut().downstream_route_port,
        2,
        "the SS downstream device is addressed (at `SuperSpeed`, or its \
         descriptor validation would have refused the attach): {fault:?}"
    );
}

#[test]
fn persistently_garbled_hub_descriptor_replies_fail_the_attach_closed() {
    // A hub that answers the hub-descriptor read wrongly on every attempt
    // exhausts the bounded retry budget and the attach still fails closed
    // — the retry rescues a one-off wrong answer, never loops forever or
    // serves a hub whose topology is a guess.
    let mem = shared_mem();
    let mut mock = MockXhci::with_hub(&mem, 4, 2);
    mock.garble_hub_descriptor_replies = u8::MAX;
    let mut device = started_device(mock, &mem);
    assert_eq!(
        device.attach_root_on_port(1).err(),
        Some(DriverError::BadMagic)
    );
}

#[test]
fn faulting_hub_port_status_records_the_completion_code() {
    // The metal capture reached `4127` for every downstream port but
    // each `wstatus` read as the all-ones sentinel — the per-port class
    // `GET_STATUS` faulted while the hub-descriptor read and Port-Power
    // writes succeeded. The bring-up diagnostic surfaces the raw xHCI
    // completion code so a metal capture can tell *why*; this pins that a faulting `GET_STATUS` fails closed and
    // leaves `last_completion_code()` at the failing code rather than a
    // stale success.
    let mem = shared_mem();
    let mut mock = MockXhci::with_hub(&mem, 4, 2);
    mock.fault_hub_port_status = true;
    let mut device = started_device(mock, &mem);
    let hub = install_root_hub_on_port_1(&mut device);

    assert_eq!(
        device.hub_port_status(hub, 2),
        Err(DriverError::EndpointStalled),
        "a STALLed GET_STATUS fails closed, with the refusal surfaced
         distinctly and the control endpoint already recovered"
    );
    assert_eq!(
        device.last_completion_code(),
        CompletionCode::StallError.as_u8(),
        "the failing completion code is preserved for the diagnostic"
    );
}

#[test]
fn faulting_hub_port_status_records_an_undecodable_completion_code() {
    // The metal capture reported `completion_hex=0` for every per-port
    // `GET_STATUS` — but the fast (logging-cadence) failure means an
    // event *did* arrive; `0` is the diagnostic mislabelling a
    // real-but-rejected code as a timeout. `await_event_for` previously
    // returned before the caller recorded the code whenever the event
    // carried a completion code this driver does not model (its
    // fail-closed `completion_code()` decode), leaving
    // `last_completion_code()` at the `0` "no event" sentinel. The fix
    // records the raw code as the event is observed, so a reserved /
    // controller-specific code (here xHCI `7`, Resource Error) survives
    // for the metal capture. This fails before the
    // fix (code lost to `0`) and passes after.
    const RESOURCE_ERROR: u8 = 7;
    let mem = shared_mem();
    let mut mock = MockXhci::with_hub(&mem, 4, 2);
    mock.fault_hub_port_status_raw = RESOURCE_ERROR;
    let mut device = started_device(mock, &mem);
    let hub = install_root_hub_on_port_1(&mut device);

    assert_eq!(
        device.hub_port_status(hub, 2),
        Err(DriverError::OutOfRange),
        "an undecodable GET_STATUS completion fails closed on the decode"
    );
    assert_eq!(
        device.last_completion_code(),
        RESOURCE_ERROR,
        "the raw, undecodable completion code is preserved for the diagnostic"
    );
}

#[test]
fn faulting_hub_port_status_records_an_unexpected_event_type() {
    // The next metal capture read `completion_hex=0` on two ports with
    // the *fast* failure cadence — i.e. an event arrived but it was not
    // a completion the wait expected. `await_event_for` rejects an event
    // whose TRB-type it does not handle (an asynchronous controller
    // event interleaved with the awaited transfer) via its `_` arm,
    // which records no completion code — so `completion_hex=0` alone
    // cannot tell that from a genuine poll-budget timeout. The reject
    // now records `last_reject_reason()=1` (unexpected type) and the raw
    // type in `last_event_type()`, while `last_completion_code()` stays
    // `0` truthfully (no completion code was carried), distinguishing
    // the two. Fails before the fix (no such
    // accessors / reason lost); passes after.
    let unexpected = TrbType::NoOp.as_u8();
    let mem = shared_mem();
    let mut mock = MockXhci::with_hub(&mem, 4, 2);
    mock.fault_hub_port_status_evtype = unexpected;
    let mut device = started_device(mock, &mem);
    let hub = install_root_hub_on_port_1(&mut device);

    assert_eq!(
        device.hub_port_status(hub, 2),
        Err(DriverError::DeviceFault),
        "an unexpected event type fails the GET_STATUS wait closed"
    );
    assert_eq!(
        device.last_reject_reason(),
        1,
        "the reject reason names an unexpected event type"
    );
    assert_eq!(
        device.last_event_type(),
        unexpected,
        "the rejected event's raw TRB-type is preserved for the diagnostic"
    );
    assert_eq!(
        device.last_completion_code(),
        0,
        "no completion code was carried — truthfully 0, not a timeout label"
    );
}

#[test]
fn reports_flow_through_the_report_source() {
    let mem = shared_mem();
    let mut device = started_device(MockXhci::with_device(&mem), &mem);
    attach_root_device(&mut device, 1).expect("enumeration succeeds");

    let mut buf = [0u8; REPORT_LEN];
    assert_eq!(device.next_report(0, &mut buf), Ok(None));
    device
        .host_mut()
        .pending_reports
        .push_back(alloc::vec![0, 0, 0x04, 0, 0, 0, 0, 0]);
    device.host_mut().process_int_ring();
    assert_eq!(device.next_report(0, &mut buf), Ok(Some(REPORT_LEN)));
    assert_eq!(buf, [0, 0, 0x04, 0, 0, 0, 0, 0]);

    // The 3-byte mouse report arrives as a short packet.
    assert_eq!(device.next_report(0, &mut buf), Ok(None));
    device
        .host_mut()
        .pending_reports
        .push_back(alloc::vec![0x01, 0xFF, 0x02]);
    device.host_mut().process_int_ring();
    assert_eq!(device.next_report(0, &mut buf), Ok(Some(3)));
    assert_eq!(buf[..3], [0x01, 0xFF, 0x02]);
    assert_eq!(device.next_report(0, &mut buf), Ok(None));
}

#[test]
fn report_source_rearms_across_the_ring_wrap() {
    let mem = shared_mem();
    let mut device = started_device(MockXhci::with_device(&mem), &mem);
    attach_root_device(&mut device, 1).expect("enumeration succeeds");

    // More reports than the ring's data slots: arming and draining them all
    // proves retire + on-demand arm keep the ring live across the Link-TRB wrap.
    let total = 2 * RING_TRBS;

    let mut buf = [0u8; REPORT_LEN];
    for index in 0..total {
        let marker = u8::try_from(index).expect("small index");
        assert_eq!(device.next_report(0, &mut buf), Ok(None));
        device
            .host_mut()
            .pending_reports
            .push_back(alloc::vec![marker, 0, 0, 0, 0, 0, 0, 0]);
        device.host_mut().process_int_ring();
        assert_eq!(device.next_report(0, &mut buf), Ok(Some(REPORT_LEN)));
        assert_eq!(buf[0], marker, "reports arrive in order");
    }
    assert_eq!(device.next_report(0, &mut buf), Ok(None));
}

#[test]
fn report_source_rearms_after_a_rejected_completion() {
    // A single transfer event the driver rejects per-report (an
    // unexpected completion code) must still leave the interrupt endpoint
    // re-armed, so the *next* report is delivered. Before the re-arm
    // hardening this returned the error *before* retiring/arming the ring,
    // so the endpoint went silent forever and a busy-polling keyboard
    // driver kept reading an empty event ring while the keyboard appeared
    // dead after one keystroke (the on-metal HDMI-console symptom). This
    // fails before the fix (the second report never arrives) and passes
    // after.
    let mem = shared_mem();
    let mut device = started_device(MockXhci::with_device(&mem), &mem);
    attach_root_device(&mut device, 1).expect("enumeration succeeds");

    // The next report posts a non-Success/ShortPacket completion code the
    // decode rejects; the one after is normal.
    let mut buf = [0u8; REPORT_LEN];
    device.host_mut().fault_one_report_completion = Some(CompletionCode::StallError);
    assert_eq!(device.next_report(0, &mut buf), Ok(None));
    device
        .host_mut()
        .pending_reports
        .push_back(alloc::vec![0xAA, 0, 0, 0, 0, 0, 0, 0]);
    device.host_mut().process_int_ring();

    // The rejected report surfaces a per-report fault…
    assert_eq!(
        device.next_report(0, &mut buf),
        Err(DriverError::DeviceFault)
    );
    // …but the ring was retired, so the next class request can arm a fresh
    // transfer and the following good report still arrives rather than the
    // keyboard going permanently silent.
    assert_eq!(device.next_report(0, &mut buf), Ok(None));
    device
        .host_mut()
        .pending_reports
        .push_back(alloc::vec![0, 0, 0x05, 0, 0, 0, 0, 0]);
    device.host_mut().process_int_ring();
    assert_eq!(device.next_report(0, &mut buf), Ok(Some(REPORT_LEN)));
    assert_eq!(buf, [0, 0, 0x05, 0, 0, 0, 0, 0]);
    assert_eq!(device.next_report(0, &mut buf), Ok(None));
}

#[test]
fn rejected_report_records_its_completion_code_surviving_a_later_control_transfer() {
    // When a downstream keyboard is unplugged, on metal the disconnect first
    // surfaces as the device's interrupt-IN transfer faulting. The HCD then
    // issues a hub GET_PORT_STATUS control transfer to confirm — which resets
    // the shared per-transfer event diagnostics. The controller's verdict on
    // the keyboard's *own* endpoint (a transient transaction error vs. a
    // device-gone code) is the datum that decides the correct teardown, so it
    // must be captured at the report fault and survive that confirmation
    // control transfer. This asserts the dedicated `last_report_fault_code`
    // records the rejected code and is not clobbered by a subsequent control
    // transfer (it fails before that field existed, when the code was lost).
    use crate::transport::UrbEngine;
    let mem = shared_mem();
    let mut device = started_device(MockXhci::with_device(&mem), &mem);
    attach_root_device(&mut device, 1).expect("enumeration succeeds");
    assert_eq!(
        device.last_report_fault_code(0),
        0,
        "no report has faulted yet"
    );

    // The next interrupt-IN report posts a completion code the decode rejects
    // (the unplug-style fault).
    let mut buf = [0u8; REPORT_LEN];
    device.host_mut().fault_one_report_completion = Some(CompletionCode::UsbTransactionError);
    assert_eq!(device.next_report(0, &mut buf), Ok(None));
    device
        .host_mut()
        .pending_reports
        .push_back(alloc::vec![0xAA, 0, 0, 0, 0, 0, 0, 0]);
    device.host_mut().process_int_ring();
    assert_eq!(
        device.next_report(0, &mut buf),
        Err(DriverError::DeviceFault)
    );
    assert_eq!(
        device.last_report_fault_code(0),
        CompletionCode::UsbTransactionError.as_u8(),
        "the rejected report's completion code is captured"
    );

    // A subsequent control transfer (standing in for the hub disconnect
    // confirmation the HCD issues next) resets the shared event diagnostics
    // but must leave the report fault code intact.
    let mut descriptor = [0u8; 18];
    let get_device_descriptor = [0x80, 0x06, 0x00, 0x01, 0x00, 0x00, 0x12, 0x00];
    device
        .engine_for(0)
        .control_in(get_device_descriptor, &mut descriptor)
        .expect("the device-descriptor control transfer completes");
    assert_eq!(
        device.last_completion_code(),
        CompletionCode::Success.as_u8(),
        "the control transfer reset the shared diagnostics to its own result"
    );
    assert_eq!(
        device.last_report_fault_code(0),
        CompletionCode::UsbTransactionError.as_u8(),
        "the report fault code survives a later control transfer"
    );
}

#[test]
fn next_report_before_enumeration_fails_closed() {
    let mem = shared_mem();
    let mut device = started_device(MockXhci::with_device(&mem), &mem);
    let mut buf = [0u8; REPORT_LEN];
    assert_eq!(
        device.next_report(0, &mut buf),
        Err(DriverError::DeviceFault)
    );
}

#[test]
fn start_enables_the_interrupter() {
    // The engine's synchronous waits park on the controller interrupt, so
    // starting the controller enables its interrupter as part of
    // `UsbDevice::start` (and every post-reset re-program): the
    // per-interrupter Interrupt Enable is set, moderation is disabled
    // (lowest completion latency), and the global `USBCMD.INTE` is set so a
    // posted event asserts the device's interrupt.
    let mem = shared_mem();
    let mut device = started_device(MockXhci::with_device(&mem), &mem);
    let host = device.host_mut();
    assert_eq!(
        host.iman & regs::IMAN_IE,
        regs::IMAN_IE,
        "interrupter Interrupt Enable is set by start"
    );
    assert_eq!(
        host.imod, 0,
        "interrupt moderation disabled (lowest latency)"
    );
    assert_eq!(
        host.usbcmd & regs::USBCMD_INTE,
        regs::USBCMD_INTE,
        "global Interrupter Enable is set by start"
    );
}

#[test]
fn a_missing_completion_times_out_by_wall_clock_and_parks_instead_of_spinning() {
    // A controller that never posts the awaited completion fails closed
    // once the wall-clock wait budget is spent — reached by *parking* on
    // the event-wait seam (each park advances the deterministic test
    // clock by the granted budget), never by spinning an iteration count.
    // The engine grants each park the whole remaining budget, so the
    // timeout costs a handful of parks, not a poll loop.
    let mem = shared_mem();
    let wait = TestWait::leaked();
    // `MockXhci::new()` carries no device model: a command doorbell rings
    // into silence and no event is ever posted.
    let mut device = started_device_with_wait(MockXhci::new(), &mem, wait);
    let waits_before = wait.waits.get();
    let clock_before = wait.now_us.get();
    assert_eq!(
        device
            .command_for_test(Trb::new(TrbType::EnableSlot, 0, 0, 0))
            .err(),
        Some(DriverError::DeviceFault),
        "a silent controller fails closed"
    );
    assert_eq!(
        device.last_reject_reason(),
        4,
        "the failure is the genuine wait-budget timeout"
    );
    assert!(
        wait.now_us.get() - clock_before >= 5_000_000,
        "the wait spans the whole wall-clock budget before failing"
    );
    let parks = wait.waits.get() - waits_before;
    assert!(parks >= 1, "the engine parked for the missing event");
    assert!(
        parks <= 4,
        "the wait parks with the remaining budget, never spinning: {parks} parks"
    );
}

#[test]
fn an_empty_root_hub_parks_through_the_connect_window_and_serves_nothing() {
    // With nothing connected, the boot-time walk powers the ports, parks
    // through the power-on/attach-debounce window (never spinning), and
    // comes up serving nothing — the "controller up, awaiting the first
    // connect" state, not an error.
    let mem = shared_mem();
    let wait = TestWait::leaked();
    let mut device = started_device_with_wait(MockXhci::new(), &mem, wait);
    let waits_before = wait.waits.get();
    device
        .bring_up(&TestDelay::default())
        .expect("an empty controller comes up serving nothing");
    assert!(!device.any_device_live(), "nothing was enumerated");
    let parks = wait.waits.get() - waits_before;
    assert!(parks >= 1, "the connect debounce parked");
    assert!(
        parks <= 4,
        "the debounce parks with the remaining window, never spinning: {parks} parks"
    );
}

#[test]
fn enable_interrupter_clears_stale_pending_and_global_status_before_arming() {
    // Stale Interrupt Pending and port-change/event latches can be left
    // visible by the firmware hand-off or the discovery path. The enable
    // sequence clears them before arming, so the first real completion
    // produces a fresh controller interrupt.
    let mem = shared_mem();
    let mut xhci = Xhci::open(MockXhci::with_device(&mem)).expect("bring-up succeeds");
    xhci.host.iman = regs::IMAN_IP;
    xhci.host.hse_latched = true;
    xhci.host.eint_latched = true;
    xhci.host.pcd_latched = true;
    xhci.host.status_write_needs_read_flush = true;

    xhci.enable_interrupter().expect("enable interrupter");

    assert_eq!(
        xhci.host.iman & regs::IMAN_IP,
        0,
        "the stale Interrupt Pending was cleared"
    );
    assert_eq!(
        xhci.host.read32(MockXhci::op(regs::USBSTS)).unwrap()
            & (regs::USBSTS_HSE | regs::USBSTS_EINT | regs::USBSTS_PCD),
        0,
        "stale global status was cleared and flushed before arming"
    );
    assert_eq!(
        xhci.host.iman & regs::IMAN_IE,
        regs::IMAN_IE,
        "interrupter is armed after stale status cleanup"
    );
    assert_eq!(
        xhci.host.usbcmd & regs::USBCMD_INTE,
        regs::USBCMD_INTE,
        "global interrupt enable is set after stale status cleanup"
    );
}

#[test]
fn acknowledge_interrupt_clears_global_and_interrupter_pending_and_keeps_enable() {
    // Servicing a delivered interrupt clears `USBSTS.EINT` and `IMAN.IP`
    // before draining the event ring, keeping Interrupt Enable set so the
    // interrupter stays armed for the next completion.
    let mem = shared_mem();
    let mut device = started_device(MockXhci::with_device(&mem), &mem);
    attach_root_device(&mut device, 1).expect("enumeration succeeds");
    // The controller posts an event and sets both interrupt-status latches.
    device.host_mut().eint_latched = true;
    device.host_mut().iman |= regs::IMAN_IP;

    device
        .acknowledge_interrupt()
        .expect("acknowledge interrupt");

    let host = device.host_mut();
    assert_eq!(
        host.read32(MockXhci::op(regs::USBSTS)).unwrap() & regs::USBSTS_EINT,
        0,
        "global Event Interrupt status was cleared"
    );
    assert_eq!(
        host.iman & regs::IMAN_IP,
        0,
        "Interrupt Pending was cleared"
    );
    assert_eq!(
        host.iman & regs::IMAN_IE,
        regs::IMAN_IE,
        "Interrupt Enable stays set after the acknowledge"
    );
}

#[test]
fn acknowledge_clears_ip_only_and_a_zero_event_wake_never_writes_erdp() {
    // The metal symptom this guards against: the controller wakes the URB loop
    // continuously the moment a key is pressed (a self-sustaining interrupt
    // storm), and the keyboard never types. Its cause was a *standalone* ERDP
    // write performed on every interrupt service, including a wake that
    // dequeued nothing. Writing ERDP (with the Event Handler Busy clear bit)
    // while the controller still holds an un-dequeued event — routine on the
    // non-coherent VL805/PCIe path, where the MSI can arrive before the event
    // TRB's DMA write is visible to this PE — tells the controller the ring is
    // drained to a point *behind* its own enqueue, so it re-asserts the
    // interrupt immediately and the loop spins forever.
    //
    // The contract: `acknowledge_interrupt` clears IMAN.IP only and never
    // touches ERDP, and a wake that dequeues nothing performs no ERDP write at
    // all. Event Handler Busy is released solely by the per-event dequeue
    // advance the drain performs (`ack_event`), so ERDP is written only once
    // the controller's event is genuinely consumed — never speculatively.
    let mem = shared_mem();
    let mut device = started_device(MockXhci::with_device(&mem), &mem);
    attach_root_device(&mut device, 1).expect("enumeration succeeds");

    // The controller asserts an interrupt (sets EHB + IP) but the event TRB is
    // not yet visible to this PE: the drain that follows finds nothing.
    device.host_mut().assert_event_interrupt();
    assert!(
        device.host_mut().event_handler_busy,
        "the controller marks the event handler busy on assertion"
    );
    let erdp_before = device.host_mut().erdp[0];

    // Servicing: acknowledge clears IMAN.IP but must NOT write ERDP or clear
    // EHB — a standalone ERDP write on a not-yet-consumed ring is the storm.
    device.acknowledge_interrupt().expect("acknowledge");
    assert_eq!(
        device.host_mut().iman & regs::IMAN_IP,
        0,
        "IP cleared on ack"
    );
    assert!(
        device.host_mut().event_handler_busy,
        "acknowledge must leave EHB set: only the drain advances ERDP"
    );
    assert_eq!(
        device.host_mut().erdp[0],
        erdp_before,
        "acknowledge must not write ERDP"
    );

    // The (empty) drain dequeues nothing: `next_report` only arms a transfer
    // and writes no ERDP — so the controller is given no stale pointer to
    // re-assert on, and the loop does not spin.
    let mut buf = [0u8; REPORT_LEN];
    assert_eq!(device.next_report(0, &mut buf), Ok(None));
    assert_eq!(
        device.host_mut().erdp[0],
        erdp_before,
        "a zero-event wake performs no ERDP write (no storm)"
    );

    // When the real report finally lands, the per-event drain consumes it and
    // its ERDP advance releases EHB, so the next event re-asserts the
    // interrupt — interrupt delivery resumes without any standalone write.
    device
        .host_mut()
        .pending_reports
        .push_back(alloc::vec![0, 0, 0x04, 0, 0, 0, 0, 0]);
    device.host_mut().process_int_ring();
    assert!(matches!(device.next_report(0, &mut buf), Ok(Some(_))));
    assert!(
        !device.host_mut().event_handler_busy,
        "the per-event ERDP advance releases Event Handler Busy"
    );
    device.host_mut().assert_event_interrupt();
    assert_eq!(
        device.host_mut().iman & regs::IMAN_IP,
        regs::IMAN_IP,
        "the next event re-asserts the interrupt once the drain cleared EHB"
    );
}

#[test]
fn a_cycle_owned_but_not_yet_landed_event_is_not_consumed_until_its_body_arrives() {
    // The metal "first key then silent" fault: on the non-coherent BCM2711/
    // VL805 PCIe path the controller's event-TRB write does not reach RAM
    // atomically, so the announcing cycle bit can be visible to this PE while
    // the 16-byte body is still the zeroed initial state. The drain must NOT
    // consume such a phantom: a real event TRB never has type 0, and consuming
    // a cycle-owned but type-0 entry advances the dequeue past the controller's
    // enqueue, permanently desynchronises the consumer cycle, and (because the
    // stray ERDP write leaves the interrupter pointing behind its enqueue)
    // wedges the controller with Event Handler Busy stuck — no further
    // completion interrupts, so only the first keystroke is ever delivered.
    // The entry must be left un-consumed (no ERDP write) and re-read once its
    // body lands.
    let mem = shared_mem();
    let mut device = started_device(MockXhci::with_device(&mem), &mem);
    attach_root_device(&mut device, 1).expect("enumeration succeeds");
    let mut buf = [0u8; REPORT_LEN];
    assert_eq!(device.next_report(0, &mut buf), Ok(None), "arms a transfer");

    // The controller posts the report event (its cycle bit is visible) but its
    // body has not yet reached RAM — the entry reads as cycle-owned, all-zero.
    device
        .host_mut()
        .pending_reports
        .push_back(alloc::vec![0, 0, 0x04, 0, 0, 0, 0, 0]);
    device.host_mut().process_int_ring();
    device.host_mut().unland_last_event();
    let erdp_before = device.host_mut().erdp[0];

    // The drain leaves the not-yet-landed entry alone: no consume, no fault,
    // and crucially no ERDP write (which would desync the ring and wedge EHB).
    assert_eq!(
        device.next_report(0, &mut buf),
        Ok(None),
        "a cycle-owned but zero-body entry is not consumed"
    );
    assert_eq!(
        device.host_mut().erdp[0],
        erdp_before,
        "no ERDP write on a not-yet-landed entry — the controller is not desynced"
    );
    // Once the body lands, the very same entry is consumed normally and the
    // report is delivered.
    device.host_mut().land_last_event();
    assert!(
        matches!(device.next_report(0, &mut buf), Ok(Some(_))),
        "the report is delivered once its body lands"
    );
    assert_ne!(
        device.host_mut().erdp[0],
        erdp_before,
        "the real event advances ERDP (releasing Event Handler Busy)"
    );
}

#[test]
fn controller_faulted_reports_hse_and_halt_and_recovery_clears_it() {
    // A halted/errored controller (USBSTS.HSE or HCHalted) raises no further
    // interrupts until a Host Controller Reset, so a watched device's hot-plug
    // and transfers go silent — the metal "unplug worked but the controller
    // never saw the re-plug" fault. On the Pi 4 the VL805 latches a Host System
    // Error during the downstream-device hot-removal teardown, after its
    // Disable Slot has already completed. The HCD detects this and recovers by
    // resetting and re-enumerating; this verifies the predicate it keys on and
    // that the mandated reset clears the fault.
    let mem = shared_mem();
    let mut device = started_device(MockXhci::with_device(&mem), &mem);
    assert!(
        !device.controller_faulted(),
        "a running, error-free controller is healthy"
    );

    // A latched Host System Error is a fault.
    device.host_mut().hse_latched = true;
    assert!(
        device.controller_faulted(),
        "USBSTS.HSE is a controller fault"
    );

    // The recovery (a full Host Controller Reset plus fresh enumeration) clears
    // the fault and returns the controller to a usable, interrupt-capable state
    // — the same path a cold boot performs.
    let delay = TestDelay::default();
    device
        .reset_and_reenumerate(&delay)
        .expect("reset recovers a faulted controller");
    assert!(
        !device.controller_faulted(),
        "the Host Controller Reset cleared the latched fault"
    );

    // A halted controller (Run/Stop clear → USBSTS.HCHalted) is equally a
    // fault, independent of HSE.
    device.host_mut().usbcmd &= !regs::USBCMD_RUN;
    assert!(
        device.controller_faulted(),
        "USBSTS.HCHalted is a controller fault"
    );
}

#[test]
fn forged_report_residual_fails_closed() {
    let mem = shared_mem();
    let mut device = started_device(MockXhci::with_device(&mem), &mem);
    attach_root_device(&mut device, 1).expect("enumeration succeeds");
    let mut buf = [0u8; REPORT_LEN];
    assert_eq!(device.next_report(0, &mut buf), Ok(None));
    device.host_mut().forge_report_residual = true;
    device
        .host_mut()
        .pending_reports
        .push_back(alloc::vec![0, 0, 0x04, 0, 0, 0, 0, 0]);
    device.host_mut().process_int_ring();
    assert_eq!(
        device.next_report(0, &mut buf),
        Err(DriverError::DeviceFault)
    );
}

#[test]
fn boot_keyboard_decodes_over_the_xhci_transfer_ring() {
    let mem = shared_mem();
    let mut device = started_device(MockXhci::with_device(&mem), &mem);
    attach_root_device(&mut device, 1).expect("enumeration succeeds");
    let mut arm = [0u8; REPORT_LEN];
    assert_eq!(device.next_report(0, &mut arm), Ok(None));
    // Left Shift held plus key usage 0x04 (`A`).
    device
        .host_mut()
        .pending_reports
        .push_back(alloc::vec![0x02, 0, 0x04, 0, 0, 0, 0, 0]);
    device.host_mut().process_int_ring();

    let mut keyboard = BootKeyboard::new(device.engine_for(0));
    let zero = rustos_abi::driver::input::InputEvent {
        kind: rustos_abi::driver::input::InputEventKind::Key,
        reserved0: 0,
        code: 0,
        value: 0,
    };
    let mut events = [zero; 4];
    let drained = keyboard.poll(&mut events).expect("poll succeeds");
    assert_eq!(drained, 2);
    assert_eq!(events[0].code, 0x04, "key press decoded");
    assert_eq!(events[0].value, 1);
    assert_eq!(events[1].code, 0xE1, "left-shift modifier edge");
    assert_eq!(events[1].value, 1);
    assert_eq!(keyboard.poll(&mut events), Ok(0));
}

/// Decode a fixture and return its interfaces, panicking (test-only) on a
/// refusal — the shared happy-path entry for the decode assertions.
fn decoded(buf: &[u8]) -> [Option<InterfaceInfo>; crate::device::MAX_INTERFACES] {
    InterfaceInfo::decode_all(buf).expect("fixture decodes")
}

#[test]
fn interface_info_decodes_and_fails_closed() {
    // The boot-keyboard fixture: config value 1, interface 0, class
    // `0x03_01_01`, and no second interface.
    let interfaces = decoded(&MOCK_CONFIG_DESCRIPTOR);
    let info = interfaces[0].expect("the keyboard interface decodes");
    assert_eq!(info.configuration_value, 1);
    assert_eq!(info.interface_number, 0);
    assert_eq!(info.class24, 0x03_01_01);
    assert!(info.is_servable());
    // A HID interface carries no bulk endpoints.
    assert!(!info.has_bulk_pair());
    assert_eq!((info.bulk_in_dci, info.bulk_out_dci), (0, 0));
    assert_eq!(
        interfaces[1], None,
        "a single-interface device stays single"
    );

    // The mass-storage fixture: class `08:06:50` with the bulk pair at the
    // DCIs its endpoint descriptors report (EP3 IN → 7, EP4 OUT → 8),
    // max packet 512 each — read, never assumed.
    let msd = decoded(&MOCK_MSD_CONFIG_DESCRIPTOR)[0].expect("MSD interface decodes");
    assert_eq!(msd.class24, 0x08_06_50);
    assert!(msd.has_bulk_pair());
    assert_eq!(msd.bulk_in_dci, 7);
    assert_eq!(msd.bulk_in_max_packet, 512);
    assert_eq!(msd.bulk_out_dci, 8);
    assert_eq!(msd.bulk_out_max_packet, 512);

    // Too short to hold the configuration header.
    assert_eq!(
        InterfaceInfo::decode_all(&MOCK_CONFIG_DESCRIPTOR[..8]),
        Err(DriverError::BadMagic)
    );
    // Leading descriptor is not a configuration descriptor.
    let mut wrong_type = MOCK_CONFIG_DESCRIPTOR;
    wrong_type[1] = 0x01;
    assert_eq!(
        InterfaceInfo::decode_all(&wrong_type),
        Err(DriverError::BadMagic)
    );
    // An interface descriptor claiming a length that runs off the end.
    let mut runaway = MOCK_CONFIG_DESCRIPTOR;
    runaway[9] = 0xFF;
    assert_eq!(
        InterfaceInfo::decode_all(&runaway),
        Err(DriverError::BadMagic)
    );
    // A configuration with no interface descriptor at all (only the
    // 9-byte header).
    assert_eq!(
        InterfaceInfo::decode_all(&MOCK_CONFIG_DESCRIPTOR[..9]),
        Err(DriverError::BadMagic)
    );
    // A second interface class is honoured (boot mouse `0x03_01_02`).
    let mut mouse = MOCK_CONFIG_DESCRIPTOR;
    mouse[16] = 0x02;
    assert_eq!(
        decoded(&mouse)[0].expect("mouse decodes").class24,
        0x03_01_02
    );
}

#[test]
fn interface_info_decodes_every_interface_of_a_composite_device() {
    // The composite receiver fixture: interface 0 is the boot keyboard
    // (EP1 IN → DCI 3), interface 1 the boot mouse (EP2 IN → DCI 5), and
    // the trailing alternate setting of interface 1 (EP3) is skipped.
    let interfaces = decoded(&MOCK_COMPOSITE_CONFIG_DESCRIPTOR);
    let keyboard = interfaces[0].expect("the keyboard interface decodes");
    assert_eq!(keyboard.interface_number, 0);
    assert_eq!(keyboard.class24, 0x03_01_01);
    assert_eq!(keyboard.int_dci, 3);
    assert!(keyboard.is_servable());
    let mouse = interfaces[1].expect("the mouse interface decodes");
    assert_eq!(mouse.interface_number, 1);
    assert_eq!(mouse.class24, 0x03_01_02);
    assert_eq!(
        mouse.int_dci, 5,
        "the default setting's EP2, never the alternate setting's EP3"
    );
    assert!(mouse.is_servable());
    assert_eq!(interfaces[2], None, "the alternate setting adds nothing");
}

#[test]
fn interface_info_drops_a_malformed_hid_interface_but_serves_its_sibling() {
    // Interface 0 is a HID interface with **no** interrupt-IN endpoint
    // (malformed — nothing to poll); interface 1 is a well-formed boot
    // mouse. The malformed one is dropped, the sibling still served.
    let config: [u8; 34] = [
        // Configuration: wTotalLength=34, 2 interfaces.
        0x09, 0x02, 0x22, 0x00, 0x02, 0x01, 0x00, 0xA0, 0x32, //
        // Interface 0: HID boot keyboard with no endpoint at all.
        0x09, 0x04, 0x00, 0x00, 0x00, 0x03, 0x01, 0x01, 0x00, //
        // Interface 1: HID boot mouse, EP1 IN (DCI 3).
        0x09, 0x04, 0x01, 0x00, 0x01, 0x03, 0x01, 0x02, 0x00, //
        0x07, 0x05, 0x81, 0x03, 0x04, 0x00, 0x0A,
    ];
    let interfaces = decoded(&config);
    let mouse = interfaces[0].expect("the well-formed sibling is served");
    assert_eq!(mouse.class24, 0x03_01_02);
    assert_eq!(interfaces[1], None);

    // A device whose *only* interface is the malformed HID one has nothing
    // decodable and is rejected whole.
    assert_eq!(
        InterfaceInfo::decode_all(&config[..18]),
        Err(DriverError::BadMagic)
    );
}

#[test]
fn interface_info_bounds_the_decoded_interface_set() {
    // Five interfaces in one configuration: only the first
    // `MAX_INTERFACES` are decoded; the excess is ignored, never trusted.
    let mut config = alloc::vec![
        // Configuration header: wTotalLength=89, 5 interfaces.
        0x09u8, 0x02, 0x59, 0x00, 0x05, 0x01, 0x00, 0xA0, 0x32,
    ];
    for number in 0..5u8 {
        config.extend_from_slice(&[0x09, 0x04, number, 0x00, 0x01, 0x03, 0x01, 0x02, 0x00]);
        config.extend_from_slice(&[0x07, 0x05, 0x81 + number, 0x03, 0x04, 0x00, 0x0A]);
    }
    let interfaces = InterfaceInfo::decode_all(&config).expect("the set decodes");
    assert_eq!(interfaces.iter().flatten().count(), 4);
    assert_eq!(
        interfaces[3]
            .expect("the fourth interface decodes")
            .interface_number,
        3
    );
}

#[test]
fn describe_device_emits_the_hid_child_node() {
    use rustos_abi::{HwDeviceClass, HwMatchKey};
    let mem = shared_mem();
    let mut device = started_device(MockXhci::with_device(&mem), &mem);
    attach_root_device(&mut device, 1).expect("enumeration succeeds");

    // The emitted child node carries the device's vid:pid and the
    // *interface* class read from the configuration descriptor
    // (`0x03_01_01`), parented at the controller node and assigned the
    // tree owner's id.
    let node = device.describe_device(0, 7, 9).expect("identity captured");
    assert_eq!(node.id(), 9);
    assert_eq!(node.parent(), 7);
    assert_ne!(node.address(), 0, "the node names its device's slot");
    assert_eq!(node.class(), Some(HwDeviceClass::Input));
    assert_eq!(node.match_keys().len(), 1);
    let emitted = node.match_keys()[0];
    assert_eq!(emitted, HwMatchKey::usb(0x046D, 0xC077, 0x03_01_01));

    // A HID boot-keyboard class bind key (HID class `0x03_01_01`, the key the
    // `usb_kbd` class driver carries) resolves against the emitted node by
    // class (vendor/product wildcard), exactly as `devmgr` will. Constructed
    // inline so this protocol crate does not depend on a concrete driver.
    let keyboard_key = HwMatchKey::usb(0, 0, 0x03_01_01);
    assert!(keyboard_key.matches(&emitted));
    // A boot-mouse bind key (HID class `0x03_01_02`) must not bind a keyboard
    // interface.
    let mouse_key = HwMatchKey::usb(0, 0, 0x03_01_02);
    assert!(!mouse_key.matches(&emitted));
}

#[test]
fn describe_device_before_enumeration_fails_closed() {
    let mem = shared_mem();
    let device = started_device(MockXhci::with_device(&mem), &mem);
    // No device enumerated yet: the identity is absent, so the bus
    // refuses to fabricate a node.
    assert_eq!(
        device.describe_device(0, 7, 9).err(),
        Some(DriverError::NotFound)
    );
}

/// Bring up a directly-attached mass-storage device on root port 1,
/// asserting its identity so every bulk test starts from a proven
/// enumeration.
fn started_msd(mem: &SharedMem) -> UsbDevice<'static, MockXhci, MockDma> {
    let mut device = started_device(MockXhci::with_msd_device(mem), mem);
    let index = attach_root_device(&mut device, 1).expect("the MSD enumerates");
    let identity = device.device_identity(index).expect("identity captured");
    assert_eq!(identity.vendor_id, 0x0781);
    assert_eq!(identity.product_id, 0x5567);
    device
}

#[test]
fn enumerating_a_mass_storage_device_configures_its_bulk_endpoint_pair() {
    use rustos_abi::{HwDeviceClass, HwMatchKey};
    let mem = shared_mem();
    let mut device = started_msd(&mem);

    // The controller was told about both bulk endpoints at the DCIs the
    // descriptor reports (EP3 IN → 7, EP4 OUT → 8 — never assumed), and
    // the device reached the configured state.
    assert_eq!(device.host_mut().bulk_in.dci, 7);
    assert_eq!(device.host_mut().bulk_out.dci, 8);
    assert!(device.host_mut().configured);

    // The emitted node is an honest storage node carrying the interface's
    // real class triple, so a mass-storage class driver's bind key
    // (`08:06:50`, vendor/product wildcard) resolves against it.
    let node = device.describe_device(0, 7, 9).expect("identity captured");
    assert_eq!(node.class(), Some(HwDeviceClass::Storage));
    let emitted = node.match_keys()[0];
    assert_eq!(emitted, HwMatchKey::usb(0x0781, 0x5567, 0x08_06_50));
    assert!(HwMatchKey::usb(0, 0, 0x08_06_50).matches(&emitted));
}

#[test]
fn bulk_out_transfers_deliver_the_bytes_to_the_device() {
    let mem = shared_mem();
    let mut device = started_msd(&mem);

    let payload = alloc::vec![0x5Au8; 24];
    let slot = device
        .queue_bulk_out(0, OUT_PIPE, &payload)
        .expect("TD queues");
    assert_eq!(slot, 0);

    // The mock device consumed the TD at the doorbell and captured the
    // bytes; the completion reports every byte accepted.
    assert_eq!(device.host_mut().bulk_out_received, alloc::vec![payload]);
    let complete = device
        .poll_bulk(0, &mut [])
        .expect("poll succeeds")
        .expect("a completion is pending");
    assert_eq!(complete.pipe, OUT_PIPE);
    assert_eq!(complete.slot, 0);
    assert_eq!(complete.result, Ok(24));
    // Nothing further is pending.
    assert_eq!(device.poll_bulk(0, &mut []), Ok(None));
}

#[test]
fn bulk_in_transfers_land_the_devices_bytes_and_report_short_packets_honestly() {
    let mem = shared_mem();
    let mut device = started_msd(&mem);

    // The device answers the 64-byte read with only 10 bytes (a short
    // packet, e.g. a short SCSI response).
    let response = alloc::vec![0xA7u8; 10];
    device
        .host_mut()
        .bulk_in_responses
        .push_back(response.clone());
    device.queue_bulk_in(0, IN_PIPE, 64).expect("TD queues");

    let mut buf = [0u8; 64];
    let complete = device
        .poll_bulk(0, &mut buf)
        .expect("poll succeeds")
        .expect("a completion is pending");
    assert_eq!(complete.pipe, IN_PIPE);
    assert_eq!(complete.result, Ok(10));
    assert_eq!(&buf[..10], &response[..]);
}

#[test]
fn several_bulk_tds_queue_per_direction_and_complete_in_order() {
    let mem = shared_mem();
    let mut device = started_msd(&mem);

    // Three reads with distinct payloads, queued before any is reaped.
    for byte in [0x11u8, 0x22, 0x33] {
        device
            .host_mut()
            .bulk_in_responses
            .push_back(alloc::vec![byte; 8]);
    }
    for expected_slot in 0..3 {
        let slot = device.queue_bulk_in(0, IN_PIPE, 8).expect("TD queues");
        assert_eq!(slot, expected_slot);
    }

    // Completions arrive in submission order, each with its own bytes.
    for (expected_slot, byte) in [(0usize, 0x11u8), (1, 0x22), (2, 0x33)] {
        let mut buf = [0u8; 8];
        let complete = device
            .poll_bulk(0, &mut buf)
            .expect("poll succeeds")
            .expect("a completion is pending");
        assert_eq!(complete.pipe, IN_PIPE);
        assert_eq!(complete.slot, expected_slot);
        assert_eq!(complete.result, Ok(8));
        assert_eq!(buf, [byte; 8]);
    }
    assert_eq!(device.poll_bulk(0, &mut []), Ok(None));
}

#[test]
fn decode_all_captures_a_second_bulk_pair_for_uas_pipes() {
    // A UAS-shaped interface: class 08:06:62 with two bulk endpoints per
    // direction. The decoder captures both pairs so all four pipes can be
    // configured; a BOT-shaped interface leaves the second pair absent.
    let mut config = alloc::vec![
        9u8, 2, 0, 0, 1, 1, 0, 0x80, 50, // configuration header
        9, 4, 0, 0, 4, 0x08, 0x06, 0x62, 0, // interface, four endpoints
        7, 5, 0x01, 0x02, 0, 2, 0, // bulk-OUT EP1 (command)
        7, 5, 0x82, 0x02, 0, 2, 0, // bulk-IN EP2 (status)
        7, 5, 0x83, 0x02, 0, 2, 0, // bulk-IN EP3 (data-in)
        7, 5, 0x04, 0x02, 0, 2, 0, // bulk-OUT EP4 (data-out)
    ];
    let total = u16::try_from(config.len()).expect("fits");
    config[2..4].copy_from_slice(&total.to_le_bytes());
    let interfaces = InterfaceInfo::decode_all(&config).expect("decodes");
    let iface = interfaces[0].expect("one interface");
    assert_eq!(iface.bulk_out_dci, 2); // EP1 OUT
    assert_eq!(iface.bulk_in_dci, 5); // EP2 IN
    assert_eq!(iface.bulk_in2_dci, 7); // EP3 IN
    assert_eq!(iface.bulk_out2_dci, 8); // EP4 OUT
}

#[test]
fn control_out_data_stage_reaches_the_device() {
    // The CBI ADSC path end to end through the engine: the command block
    // is staged through the control data buffer and delivered to the
    // device's control endpoint as an OUT data stage.
    let mem = shared_mem();
    let mut device = started_msd(&mem);
    let block = [0x28u8, 0, 0, 0, 0, 9, 0, 0, 1, 0, 0, 0];
    let setup = [0x21, 0x00, 0, 0, 0, 0, 12, 0];
    {
        let mut engine = device.engine_for(0);
        crate::transport::UrbEngine::control_out(&mut engine, setup, &block)
            .expect("the block is delivered");
    }
    assert_eq!(device.host_mut().adsc_blocks, alloc::vec![block.to_vec()]);
}

#[test]
fn a_stalled_control_out_is_surfaced_and_the_endpoint_recovered() {
    // A refused class request (the CBI "command not accepted" answer):
    // the STALL surfaces distinctly, and EP0 is recovered in place so the
    // very next control transfer serves.
    let mem = shared_mem();
    let mut device = started_msd(&mem);
    // An unmodelled request: the mock's generic arm STALLs and halts EP0.
    let refused = [0x21, 0xDE, 0, 0, 0, 0, 4, 0];
    {
        let mut engine = device.engine_for(0);
        assert_eq!(
            crate::transport::UrbEngine::control_out(&mut engine, refused, &[1, 2, 3, 4]),
            Err(DriverError::EndpointStalled)
        );
    }
    // The recovery ran: a follow-up control-IN (a device-descriptor read)
    // still serves on the rebuilt EP0 ring.
    let mut data = [0u8; 18];
    {
        let mut engine = device.engine_for(0);
        let n = crate::transport::UrbEngine::control_in(
            &mut engine,
            [0x80, 0x06, 0x00, 0x01, 0x00, 0x00, 18, 0],
            &mut data,
        )
        .expect("EP0 serves after the recovery");
        assert_eq!(n, 18);
    }
}

#[test]
fn a_full_bulk_ring_refuses_further_tds_and_bounds_the_queue() {
    let mem = shared_mem();
    let mut device = started_msd(&mem);

    // With no responses scripted, every queued TD stays in flight. The
    // ring holds `BULK_SLOTS - 1` TDs (one slot stays free to distinguish
    // full from empty); the next queue is refused, never wrapped over.
    for _ in 0..BULK_SLOTS - 1 {
        device.queue_bulk_in(0, IN_PIPE, 8).expect("TD queues");
    }
    assert_eq!(
        device.queue_bulk_in(0, IN_PIPE, 8).err(),
        Some(DriverError::Busy)
    );
    assert_eq!(
        device.bulk_in_flight(0, IN_PIPE),
        BULK_SLOTS - 1,
        "every accepted TD stays accounted"
    );

    // An oversize TD is refused before any staging is touched.
    assert_eq!(
        device.queue_bulk_in(0, IN_PIPE, BULK_BUF_LEN + 1).err(),
        Some(DriverError::LengthOutOfRange)
    );
}

#[test]
fn a_bulk_stall_recovers_the_endpoint_and_answers_every_queued_td() {
    let mem = shared_mem();
    let mut device = started_msd(&mem);

    // Two reads are in flight when the device STALLs the first.
    device.host_mut().bulk_in.stall_next = true;
    device
        .queue_bulk_in(0, IN_PIPE, 8)
        .expect("first TD queues");
    device
        .queue_bulk_in(0, IN_PIPE, 8)
        .expect("second TD queues");

    // The stalled TD surfaces the distinct per-transfer stall, and the
    // recovery ran in-line: Reset Endpoint → Set TR Dequeue Pointer →
    // CLEAR_FEATURE(ENDPOINT_HALT), leaving the mock endpoint running.
    let mut buf = [0u8; 8];
    let complete = device
        .poll_bulk(0, &mut buf)
        .expect("poll succeeds")
        .expect("the stalled TD completes");
    assert_eq!(complete.slot, 0);
    assert_eq!(complete.result, Err(DriverError::EndpointStalled));
    assert_eq!(device.host_mut().bulk_in.halt, 0, "endpoint recovered");

    // The TD the halt abandoned is answered too — never silently lost.
    let aborted = device
        .poll_bulk(0, &mut buf)
        .expect("poll succeeds")
        .expect("the abandoned TD is answered");
    assert_eq!(aborted.slot, 1);
    assert_eq!(aborted.result, Err(DriverError::EndpointStalled));

    // The recovered endpoint serves fresh transfers immediately.
    device
        .host_mut()
        .bulk_in_responses
        .push_back(alloc::vec![0x77u8; 8]);
    device
        .queue_bulk_in(0, IN_PIPE, 8)
        .expect("fresh TD queues");
    let fresh = device
        .poll_bulk(0, &mut buf)
        .expect("poll succeeds")
        .expect("the fresh TD completes");
    assert_eq!(fresh.result, Ok(8));
    assert_eq!(buf, [0x77u8; 8]);
}

#[test]
fn a_downstream_msd_stall_recovery_targets_the_device_never_the_hub() {
    // A storage stick behind the onboard hub (the Pi topology): at rest the
    // hub is the active control context, so the recovery's
    // CLEAR_FEATURE(ENDPOINT_HALT) must switch to the device's own EP0 — the
    // mock STALLs a clear wrongly issued to the hub, so a mistargeted
    // recovery fails this test loudly.
    let mem = shared_mem();
    let mut mock = MockXhci::with_hub(&mem, 4, 2);
    mock.msd_device = true;
    let mut device = started_device(mock, &mem);
    let delay = TestDelay::default();
    device.bring_up(&delay).expect("bring-up runs");
    // The root hub holds entry 0's region, so the stick is index 1.
    let descriptor = device
        .device_identity(1)
        .expect("a downstream device is enumerated");
    assert_eq!(descriptor.vendor_id, 0x0781);

    device.host_mut().bulk_out.stall_next = true;
    device
        .queue_bulk_out(1, OUT_PIPE, &[0xE1u8; 4])
        .expect("TD queues");
    let complete = device
        .poll_bulk(1, &mut [])
        .expect("poll succeeds")
        .expect("the stalled TD completes");
    assert_eq!(complete.result, Err(DriverError::EndpointStalled));
    assert_eq!(device.host_mut().bulk_out.halt, 0, "endpoint recovered");
    // The clear reached the device's EP0 (a mistargeted one STALLs and
    // halts EP0 in the mock), and the hub watch survived the recovery.
    assert!(!device.host_mut().ep0_halted, "EP0 was never mistargeted");
    assert!(device.hub_watch_active(), "the hub watch keeps its ring");

    // And the recovered endpoint accepts a fresh transfer end to end.
    device
        .queue_bulk_out(1, OUT_PIPE, &[0xE2u8; 4])
        .expect("fresh TD queues");
    let fresh = device
        .poll_bulk(1, &mut [])
        .expect("poll succeeds")
        .expect("the fresh TD completes");
    assert_eq!(fresh.result, Ok(4));
}

#[test]
fn urb_engine_bulk_serves_the_configured_endpoints_and_rejects_others() {
    use crate::transport::UrbEngine;
    let mem = shared_mem();
    let mut device = started_msd(&mem);

    // A bulk URB naming an endpoint that is not the configured one in its
    // direction is refused before any ring is touched.
    let mut buf = [0u8; 8];
    assert_eq!(
        UrbEngine::bulk_in(&mut device.engine_for(0), 1, &mut buf).err(),
        Some(DriverError::OutOfRange)
    );
    assert_eq!(
        UrbEngine::bulk_out(&mut device.engine_for(0), 3, &buf).err(),
        Some(DriverError::OutOfRange)
    );

    // The right endpoints serve the arm-then-reap URB shape: the first
    // drive arms (still in flight), the next reaps the completion.
    device
        .host_mut()
        .bulk_in_responses
        .push_back(alloc::vec![0x42u8; 8]);
    assert_eq!(
        UrbEngine::bulk_in(&mut device.engine_for(0), 3, &mut buf),
        Ok(None)
    );
    assert_eq!(
        UrbEngine::bulk_in(&mut device.engine_for(0), 3, &mut buf),
        Ok(Some(8))
    );
    assert_eq!(buf, [0x42u8; 8]);

    assert_eq!(
        UrbEngine::bulk_out(&mut device.engine_for(0), 4, &[0x9Cu8; 6]),
        Ok(None)
    );
    assert_eq!(
        UrbEngine::bulk_out(&mut device.engine_for(0), 4, &[0x9Cu8; 6]),
        Ok(Some(6))
    );
    assert_eq!(
        device.host_mut().bulk_out_received.last(),
        Some(&alloc::vec![0x9Cu8; 6])
    );
}

/// `C_PORT_CONNECTION` (USB 2.0 §11.24.2.7.2.1) — the connect-status-change
/// bit a hub latches in `wPortChange`, which the watch reads and clears.
const PORT_CHANGE_CONNECTION: u16 = 1 << 0;

#[test]
fn hub_watch_arms_after_enumerating_through_a_hub() {
    // Reaching the keyboard through the onboard hub arms the hub's
    // status-change endpoint, so a later downstream connect/disconnect is
    // delivered event-driven rather than polled.
    let mem = shared_mem();
    let mut mock = MockXhci::with_hub(&mem, 4, 4);
    mock.hub_downstream_status = 1 << 0;
    let mut device = started_device(mock, &mem);
    let delay = TestDelay::default();

    device
        .bring_up(&delay)
        .expect("the keyboard behind the hub is reached");
    assert!(
        device.hub_watch_active(),
        "the hub status-change watch is armed once a hub is descended"
    );
    // With no change pending, servicing the watch is a no-op (it parks on the
    // controller interrupt, never polling).
    assert_eq!(device.next_hub_change(&delay), Ok(HubEvent::None));
}

#[test]
fn enumeration_drains_every_port_change_latch_so_the_hub_watch_stays_quiet() {
    // Real hubs latch a Reset-change (`wPortChange` bit 4) when a downstream
    // port is reset during enumeration, alongside the connect change. The hub
    // keeps its status-change endpoint asserting a report for that port until
    // *every* latched change is cleared. Clearing only the connect change
    // leaves the reset change latched, so the freshly-armed watch fires
    // immediately and forever on a stale change — drowning/faulting the
    // keyboard's reports. This is the metal regression: enumeration must drain
    // the whole change set so the watch goes quiet until a real hot-plug.
    let mem = shared_mem();
    let mut mock = MockXhci::with_hub(&mem, 4, 4);
    mock.hub_downstream_status = 1 << 0;
    mock.hub_downstream_change = PORT_CHANGE_CONNECTION;
    let mut device = started_device(mock, &mem);
    let delay = TestDelay::default();

    device
        .bring_up(&delay)
        .expect("the keyboard behind the hub is reached");

    // Enumeration reset the downstream port (latching the Reset-change) and
    // must have drained both that and the connect change, so nothing remains
    // for the status-change endpoint to report.
    assert_eq!(
        device.host_mut().hub_downstream_change,
        0,
        "enumeration must clear every port-change latch, not just connect"
    );

    // A status-change report with no genuine change pending is a no-op: the
    // watch fabricates neither a connect nor a disconnect, and leaves the port
    // clear (no re-arm storm).
    device.host_mut().post_hub_status_change(&[1 << 4]);
    assert_eq!(device.next_hub_change(&delay), Ok(HubEvent::None));
    assert_eq!(device.host_mut().hub_downstream_change, 0);
    assert!(
        device.device_live(1),
        "the keyboard stays enumerated through a spurious status-change report"
    );
}

#[test]
fn hub_watch_retracts_a_disconnected_downstream_device() {
    let mem = shared_mem();
    let mut mock = MockXhci::with_hub(&mem, 4, 4);
    mock.hub_downstream_status = 1 << 0;
    let mut device = started_device(mock, &mem);
    let delay = TestDelay::default();

    device
        .bring_up(&delay)
        .expect("the keyboard behind the hub is reached");
    assert_eq!(
        device.raw_device_slot(1),
        2,
        "the keyboard occupies the second slot"
    );

    // Unplug the keyboard: its hub port now reads disconnected with the
    // connect-status change latched, and the hub posts a status-change report
    // naming downstream port 4 (bit 4 of the change bitmap).
    device.host_mut().hub_downstream_status = 0;
    device.host_mut().hub_downstream_change = PORT_CHANGE_CONNECTION;
    device.host_mut().post_hub_status_change(&[1 << 4]);

    assert_eq!(
        device.next_hub_change(&delay),
        Ok(HubEvent::Detached(1)),
        "the disconnected downstream device is detected"
    );
    assert!(!device.device_live(1), "its device slot was freed");
    assert!(
        device.hub_watch_active(),
        "the controller and its hub watch stay up after a detach"
    );
}

#[test]
fn a_stray_controller_event_during_a_hub_poll_never_silences_the_watch() {
    // The decisive "controller goes quiet after the first report" metal
    // symptom: while a keyboard sits behind the (integrated) hub, a stray
    // controller event the engine does not model lands on the shared event
    // ring ahead of the hub's status-change completion. `poll_hub_completion`
    // used to fault on it, so `next_hub_change` returned its `?` error before
    // re-arming the status-change endpoint — leaving it with no outstanding
    // transfer, so the hub could never post another report and every later
    // disconnect/reconnect went unseen. The opportunistic poll must instead
    // DRAIN such an event and keep scanning, so the watch is never silenced.
    let mem = shared_mem();
    let mut mock = MockXhci::with_hub(&mem, 4, 4);
    mock.hub_downstream_status = 1 << 0;
    let mut device = started_device(mock, &mem);
    let delay = TestDelay::default();

    device
        .bring_up(&delay)
        .expect("the keyboard behind the hub is reached");
    assert!(device.hub_watch_active());
    assert!(device.device_live(1));

    // A stray controller event (a Host Controller Event, raw TRB-type 37 —
    // not a transfer/command this poll tracks, not a port-status-change) lands
    // ahead of the hub's status-change completion, which carries no genuine
    // port change.
    device.host_mut().post_event_raw_type(0xDEAD, 37);
    device.host_mut().post_hub_status_change(&[1 << 4]);

    // The stray event is drained, the hub completion is still found, and the
    // (no-change) report is serviced quietly. Before the fix this returned
    // `Err` and silenced the watch.
    assert_eq!(device.next_hub_change(&delay), Ok(HubEvent::None));
    assert!(
        device.hub_watch_active(),
        "the watch survived the stray event"
    );
    assert!(
        device.device_live(1),
        "the keyboard stays enumerated through a stray controller event"
    );

    // A genuine later disconnect is still detected — proof the watch was never
    // silenced by the earlier stray event.
    device.host_mut().hub_downstream_status = 0;
    device.host_mut().hub_downstream_change = PORT_CHANGE_CONNECTION;
    device.host_mut().post_hub_status_change(&[1 << 4]);
    assert_eq!(
        device.next_hub_change(&delay),
        Ok(HubEvent::Detached(1)),
        "the disconnect is still seen after the stray event was tolerated"
    );
    assert!(!device.device_live(1), "its device slot was freed");
    assert!(device.hub_watch_active());
}

#[test]
fn faulted_downstream_report_can_confirm_and_detach_a_gone_device() {
    let mem = shared_mem();
    let mut mock = MockXhci::with_hub(&mem, 4, 4);
    mock.hub_downstream_status = 1 << 0;
    let mut device = started_device(mock, &mem);
    let delay = TestDelay::default();

    device
        .bring_up(&delay)
        .expect("the keyboard behind the hub is reached");
    let mut buf = [0u8; REPORT_LEN];
    device.host_mut().fault_one_report_completion = Some(CompletionCode::StallError);
    assert_eq!(device.next_report(1, &mut buf), Ok(None));
    device
        .host_mut()
        .pending_reports
        .push_back(alloc::vec![0xAA, 0, 0, 0, 0, 0, 0, 0]);
    device.host_mut().process_int_ring();

    device.host_mut().hub_downstream_status = 0;
    assert_eq!(
        device.next_report(1, &mut buf),
        Err(DriverError::DeviceFault)
    );
    assert_eq!(device.detach_if_device_gone(1), Ok(true));
    assert!(!device.device_live(1), "the vanished device slot was freed");
    assert!(
        device.hub_watch_active(),
        "the hub watch remains armed for a later reattach"
    );
}

#[test]
fn fault_driven_detach_rearms_a_stashed_hub_change_for_reattach() {
    let mem = shared_mem();
    let mut mock = MockXhci::with_hub(&mem, 4, 4);
    mock.hub_downstream_status = 1 << 0;
    let mut device = started_device(mock, &mem);
    let delay = TestDelay::default();

    device
        .bring_up(&delay)
        .expect("the keyboard behind the hub is reached");
    let mut buf = [0u8; REPORT_LEN];
    device.host_mut().fault_one_report_completion = Some(CompletionCode::StallError);
    assert_eq!(device.next_report(1, &mut buf), Ok(None));
    device.host_mut().hub_downstream_status = 0;
    device.host_mut().hub_downstream_change = PORT_CHANGE_CONNECTION;
    device.host_mut().post_hub_status_change(&[1 << 4]);
    device
        .host_mut()
        .pending_reports
        .push_back(alloc::vec![0xAA, 0, 0, 0, 0, 0, 0, 0]);
    device.host_mut().process_int_ring();

    assert_eq!(
        device.next_report(1, &mut buf),
        Err(DriverError::DeviceFault)
    );
    assert_eq!(device.detach_if_device_gone(1), Ok(true));

    assert_eq!(device.next_hub_change(&delay), Ok(HubEvent::None));
    device.host_mut().hub_downstream_status = 1 << 0;
    device.host_mut().hub_downstream_change = PORT_CHANGE_CONNECTION;
    device.host_mut().post_hub_status_change(&[1 << 4]);
    match device.next_hub_change(&delay) {
        Ok(HubEvent::Attached(index)) => {
            let identity = device
                .device_identity(index)
                .expect("the attached device is served");
            assert_eq!(identity.vendor_id, 0x046D);
            assert_eq!(identity.product_id, 0xC077);
        }
        other => panic!("expected a fresh attach after re-arming the hub watch, got {other:?}"),
    }
}

#[test]
fn trailing_freed_slot_transfer_event_is_drained_not_faulted() {
    let mem = shared_mem();
    let mut mock = MockXhci::with_hub(&mem, 4, 4);
    mock.hub_downstream_status = 1 << 0;
    let mut device = started_device(mock, &mem);
    let delay = TestDelay::default();

    device
        .bring_up(&delay)
        .expect("the keyboard behind the hub is reached");
    let freed_slot = device.raw_device_slot(1);
    assert!(freed_slot != 0, "the keyboard enumerated on a real slot");

    // The unplug faults the device's interrupt-IN transfer; the fault path
    // confirms the downstream port is gone and frees the device slot.
    let mut buf = [0u8; REPORT_LEN];
    device.host_mut().fault_one_report_completion = Some(CompletionCode::StallError);
    assert_eq!(device.next_report(1, &mut buf), Ok(None));
    device.host_mut().hub_downstream_status = 0;
    device
        .host_mut()
        .pending_reports
        .push_back(alloc::vec![0xAA, 0, 0, 0, 0, 0, 0, 0]);
    device.host_mut().process_int_ring();
    assert_eq!(
        device.next_report(1, &mut buf),
        Err(DriverError::DeviceFault)
    );
    assert_eq!(device.detach_if_device_gone(1), Ok(true));

    // The controller now posts a *trailing* transfer completion still addressed
    // to the just-freed device slot — ahead of the hub's disconnect
    // status-change report on the shared event ring. Before the fix this
    // matched no live endpoint and faulted the hub watch.
    device.host_mut().post_transfer_event_for_slot(
        0x4242,
        CompletionCode::StallError,
        3,
        0,
        freed_slot,
    );
    device.host_mut().hub_downstream_change = PORT_CHANGE_CONNECTION;
    device.host_mut().post_hub_status_change(&[1 << 4]);

    // The stale event is drained, not faulted: the hub change is serviced
    // quietly (the device is already gone) and the watch stays armed.
    assert_eq!(device.next_hub_change(&delay), Ok(HubEvent::None));
    assert!(
        device.hub_watch_active(),
        "the hub watch survived the stale event and is armed for a reconnect"
    );

    // A genuine reconnect still enumerates a brand-new device on a fresh slot.
    device.host_mut().hub_downstream_status = 1 << 0;
    device.host_mut().hub_downstream_change = PORT_CHANGE_CONNECTION;
    device.host_mut().post_hub_status_change(&[1 << 4]);
    match device.next_hub_change(&delay) {
        Ok(HubEvent::Attached(index)) => {
            let identity = device
                .device_identity(index)
                .expect("the attached device is served");
            assert_eq!(identity.vendor_id, 0x046D);
            assert_eq!(identity.product_id, 0xC077);
        }
        other => panic!("expected a fresh attach after draining the stale event, got {other:?}"),
    }
    // Once the fresh device owns its slot the freed-slot tolerance is cleared.
    assert!(device.device_live(1));
}

#[test]
fn fault_driven_detach_leaves_unposted_hub_latch_for_rearm() {
    let mem = shared_mem();
    let mut mock = MockXhci::with_hub(&mem, 4, 4);
    mock.hub_downstream_status = 1 << 0;
    let mut device = started_device(mock, &mem);
    let delay = TestDelay::default();

    device
        .bring_up(&delay)
        .expect("the keyboard behind the hub is reached");
    let mut buf = [0u8; REPORT_LEN];
    device.host_mut().fault_one_report_completion = Some(CompletionCode::StallError);
    assert_eq!(device.next_report(1, &mut buf), Ok(None));
    device.host_mut().hub_downstream_status = 0;
    device.host_mut().hub_downstream_change = PORT_CHANGE_CONNECTION;
    device
        .host_mut()
        .pending_reports
        .push_back(alloc::vec![0xAA, 0, 0, 0, 0, 0, 0, 0]);
    device.host_mut().process_int_ring();

    assert_eq!(
        device.next_report(1, &mut buf),
        Err(DriverError::DeviceFault)
    );
    assert_eq!(device.detach_if_device_gone(1), Ok(true));
    assert_eq!(
        device.host_mut().hub_downstream_change,
        PORT_CHANGE_CONNECTION,
        "the hub latch stays set until the status endpoint reports it"
    );

    device.host_mut().post_hub_status_change(&[1 << 4]);
    assert_eq!(device.next_hub_change(&delay), Ok(HubEvent::None));
    assert_eq!(device.host_mut().hub_downstream_change, 0);

    device.host_mut().hub_downstream_status = 1 << 0;
    device.host_mut().hub_downstream_change = PORT_CHANGE_CONNECTION;
    device.host_mut().post_hub_status_change(&[1 << 4]);
    match device.next_hub_change(&delay) {
        Ok(HubEvent::Attached(index)) => {
            let identity = device
                .device_identity(index)
                .expect("the attached device is served");
            assert_eq!(identity.vendor_id, 0x046D);
            assert_eq!(identity.product_id, 0xC077);
        }
        other => panic!("expected a fresh attach after the delayed hub re-arm, got {other:?}"),
    }
}

#[test]
fn live_downstream_report_fault_is_not_misclassified_as_detach() {
    let mem = shared_mem();
    let mut mock = MockXhci::with_hub(&mem, 4, 4);
    mock.hub_downstream_status = 1 << 0;
    let mut device = started_device(mock, &mem);
    let delay = TestDelay::default();

    device
        .bring_up(&delay)
        .expect("the keyboard behind the hub is reached");
    let mut buf = [0u8; REPORT_LEN];
    device.host_mut().fault_one_report_completion = Some(CompletionCode::StallError);
    assert_eq!(device.next_report(1, &mut buf), Ok(None));
    device
        .host_mut()
        .pending_reports
        .push_back(alloc::vec![0xAA, 0, 0, 0, 0, 0, 0, 0]);
    device.host_mut().process_int_ring();

    assert_eq!(
        device.next_report(1, &mut buf),
        Err(DriverError::DeviceFault)
    );
    assert_eq!(device.detach_if_device_gone(1), Ok(false));
    assert!(
        device.device_live(1),
        "a live device's transfer fault remains a report fault"
    );
}

#[test]
fn split_transaction_fault_detaches_without_a_hub_status_confirmation() {
    // The metal case: a low/full-speed keyboard hangs off a hub that stays
    // plugged in, so on unplug the hub's downstream port keeps reading
    // connected and a hub `GET_PORT_STATUS` confirmation is unreliable (it
    // times out). The disconnect surfaces *only* as the keyboard's own
    // interrupt-IN transfer faulting with a Split Transaction Error (the hub's
    // transaction translator can no longer reach the gone device). That code is
    // conclusive on its own, so the device slot must be freed directly —
    // without depending on the hub confirmation, which here would (wrongly)
    // report the port still connected and leave the device wedged forever.
    let mem = shared_mem();
    let mut mock = MockXhci::with_hub(&mem, 4, 4);
    mock.hub_downstream_status = 1 << 0;
    let mut device = started_device(mock, &mem);
    let delay = TestDelay::default();

    device
        .bring_up(&delay)
        .expect("the keyboard behind the hub is reached");

    let mut buf = [0u8; REPORT_LEN];
    device.host_mut().fault_one_report_completion = Some(CompletionCode::SplitTransactionError);
    assert_eq!(device.next_report(1, &mut buf), Ok(None));
    device
        .host_mut()
        .pending_reports
        .push_back(alloc::vec![0xAA, 0, 0, 0, 0, 0, 0, 0]);
    device.host_mut().process_int_ring();
    assert_eq!(
        device.next_report(1, &mut buf),
        Err(DriverError::DeviceFault)
    );
    assert_eq!(
        device.last_report_fault_code(1),
        CompletionCode::SplitTransactionError.as_u8(),
        "the keyboard endpoint's device-gone code is captured"
    );

    // The hub's downstream port is deliberately left reading connected: the fix
    // must NOT depend on the hub confirmation. Before the fix this returned
    // Ok(false) (hub says connected) and the device was never freed.
    assert_eq!(device.detach_if_device_gone(1), Ok(true));
    assert!(!device.device_live(1), "the gone device's slot was freed");
    assert!(
        device.hub_watch_active(),
        "the hub watch stays armed for the re-plug"
    );
    assert_eq!(
        device.last_report_fault_code(1),
        0,
        "the acted-on fault code is cleared so a re-plug is not re-detached"
    );

    // Re-plug: the hub posts a connect change and the device re-enumerates on a
    // fresh slot.
    device.host_mut().hub_downstream_change = PORT_CHANGE_CONNECTION;
    device.host_mut().post_hub_status_change(&[1 << 4]);
    match device.next_hub_change(&delay) {
        Ok(HubEvent::Attached(index)) => {
            let identity = device
                .device_identity(index)
                .expect("the attached device is served");
            assert_eq!(identity.vendor_id, 0x046D);
            assert_eq!(identity.product_id, 0xC077);
        }
        other => {
            panic!("expected a fresh attach after the split-transaction detach, got {other:?}")
        }
    }
    assert!(
        device.device_live(1),
        "the re-plugged keyboard is live again"
    );
}

#[test]
fn split_transaction_detach_frees_the_slot_even_when_disable_is_never_confirmed() {
    // The decisive metal case (matching the captured log): the keyboard's
    // interrupt-IN endpoint faults with a Split Transaction Error AND the
    // controller never lets the Disable Slot command complete — the gone
    // device's hub cannot acknowledge it, so the teardown's command wait times
    // out. The teardown must still free the slot *locally* (best-effort), or
    // `device_slot` stays set, `process_hub_change` ignores the re-plug connect
    // (it enumerates only when no device is tracked), and the keyboard is never
    // re-detected — exactly the "no log on re-plug" symptom.
    let mem = shared_mem();
    let mut mock = MockXhci::with_hub(&mem, 4, 4);
    mock.hub_downstream_status = 1 << 0;
    let mut device = started_device(mock, &mem);
    let delay = TestDelay::default();

    device
        .bring_up(&delay)
        .expect("the keyboard behind the hub is reached");

    let mut buf = [0u8; REPORT_LEN];
    device.host_mut().fault_one_report_completion = Some(CompletionCode::SplitTransactionError);
    assert_eq!(device.next_report(1, &mut buf), Ok(None));
    device
        .host_mut()
        .pending_reports
        .push_back(alloc::vec![0xAA, 0, 0, 0, 0, 0, 0, 0]);
    device.host_mut().process_int_ring();
    assert_eq!(
        device.next_report(1, &mut buf),
        Err(DriverError::DeviceFault)
    );

    // The controller will NOT acknowledge the Disable Slot — model the metal
    // controller that never posts the completion the teardown waits for.
    device.host_mut().suppress_disable_completion = true;

    // The slot is still freed locally despite the unconfirmable Disable Slot.
    assert_eq!(device.detach_if_device_gone(1), Ok(true));
    assert!(
        !device.device_live(1),
        "the slot is freed best-effort even without a Disable Slot confirmation"
    );
    assert!(
        device.hub_watch_active(),
        "the hub watch stays armed for the re-plug"
    );

    // Re-plug now re-enumerates (it would not if `device_slot` were still set).
    // The controller acknowledges the re-enumeration's commands again.
    device.host_mut().suppress_disable_completion = false;
    device.host_mut().hub_downstream_change = PORT_CHANGE_CONNECTION;
    device.host_mut().post_hub_status_change(&[1 << 4]);
    match device.next_hub_change(&delay) {
        Ok(HubEvent::Attached(index)) => {
            let identity = device
                .device_identity(index)
                .expect("the attached device is served");
            assert_eq!(identity.vendor_id, 0x046D);
            assert_eq!(identity.product_id, 0xC077);
        }
        other => panic!("expected a fresh attach after an unconfirmed detach, got {other:?}"),
    }
    assert!(
        device.device_live(1),
        "the re-plugged keyboard is live again"
    );
}

#[test]
fn a_failed_status_change_service_re_arms_the_watch_so_a_replug_is_still_seen() {
    // The decisive reconnect bug: after a downstream keyboard is torn down on
    // its own device-unreachable fault code, the hub posts a status-change
    // report, but the gone device's transaction translator briefly cannot
    // answer the hub's `GET_PORT_STATUS` (the metal `reject_hex=4` timeout), so
    // servicing that report errors. The status-change endpoint MUST still be
    // re-armed across that error — otherwise it is left with no outstanding
    // transfer, the hub can never post another report, and the later reconnect
    // produces no interrupt at all (the "re-plug not detected" symptom). The
    // engine then never wakes again.
    let mem = shared_mem();
    let mut mock = MockXhci::with_hub(&mem, 4, 4);
    mock.hub_downstream_status = 1 << 0;
    let mut device = started_device(mock, &mem);
    let delay = TestDelay::default();

    device
        .bring_up(&delay)
        .expect("the keyboard behind the hub is reached");

    // Unplug: the keyboard's interrupt-IN endpoint faults with a Split
    // Transaction Error and the slot is freed directly (the hub confirmation is
    // unreliable, so the device-unreachable code is conclusive on its own).
    let mut buf = [0u8; REPORT_LEN];
    device.host_mut().fault_one_report_completion = Some(CompletionCode::SplitTransactionError);
    assert_eq!(device.next_report(1, &mut buf), Ok(None));
    device
        .host_mut()
        .pending_reports
        .push_back(alloc::vec![0xAA, 0, 0, 0, 0, 0, 0, 0]);
    device.host_mut().process_int_ring();
    assert_eq!(
        device.next_report(1, &mut buf),
        Err(DriverError::DeviceFault)
    );
    assert_eq!(device.detach_if_device_gone(1), Ok(true));
    assert!(!device.device_live(1), "the gone device's slot was freed");
    assert!(device.hub_watch_active());

    // The hub posts a status-change report, but servicing it fails: right
    // after a downstream disconnect the gone device's transaction translator
    // briefly cannot answer the hub's class control transfers (the metal
    // `reject_hex=4`), so the changed port's `GET_PORT_STATUS` faults. The
    // service therefore returns an error — yet the status-change endpoint
    // MUST still be re-armed across that error, or the watch is left with no
    // outstanding transfer, the hub can never post another report, and the
    // later reconnect produces no interrupt at all (the "re-plug not
    // detected" symptom).
    device.host_mut().fault_hub_port_status = true;
    device.host_mut().hub_downstream_change = PORT_CHANGE_CONNECTION;
    device.host_mut().post_hub_status_change(&[1 << 4]);
    assert!(
        device.next_hub_change(&delay).is_err(),
        "the faulting hub control transfer surfaces as an error"
    );
    assert!(
        device.hub_watch_active(),
        "the watch stays active after a failed status-change service"
    );
    assert!(
        !device.any_device_live(),
        "the failed service enumerated nothing yet"
    );

    // The transient hub fault clears and the keyboard is (re-)plugged. The
    // connect is only delivered if the status-change endpoint was re-armed
    // despite the earlier error — i.e. an interrupt can still reach the engine.
    device.host_mut().fault_hub_port_status = false;
    device.host_mut().hub_downstream_status = 1 << 0;
    device.host_mut().hub_downstream_change = PORT_CHANGE_CONNECTION;
    device.host_mut().post_hub_status_change(&[1 << 4]);
    match device.next_hub_change(&delay) {
        Ok(HubEvent::Attached(index)) => {
            let identity = device
                .device_identity(index)
                .expect("the attached device is served");
            assert_eq!(identity.vendor_id, 0x046D);
            assert_eq!(identity.product_id, 0xC077);
        }
        other => {
            panic!("expected a fresh attach after the transient hub fault cleared, got {other:?}")
        }
    }
    assert!(
        device.device_live(1),
        "the re-plugged keyboard is live again"
    );
}

#[test]
fn hub_watch_reenumerates_a_reattached_device_on_a_fresh_slot() {
    let mem = shared_mem();
    let mut mock = MockXhci::with_hub(&mem, 4, 4);
    mock.hub_downstream_status = 1 << 0;
    let mut device = started_device(mock, &mem);
    let delay = TestDelay::default();

    device
        .bring_up(&delay)
        .expect("the keyboard behind the hub is reached");

    // Unplug.
    device.host_mut().hub_downstream_status = 0;
    device.host_mut().hub_downstream_change = PORT_CHANGE_CONNECTION;
    device.host_mut().post_hub_status_change(&[1 << 4]);
    assert_eq!(device.next_hub_change(&delay), Ok(HubEvent::Detached(1)));

    // Re-plug: the port reads connected again with the change latched. The
    // reconnect is treated as a brand-new device — a fresh slot, no reuse of
    // the old one.
    device.host_mut().hub_downstream_status = 1 << 0;
    device.host_mut().hub_downstream_change = PORT_CHANGE_CONNECTION;
    device.host_mut().post_hub_status_change(&[1 << 4]);
    match device.next_hub_change(&delay) {
        Ok(HubEvent::Attached(index)) => {
            let identity = device
                .device_identity(index)
                .expect("the reattached device is the served keyboard");
            assert_eq!(identity.vendor_id, 0x046D);
            assert_eq!(identity.product_id, 0xC077);
        }
        other => panic!("expected a fresh attach, got {other:?}"),
    }
    assert!(
        device.raw_device_slot(1) > 2,
        "a re-attach allocates a brand-new slot, never the freed one"
    );
}

#[test]
fn hub_assembly_unplug_at_root_port_tears_down_and_replug_reenumerates() {
    // On the Pi 4 the keyboard hangs off a hub, and pulling the keyboard out
    // takes that hub with it: the unplug surfaces as the hub's own *root* port
    // losing connection (its `PORTSC.CSC` latching), not as a downstream
    // hub-port change. The hub being gone, it answers neither its
    // status-change interrupt endpoint nor a GET_PORT_STATUS control
    // transfer, so watching only the downstream port never sees the
    // disconnect. The root-port scan must notice the latched change, tear
    // the assembly down, and attach a re-plug afresh — the controller
    // itself stays up throughout (no reset, so a sibling port's devices
    // would be untouched).
    let mem = shared_mem();
    let mut mock = MockXhci::with_hub(&mem, 4, 4);
    mock.hub_downstream_status = 1 << 0;
    let mut device = started_device(mock, &mem);
    let delay = TestDelay::default();

    device
        .bring_up(&delay)
        .expect("the keyboard behind the hub is reached");
    assert!(device.hub_watch_active());
    assert!(device.device_live(1));

    // While the hub is present no connect change is latched: the scan is
    // quiet and the watch is left intact for the status-change path.
    assert_eq!(device.next_root_change(&delay), Ok(HubEvent::None));
    assert!(device.hub_watch_active());

    // The whole hub assembly is now unplugged: its root port clears the
    // connect bit and latches the connect change.
    device.host_mut().portsc[0] = regs::PORTSC_PP | regs::PORTSC_CSC;
    match device.next_root_change(&delay) {
        Ok(HubEvent::HubDetached(_)) => {}
        other => panic!("the hub assembly detach is detected, got {other:?}"),
    }
    assert!(
        !device.hub_watch_active(),
        "the hub watch is dropped once the hub itself is gone"
    );
    assert!(
        !device.any_device_live(),
        "no device is tracked after the hub assembly is removed"
    );
    // The latch was consumed: a second scan is quiet, never re-firing on
    // stale state.
    assert_eq!(device.next_root_change(&delay), Ok(HubEvent::None));

    // A re-plug: the hub assembly reappears on its root port (connect +
    // latched change). The scan attaches it afresh — the hub installed,
    // descended, and watched, the keyboard behind it enumerated — without
    // any controller reset.
    device.host_mut().portsc[0] = regs::PORTSC_CCS
        | regs::PORTSC_PED
        | regs::PORTSC_PP
        | (3 << regs::PORTSC_SPEED_SHIFT)
        | regs::PORTSC_CSC;
    match device.next_root_change(&delay) {
        Ok(HubEvent::HubAttached(_)) => {}
        other => panic!("the hub assembly re-attach is served, got {other:?}"),
    }
    let identity = device
        .device_identity(1)
        .expect("the reattached hub+keyboard must enumerate");
    assert_eq!(identity.vendor_id, 0x046D);
    assert_eq!(identity.product_id, 0xC077);
    assert!(
        device.device_live(1),
        "the keyboard is live again after the re-plug"
    );
    assert!(
        device.hub_watch_active(),
        "the hub watch is re-armed for the freshly enumerated assembly"
    );
}

#[test]
fn a_device_plugged_into_a_second_root_port_is_served_while_the_hub_stays_watched() {
    // The Pi 4 metal defect this rework fixes: only the USB2 side of the
    // jacks runs through the watched onboard hub — a `SuperSpeed` device
    // trains directly on *another* root port. The old engine served only
    // the first connected root port and never scanned the others while a
    // hub watch was active, so plugging such a device produced nothing at
    // all (no log, no node). The root-port scan must attach it beside the
    // hub tier, and its unplug must detach only it.
    let mem = shared_mem();
    let mut mock = MockXhci::with_hub(&mem, 4, 4);
    mock.hub_downstream_status = 1 << 0;
    let mut device = started_device(mock, &mem);
    let delay = TestDelay::default();

    device
        .bring_up(&delay)
        .expect("the hub tier comes up on root port 1");
    assert!(device.hub_watch_active());
    assert!(
        device.device_live(1),
        "the keyboard behind the hub is served"
    );

    // A device is plugged into root port 2: connected, already enabled
    // (the `SuperSpeed` shape — no reset needed), connect change latched.
    device.host_mut().portsc[1] = regs::PORTSC_CCS
        | regs::PORTSC_PED
        | regs::PORTSC_PP
        | (3 << regs::PORTSC_SPEED_SHIFT)
        | regs::PORTSC_CSC;
    let index = match device.next_root_change(&delay) {
        Ok(HubEvent::Attached(index)) => index,
        other => panic!("the second root port's device is attached, got {other:?}"),
    };
    let identity = device
        .device_identity(index)
        .expect("the directly-attached device is served");
    assert_eq!(identity.vendor_id, 0x046D);
    assert!(
        device.device_live(1),
        "the hub's keyboard is untouched by the new attach"
    );
    assert!(device.hub_watch_active(), "the hub watch stays armed");
    // The latch was consumed: the scan is quiet until the next change.
    assert_eq!(device.next_root_change(&delay), Ok(HubEvent::None));

    // Unplug it again: the disconnect detaches only that device.
    device.host_mut().portsc[1] = regs::PORTSC_PP | regs::PORTSC_CSC;
    assert_eq!(
        device.next_root_change(&delay),
        Ok(HubEvent::Detached(index))
    );
    assert!(!device.device_live(index), "the direct device is freed");
    assert!(
        device.device_live(1),
        "the hub's keyboard survives the sibling port's unplug"
    );
    assert!(device.hub_watch_active());
}

#[test]
fn bring_up_serves_a_hub_tier_and_a_direct_root_device_together() {
    // The multi-root cold boot: the onboard hub (with its keyboard) sits
    // on root port 1 and a directly-attached device on root port 2. The
    // walk must serve *every* connected root port, not just the first.
    let mem = shared_mem();
    let mut mock = MockXhci::with_hub(&mem, 4, 4);
    mock.hub_downstream_status = 1 << 0;
    mock.portsc[1] =
        regs::PORTSC_CCS | regs::PORTSC_PED | regs::PORTSC_PP | (3 << regs::PORTSC_SPEED_SHIFT);
    let mut device = started_device(mock, &mem);
    let delay = TestDelay::default();

    device.bring_up(&delay).expect("both root ports are served");
    assert!(
        device.device_live(1),
        "the keyboard behind the hub is served"
    );
    assert!(
        device.device_live(2),
        "the directly-attached device on root port 2 is served beside it"
    );
    assert_ne!(
        device.raw_device_slot(1),
        device.raw_device_slot(2),
        "separate devices on separate slots"
    );
    assert!(device.hub_watch_active(), "the hub tier is watched");
}

#[test]
fn reset_and_reenumerate_brings_up_a_directly_attached_device_as_new() {
    // The recovery path for a directly-attached (no hub) device that
    // reconnected on its root port: a full controller reset + re-enumeration
    // brings it up as a brand-new device.
    let mem = shared_mem();
    let mut device = started_device(MockXhci::with_device(&mem), &mem);
    let delay = TestDelay::default();

    device
        .bring_up(&delay)
        .expect("the directly-attached keyboard enumerates");
    assert_eq!(device.raw_device_slot(0), 1);

    device
        .reset_and_reenumerate(&delay)
        .expect("the controller resets and re-enumerates the device");
    let descriptor = device
        .device_identity(0)
        .expect("a connected directly-attached device must enumerate");
    assert_eq!(descriptor.vendor_id, 0x046D);
    assert_ne!(
        device.raw_device_slot(0),
        0,
        "a device is enumerated after the reset"
    );
}

#[test]
fn bring_up_keyboard_comes_up_awaiting_a_connect_when_no_device_is_attached() {
    // The cold-boot path for a directly-attached topology with nothing
    // plugged in: no root-hub port reports a connected device, so bring-up
    // must NOT fail. The controller comes up `AwaitingDevice` (no hub, so the
    // root-port connect watch is used, not a hub status-change watch) and the
    // HCD waits for the first root-port connect rather than failing closed.
    let mem = shared_mem();
    let mut mock = MockXhci::with_device(&mem);
    // No connected device on any root port, and no latent device to assert a
    // connect when the ports are powered.
    mock.portsc[0] = 0;
    let mut device = started_device(mock, &mem);
    let delay = TestDelay::default();

    device
        .bring_up(&delay)
        .expect("an empty root hub comes up awaiting a device, not failing");
    assert!(!device.any_device_live());
    assert!(
        !device.hub_watch_active(),
        "no hub is present, so the root-port scan is the connect watch"
    );
    assert!(!device.device_live(0), "no device is live yet");
    // Nothing attached and no change latched: the scan is quiet.
    assert_eq!(device.next_root_change(&delay), Ok(HubEvent::None));

    // A keyboard is now plugged into a root port: the connect latches
    // `PORTSC.CSC` and the scan attaches it — no controller reset.
    device.host_mut().portsc[0] = regs::PORTSC_CCS
        | regs::PORTSC_PED
        | regs::PORTSC_PP
        | (3 << regs::PORTSC_SPEED_SHIFT)
        | regs::PORTSC_CSC;
    match device.next_root_change(&delay) {
        Ok(HubEvent::Attached(0)) => {}
        other => panic!("the first connect is attached, got {other:?}"),
    }
    let descriptor = device
        .device_identity(0)
        .expect("the now-connected device must enumerate");
    assert_eq!(descriptor.vendor_id, 0x046D);
    assert!(
        device.device_live(0),
        "the keyboard is live after the attach"
    );
}

#[test]
fn bring_up_serves_a_keyboard_and_a_storage_stick_behind_the_hub_together() {
    // The Pi 4 boot defect the multi-device engine fixes: with a storage
    // stick plugged in beside the keyboard, the stick won the engine's
    // single device slot and the keyboard never enumerated (the boot hung
    // with dead input). Both hub ports must be served concurrently, each on
    // its own device index.
    let mem = shared_mem();
    let mut mock = MockXhci::with_hub(&mem, 4, 4);
    // The stick sits on the lower-numbered port, so the bring-up walk
    // reaches it first — exactly the ordering that used to displace the
    // keyboard.
    mock.msd_downstream_port = 2;
    let mut device = started_device(mock, &mem);
    let delay = TestDelay::default();

    device
        .bring_up(&delay)
        .expect("bring-up serves both devices");
    assert!(device.device_live(1), "the stick (walked first) is served");
    assert!(device.device_live(2), "the keyboard is served beside it");
    let stick = device.device_identity(1).expect("the stick is index 1");
    assert_eq!(stick.vendor_id, 0x0781);
    assert_eq!(
        stick.interface_class >> 16,
        0x08,
        "a mass-storage interface"
    );
    let keyboard = device.device_identity(2).expect("the keyboard is index 2");
    assert_eq!(keyboard.vendor_id, 0x046D);
    assert_eq!(keyboard.interface_class >> 16, 0x03, "a HID interface");
    assert!(device.hub_watch_active());

    // Each device's node derives its own class, so `devmgr` autoloads the
    // storage class driver *and* the keyboard class driver.
    let stick_node = device.describe_device(1, 0, 1).expect("stick node");
    assert_eq!(stick_node.class(), Some(rustos_abi::HwDeviceClass::Storage));
    let kbd_node = device.describe_device(2, 0, 2).expect("keyboard node");
    assert_eq!(kbd_node.class(), Some(rustos_abi::HwDeviceClass::Input));

    // The keyboard's reports flow on its own index...
    let mut buf = [0u8; REPORT_LEN];
    assert_eq!(device.next_report(2, &mut buf), Ok(None));
    device
        .host_mut()
        .pending_reports
        .push_back(alloc::vec![0x00, 0x00, 0x04, 0x00, 0x00, 0x00, 0x00, 0x00]);
    device.host_mut().process_int_ring();
    assert_eq!(device.next_report(2, &mut buf), Ok(Some(REPORT_LEN)));
    assert_eq!(buf[2], 0x04, "the keystroke reaches the keyboard's index");

    // ...and the stick's bulk transfers on its own index, concurrently.
    let response = alloc::vec![0x42u8; 8];
    device
        .host_mut()
        .bulk_in_responses
        .push_back(response.clone());
    device.queue_bulk_in(1, IN_PIPE, 8).expect("bulk TD queues");
    let mut bulk_buf = [0u8; 8];
    let complete = device
        .poll_bulk(1, &mut bulk_buf)
        .expect("poll succeeds")
        .expect("the bulk TD completes");
    assert_eq!(complete.result, Ok(8));
    assert_eq!(&bulk_buf[..], &response[..]);
}

#[test]
fn unplugging_the_keyboard_leaves_the_storage_stick_served() {
    // A disconnect frees only the vanished device's index: the stick keeps
    // serving bulk I/O through the keyboard's unplug, and the keyboard's
    // re-plug lands back on its own (freed) index.
    let mem = shared_mem();
    let mut mock = MockXhci::with_hub(&mem, 4, 4);
    mock.msd_downstream_port = 2;
    let mut device = started_device(mock, &mem);
    let delay = TestDelay::default();
    device
        .bring_up(&delay)
        .expect("bring-up serves both devices");
    assert!(device.device_live(1) && device.device_live(2));

    // Unplug the keyboard (port 4): the hub latches the connect change and
    // posts a status-change report naming that port.
    device.host_mut().hub_downstream_status = 0;
    device.host_mut().hub_downstream_change = PORT_CHANGE_CONNECTION;
    device.host_mut().post_hub_status_change(&[1 << 4]);
    assert_eq!(
        device.next_hub_change(&delay),
        Ok(HubEvent::Detached(2)),
        "only the keyboard's index is detached"
    );
    assert!(!device.device_live(2), "the keyboard's index is freed");
    assert!(
        device.device_live(1),
        "the stick is untouched by the keyboard's unplug"
    );

    // The stick still serves bulk I/O after the keyboard is gone.
    let response = alloc::vec![0x9Cu8; 6];
    device
        .host_mut()
        .bulk_in_responses
        .push_back(response.clone());
    device.queue_bulk_in(1, IN_PIPE, 6).expect("bulk TD queues");
    let mut bulk_buf = [0u8; 6];
    let complete = device
        .poll_bulk(1, &mut bulk_buf)
        .expect("poll succeeds")
        .expect("the bulk TD completes");
    assert_eq!(complete.result, Ok(6));
    assert_eq!(&bulk_buf[..], &response[..]);

    // The keyboard re-plugs: a brand-new enumeration lands on the freed
    // index, beside the still-served stick.
    device.host_mut().hub_downstream_status = (1 << 0) | (1 << 10);
    device.host_mut().hub_downstream_change = PORT_CHANGE_CONNECTION;
    device.host_mut().post_hub_status_change(&[1 << 4]);
    match device.next_hub_change(&delay) {
        Ok(HubEvent::Attached(index)) => {
            assert_eq!(index, 2, "the re-plugged keyboard reuses the freed index");
            let identity = device
                .device_identity(index)
                .expect("the re-attached keyboard is served");
            assert_eq!(identity.vendor_id, 0x046D);
        }
        other => panic!("expected the keyboard to re-attach, got {other:?}"),
    }
    assert!(device.device_live(1), "the stick is still served");
}

#[test]
fn bring_up_serves_a_keyboard_and_a_mouse_behind_the_hub_together() {
    // The Pi 4 defect the mouse class driver rides on: a keyboard and a
    // mouse plugged in together must both be served, each on its own
    // device index with its own interrupt endpoint, and each emitted node
    // must carry its own interface class so `devmgr` autoloads the
    // keyboard *and* the mouse class driver.
    let mem = shared_mem();
    let mut mock = MockXhci::with_hub(&mem, 4, 4);
    // The mouse sits on the lower-numbered port, so the bring-up walk
    // reaches it first.
    mock.mouse_downstream_port = 2;
    let mut device = started_device(mock, &mem);
    let delay = TestDelay::default();

    device
        .bring_up(&delay)
        .expect("bring-up serves both devices");
    assert!(device.device_live(1), "the mouse (walked first) is served");
    assert!(device.device_live(2), "the keyboard is served beside it");
    let mouse = device.device_identity(1).expect("the mouse is index 1");
    assert_eq!(mouse.product_id, 0xC539);
    assert_eq!(
        mouse.interface_class, 0x03_01_02,
        "a HID boot-mouse interface"
    );
    let keyboard = device.device_identity(2).expect("the keyboard is index 2");
    assert_eq!(keyboard.product_id, 0xC077);
    assert_eq!(
        keyboard.interface_class, 0x03_01_01,
        "a HID boot-keyboard interface"
    );
    assert!(device.hub_watch_active());

    // Each node derives its own class and match key, so the keyboard and
    // the mouse class drivers autoload independently.
    let mouse_node = device.describe_device(1, 0, 1).expect("mouse node");
    assert_eq!(mouse_node.class(), Some(rustos_abi::HwDeviceClass::Input));
    let kbd_node = device.describe_device(2, 0, 2).expect("keyboard node");
    assert_eq!(kbd_node.class(), Some(rustos_abi::HwDeviceClass::Input));

    // The keyboard's reports flow on its own index...
    let mut buf = [0u8; REPORT_LEN];
    assert_eq!(device.next_report(2, &mut buf), Ok(None));
    device
        .host_mut()
        .pending_reports
        .push_back(alloc::vec![0x00, 0x00, 0x04, 0x00, 0x00, 0x00, 0x00, 0x00]);
    device.host_mut().process_int_ring();
    assert_eq!(device.next_report(2, &mut buf), Ok(Some(REPORT_LEN)));
    assert_eq!(buf[2], 0x04, "the keystroke reaches the keyboard's index");

    // ...and the mouse's boot reports on its own index, concurrently.
    let mut mouse_buf = [0u8; REPORT_LEN];
    assert_eq!(device.next_report(1, &mut mouse_buf), Ok(None));
    device
        .host_mut()
        .pending_reports2
        .push_back(alloc::vec![0x01, 0x05, 0xFB, 0x00]);
    device.host_mut().process_int2_ring();
    assert_eq!(device.next_report(1, &mut mouse_buf), Ok(Some(4)));
    assert_eq!(
        &mouse_buf[..4],
        &[0x01, 0x05, 0xFB, 0x00],
        "left button + X/Y deltas reach the mouse's index"
    );
}

#[test]
fn a_failing_port_at_bring_up_never_costs_the_keyboard_its_service() {
    // A broken or half-seated device whose port never enables after the
    // reset must be skipped fail-soft by the bring-up walk: the keyboard
    // beside it is still served, and the failure claims no device index.
    let mem = shared_mem();
    let mut mock = MockXhci::with_hub(&mem, 4, 4);
    mock.mouse_downstream_port = 2;
    mock.fail_enable_downstream_port = 2;
    let mut device = started_device(mock, &mem);
    let delay = TestDelay::default();

    device
        .bring_up(&delay)
        .expect("bring-up survives the broken port");
    assert!(
        device.device_live(1),
        "the keyboard is served on the first free index"
    );
    let keyboard = device.device_identity(1).expect("the keyboard is served");
    assert_eq!(keyboard.interface_class, 0x03_01_01);
    assert!(!device.device_live(2), "the broken device claimed no index");
    assert!(device.hub_watch_active(), "the hub watch is still armed");
}

#[test]
fn a_slow_hub_port_reset_is_polled_until_it_completes() {
    // A slow external hub legitimately takes several polls (hundreds of
    // milliseconds) to complete a downstream port reset. A single fixed
    // wait followed by one enable check refused such a device as a
    // DeviceFault; the reset-completion wait must re-poll `GET_STATUS`
    // until the hub reports the reset done and the port enabled.
    let mem = shared_mem();
    let mut mock = MockXhci::with_hub(&mem, 4, 4);
    mock.slow_enable_status_reads = 5;
    let mut device = started_device(mock, &mem);
    let delay = TestDelay::default();

    device
        .bring_up(&delay)
        .expect("bring-up polls the slow reset to completion");
    assert!(
        device.device_live(1),
        "the keyboard behind the slow port is served"
    );
    assert_eq!(device.skipped_port_count(), 0);
    assert_eq!(
        device.host_mut().slow_enable_status_reads,
        0,
        "the poll consumed every reset-in-progress read"
    );
}

#[test]
fn a_port_that_never_enables_records_its_stage_port_and_final_status() {
    // The hot-plug fault breadcrumb: an attach whose port never enables
    // must leave the enumeration stage at PortReset and record the
    // targeted port and its final observed `wPortStatus`, so the coarse
    // DeviceFault a metal log carries is localisable to "connected but
    // never enabled" rather than guessed at.
    let mem = shared_mem();
    let mut mock = MockXhci::with_hub(&mem, 4, 4);
    mock.fail_enable_downstream_port = 4;
    let mut device = started_device(mock, &mem);
    let delay = TestDelay::default();

    device
        .bring_up(&delay)
        .expect("bring-up survives the broken port");
    assert!(!device.device_live(0), "the broken device claimed no index");
    assert_eq!(device.skipped_port_count(), 1);
    let fault = device
        .last_attach_fault()
        .expect("the failed attach left its fault snapshot");
    assert_eq!(fault.port, 4);
    assert_eq!(fault.error, DriverError::DeviceFault);
    assert_eq!(fault.stage, EnumStage::PortReset);
    assert!(
        crate::device::hub_port_connected(fault.port_status),
        "the final observed status shows the device present"
    );
    assert!(
        !crate::device::hub_port_enabled(fault.port_status),
        "...but the port never enabled"
    );
    assert!(device.hub_watch_active(), "the hub watch is still armed");
}

#[test]
fn bring_up_serves_both_interfaces_of_a_composite_receiver() {
    use crate::transport::UrbEngine;
    use rustos_abi::HwMatchKey;
    // The wireless keyboard+mouse receiver: ONE device behind the hub whose
    // configuration carries a boot-keyboard interface and a boot-mouse
    // interface. Both must be served — each on its own device index with
    // its own interrupt endpoint and its own emitted node — while sharing
    // one slot and one EP0. Its 75-byte configuration also proves the
    // full-length configuration read (a 64-byte read truncated the mouse
    // interface away).
    let mem = shared_mem();
    let mut mock = MockXhci::with_hub(&mem, 4, 4);
    mock.composite_downstream_port = 4;
    // Like the real receiver, a full-speed device whose 8-byte EP0 must be
    // re-evaluated before any multi-packet descriptor read succeeds.
    mock.hub_downstream_status = 1 << 0;
    let mut device = started_device(mock, &mem);
    let delay = TestDelay::default();

    device
        .bring_up(&delay)
        .expect("the composite receiver enumerates");
    assert!(device.device_live(1), "the keyboard interface is served");
    assert!(device.device_live(2), "the mouse interface is served");
    assert!(!device.device_live(3));
    let keyboard = device.device_identity(1).expect("keyboard identity");
    assert_eq!(keyboard.product_id, 0xC534);
    assert_eq!(keyboard.interface_class, 0x03_01_01);
    let mouse = device.device_identity(2).expect("mouse identity");
    assert_eq!(mouse.product_id, 0xC534, "one physical device");
    assert_eq!(mouse.interface_class, 0x03_01_02);
    assert_eq!(
        device.raw_device_slot(1),
        device.raw_device_slot(2),
        "both interfaces ride one device slot"
    );
    assert_eq!(
        device.host_mut().evaluate_context_count,
        1,
        "the 8-byte EP0 was re-evaluated exactly once for the one device"
    );
    assert!(device.hub_watch_active());

    // Each interface publishes its own node with its own class key, so
    // `devmgr` autoloads the keyboard AND the mouse class driver.
    let kbd_node = device.describe_device(1, 0, 1).expect("keyboard node");
    assert!(HwMatchKey::usb(0, 0, 0x03_01_01).matches(&kbd_node.match_keys()[0]));
    let mouse_node = device.describe_device(2, 0, 2).expect("mouse node");
    assert!(HwMatchKey::usb(0, 0, 0x03_01_02).matches(&mouse_node.match_keys()[0]));
    // Both interface nodes carry the one physical device's slot as their
    // device address, so an inventory consumer (`lsusb`) attributes them
    // to a single device rather than listing it twice.
    assert_ne!(kbd_node.address(), 0);
    assert_eq!(kbd_node.address(), mouse_node.address());

    // Keystrokes flow on the keyboard interface's index...
    let mut buf = [0u8; REPORT_LEN];
    assert_eq!(device.next_report(1, &mut buf), Ok(None));
    device
        .host_mut()
        .pending_reports
        .push_back(alloc::vec![0x00, 0x00, 0x04, 0x00, 0x00, 0x00, 0x00, 0x00]);
    device.host_mut().process_int_ring();
    assert_eq!(device.next_report(1, &mut buf), Ok(Some(REPORT_LEN)));
    assert_eq!(buf[2], 0x04);

    // ...and mouse reports on the mouse interface's index, concurrently.
    let mut mouse_buf = [0u8; REPORT_LEN];
    assert_eq!(device.next_report(2, &mut mouse_buf), Ok(None));
    device
        .host_mut()
        .pending_reports2
        .push_back(alloc::vec![0x01, 0x05, 0xFB, 0x00]);
    device.host_mut().process_int2_ring();
    assert_eq!(device.next_report(2, &mut mouse_buf), Ok(Some(4)));
    assert_eq!(&mouse_buf[..4], &[0x01, 0x05, 0xFB, 0x00]);

    // A control transfer through the SIBLING index routes through the
    // slot's EP0 owner (the primary entry parked it), so a mouse class
    // driver's control-IN works even though its entry never held the ring.
    let mut data = [0u8; 18];
    let setup = [0x80, 0x06, 0x00, 0x01, 0x00, 0x00, 18, 0x00];
    let transferred = device
        .engine_for(2)
        .control_in(setup, &mut data)
        .expect("the sibling's control transfer routes through the EP0 owner");
    assert_eq!(transferred, 18);
    assert_eq!(&data[..], &MOCK_COMPOSITE_DESCRIPTOR[..]);
}

#[test]
fn unplugging_a_composite_receiver_frees_both_interfaces_and_a_replug_reserves_them() {
    let mem = shared_mem();
    let mut mock = MockXhci::with_hub(&mem, 4, 4);
    mock.composite_downstream_port = 4;
    // Full speed with an 8-byte EP0, like the real receiver.
    mock.hub_downstream_status = 1 << 0;
    let mut device = started_device(mock, &mem);
    let delay = TestDelay::default();

    device
        .bring_up(&delay)
        .expect("the composite receiver enumerates");
    assert!(device.device_live(1) && device.device_live(2));

    // Unplug the receiver: ONE physical disconnect must free BOTH interface
    // entries — a stale sibling entry would hold the freed slot's rings.
    device.host_mut().hub_downstream_status = 0;
    device.host_mut().hub_downstream_change = PORT_CHANGE_CONNECTION;
    device.host_mut().post_hub_status_change(&[1 << 4]);
    assert_eq!(device.next_hub_change(&delay), Ok(HubEvent::Detached(1)));
    assert!(!device.device_live(1), "the keyboard interface is freed");
    assert!(
        !device.device_live(2),
        "the sibling mouse interface is freed with it"
    );
    assert!(device.hub_watch_active());

    // Re-plug: a brand-new enumeration serves both interfaces again.
    device.host_mut().hub_downstream_status = 1 << 0;
    device.host_mut().hub_downstream_change = PORT_CHANGE_CONNECTION;
    device.host_mut().post_hub_status_change(&[1 << 4]);
    assert_eq!(device.next_hub_change(&delay), Ok(HubEvent::Attached(1)));
    assert!(device.device_live(1), "the keyboard interface is re-served");
    assert!(device.device_live(2), "the mouse interface is re-served");
    assert_eq!(
        device
            .device_identity(2)
            .expect("mouse identity")
            .interface_class,
        0x03_01_02
    );
}

#[test]
fn a_composite_receiver_beside_the_keyboard_costs_it_nothing() {
    // The metal defect this rides on: booting with the wireless receiver
    // plugged in beside the ordinary keyboard killed the keyboard. Both
    // devices — three interfaces in total — must be served concurrently.
    let mem = shared_mem();
    let mut mock = MockXhci::with_hub(&mem, 4, 4);
    // The receiver sits on the lower-numbered port, so the walk reaches it
    // first. Both devices are full speed; only the receiver's 8-byte EP0
    // needs the Evaluate Context fix-up.
    mock.composite_downstream_port = 2;
    mock.hub_downstream_status = 1 << 0;
    let mut device = started_device(mock, &mem);
    let delay = TestDelay::default();

    device
        .bring_up(&delay)
        .expect("bring-up serves both devices");
    let composite_kbd = device.device_identity(1).expect("receiver keyboard");
    assert_eq!(composite_kbd.product_id, 0xC534);
    assert_eq!(composite_kbd.interface_class, 0x03_01_01);
    let composite_mouse = device.device_identity(2).expect("receiver mouse");
    assert_eq!(composite_mouse.product_id, 0xC534);
    assert_eq!(composite_mouse.interface_class, 0x03_01_02);
    let keyboard = device.device_identity(3).expect("the ordinary keyboard");
    assert_eq!(keyboard.product_id, 0xC077);
    assert_eq!(
        keyboard.interface_class, 0x03_01_01,
        "the ordinary keyboard is served beside the receiver's two interfaces"
    );
    assert_ne!(
        device.raw_device_slot(1),
        device.raw_device_slot(3),
        "the receiver and the keyboard are separate devices on separate slots"
    );
    assert_eq!(
        device.host_mut().evaluate_context_count,
        1,
        "only the receiver's EP0 needed re-evaluating; the 64-byte keyboard did not"
    );
    assert!(device.hub_watch_active());
}

#[test]
fn a_forged_ep0_max_packet_fails_closed_without_costing_the_keyboard() {
    // A full-speed device may report bMaxPacketSize0 of 8/16/32/64 only
    // (USB 2.0 §5.5.3). A receiver forging 7 must be rejected fail-closed
    // — never programmed into the EP0 context — and its failure must not
    // cost the keyboard beside it its service.
    let mem = shared_mem();
    let mut mock = MockXhci::with_hub(&mem, 4, 4);
    mock.composite_downstream_port = 2;
    mock.forge_composite_ep0_max = true;
    mock.hub_downstream_status = 1 << 0;
    let mut device = started_device(mock, &mem);
    let delay = TestDelay::default();

    device
        .bring_up(&delay)
        .expect("bring-up survives the forged device");
    let keyboard = device.device_identity(1).expect("the keyboard is served");
    assert_eq!(keyboard.product_id, 0xC077);
    assert!(
        !device.device_live(2) && !device.device_live(3),
        "the forged device claimed no index"
    );
    assert_eq!(
        device.host_mut().evaluate_context_count,
        0,
        "a forged bMaxPacketSize0 is never programmed into the EP0 context"
    );
    assert!(device.hub_watch_active());
}

#[test]
fn ep0_max_packet_validation_follows_the_speed_rules() {
    use crate::device::ep0_max_packet_from_descriptor as validate;
    // Low speed fixes 8 (USB 2.0 §5.5.3).
    assert_eq!(validate(2, 8), Ok(8));
    assert_eq!(validate(2, 64), Err(DriverError::BadMagic));
    // Full speed allows exactly 8/16/32/64.
    for size in [8u8, 16, 32, 64] {
        assert_eq!(validate(1, size), Ok(u32::from(size)));
    }
    assert_eq!(validate(1, 7), Err(DriverError::BadMagic));
    assert_eq!(validate(1, 0), Err(DriverError::BadMagic));
    // High speed fixes 64.
    assert_eq!(validate(3, 64), Ok(64));
    assert_eq!(validate(3, 8), Err(DriverError::BadMagic));
    // `SuperSpeed` encodes its fixed 512 as the exponent 9 (USB 3.2 §9.6.1).
    assert_eq!(validate(4, 9), Ok(512));
    assert_eq!(validate(4, 64), Err(DriverError::BadMagic));
    // A speed ID this driver does not model fails closed.
    assert_eq!(validate(0, 8), Err(DriverError::DeviceFault));
}

#[test]
fn a_failed_hot_plug_attach_drains_the_port_latches_so_the_watch_stays_quiet() {
    // The metal fault loop: a connect change whose device fails to attach
    // used to leave the port's latched changes set, so the hub re-reported
    // the same change forever and every re-service re-ran the failing
    // multi-second enumeration — starving every other device's service.
    // A failed attach must drain the latches (one surfaced error, then
    // quiet) rather than wedging the watch.
    let mem = shared_mem();
    let mut mock = MockXhci::with_hub(&mem, 4, 4);
    // The watched downstream device's port never enables after a reset.
    mock.fail_enable_downstream_port = 4;
    let mut device = started_device(mock, &mem);
    let delay = TestDelay::default();

    device
        .bring_up(&delay)
        .expect("bring-up survives the broken port");
    assert!(!device.any_device_live(), "nothing enumerates");
    assert!(device.hub_watch_active(), "the hub watch is armed");
    assert_eq!(
        device.host_mut().hub_downstream_change,
        0,
        "the failed bring-up attach drained the port's latches"
    );

    // The hub reports a fresh connect change for the broken device.
    device.host_mut().hub_downstream_change = PORT_CHANGE_CONNECTION;
    device.host_mut().post_hub_status_change(&[1 << 4]);
    assert_eq!(
        device.next_hub_change(&delay),
        Err(DriverError::DeviceFault),
        "the failing attach is surfaced once"
    );
    assert_eq!(
        device.host_mut().hub_downstream_change,
        0,
        "the failed attach drained every latch, so the hub cannot re-report it"
    );
    // With the latches drained the watch goes quiet: no further completion
    // is pending, and the service reports nothing rather than re-running
    // the failing enumeration forever.
    assert_eq!(device.next_hub_change(&delay), Ok(HubEvent::None));
}

#[test]
fn route_for_child_extends_one_nibble_per_tier_and_fails_closed() {
    // Tier 0 (the root-attached hub) fills the low nibble; each deeper
    // tier the next (xHCI §8.9.1, least-significant nibble first).
    assert_eq!(route_for_child(0, 0, 3), Ok(0x3));
    assert_eq!(route_for_child(0x3, 1, 2), Ok(0x23));
    assert_eq!(route_for_child(0x23, 2, 15), Ok(0xF23));
    // Port 0 names no downstream port and a port above 15 cannot be
    // encoded in a nibble; the route string holds exactly MAX_HUB_DEPTH
    // tiers — all fail closed rather than aliasing topology.
    assert_eq!(route_for_child(0, 0, 0), Err(DriverError::OutOfRange));
    assert_eq!(route_for_child(0, 0, 16), Err(DriverError::OutOfRange));
    assert_eq!(
        route_for_child(0, MAX_HUB_DEPTH, 1),
        Err(DriverError::OutOfRange)
    );
}

#[test]
fn bring_up_serves_a_keyboard_behind_a_nested_hub() {
    // A hub plugged into a hub: the root hub carries a second hub on its
    // downstream port 3, and a full-speed keyboard hangs off that nested
    // hub's port 2. The bring-up walk descends both tiers: the nested hub
    // is installed, marked a hub on its own slot, and watched, and the
    // keyboard is addressed with the two-nibble route string and the
    // *nested* hub's transaction translator — the mock faults Address
    // Device on any other topology, so a served keyboard proves both.
    let mem = shared_mem();
    let mut device = started_device(MockXhci::with_nested_hub(&mem), &mem);
    let delay = TestDelay::default();
    device.bring_up(&delay).expect("both hub tiers come up");

    // The root hub's contexts claimed device region 0 and the nested
    // hub's region 1, so the keyboard is served on the next free index —
    // and neither hub entry is misreported as a live device.
    assert!(!device.device_live(0));
    assert!(!device.device_live(1));
    assert!(
        device.device_live(2),
        "the keyboard behind the nested hub is served"
    );
    let identity = device.device_identity(2).expect("the keyboard is served");
    assert_eq!(identity.vendor_id, 0x046D);
    assert_eq!(
        device.host_mut().downstream_route,
        0x23,
        "nibble 0 routes the root hub's port 3, nibble 1 the nested hub's port 2"
    );
    assert!(
        device.host_mut().nested_hubs[0].marked,
        "the nested hub's own slot carries the Hub bit"
    );
    assert_ne!(
        device.host_mut().nested_hubs[0].int.dci,
        0,
        "the nested hub's status-change watch is configured and armed"
    );
    assert!(device.hub_watch_active());

    // Keystrokes flow end to end through both tiers.
    let mut buf = [0u8; REPORT_LEN];
    assert_eq!(device.next_report(2, &mut buf), Ok(None));
    device
        .host_mut()
        .pending_reports
        .push_back(alloc::vec![0x00, 0x00, 0x04, 0x00, 0x00, 0x00, 0x00, 0x00]);
    device.host_mut().process_int_ring();
    let len = device
        .next_report(2, &mut buf)
        .expect("a report drains")
        .expect("a report is available");
    assert_eq!(len, REPORT_LEN);
    assert_eq!(buf[2], 0x04, "the 'a' keycode crosses both hub tiers");
}

#[test]
fn hot_plug_on_a_nested_hubs_port_attaches_and_detaches_through_its_own_watch() {
    // Nothing behind the nested hub at bring-up: both tiers' watches arm,
    // and a later connect on the *nested* hub's port is serviced through
    // the nested hub's own status-change endpoint — then the unplug frees
    // the keyboard again, leaving both hubs watched.
    let mem = shared_mem();
    let mut mock = MockXhci::with_nested_hub(&mem);
    mock.nested_hubs[0].downstream_status = 0;
    let mut device = started_device(mock, &mem);
    let delay = TestDelay::default();
    device
        .bring_up(&delay)
        .expect("both hub tiers come up empty");
    assert!(!device.any_device_live());
    assert!(device.hub_watch_active());

    // Plug the keyboard into the nested hub's port 2.
    device.host_mut().nested_hubs[0].downstream_status = 1 << 0;
    device.host_mut().nested_hubs[0].downstream_change = PORT_CHANGE_CONNECTION;
    device
        .host_mut()
        .post_nested_hub_status_change(3, &[1 << 2]);
    let index = match device.next_hub_change(&delay) {
        Ok(HubEvent::Attached(index)) => index,
        other => panic!("expected an attach through the nested hub's watch, got {other:?}"),
    };
    assert!(device.device_live(index));

    // Unplug it again: the disconnect arrives on the nested hub's watch
    // and frees only the keyboard, never a hub.
    device.host_mut().nested_hubs[0].downstream_status = 0;
    device.host_mut().nested_hubs[0].downstream_change = PORT_CHANGE_CONNECTION;
    device
        .host_mut()
        .post_nested_hub_status_change(3, &[1 << 2]);
    assert_eq!(
        device.next_hub_change(&delay),
        Ok(HubEvent::Detached(index))
    );
    assert!(!device.device_live(index));
    assert!(device.hub_watch_active(), "both watches stay armed");
}

#[test]
fn unplugging_a_nested_hub_cascades_and_a_replug_rebuilds_the_tier() {
    // Pulling a hub out of a hub takes every device behind it too: the
    // disconnect arrives on the *root* hub's watch, the nested tier is
    // torn down as one cascade, and a re-plug rebuilds it from scratch —
    // the nested hub reinstalled and watched, and its keyboard re-served.
    let mem = shared_mem();
    let mut device = started_device(MockXhci::with_nested_hub(&mem), &mem);
    let delay = TestDelay::default();
    device.bring_up(&delay).expect("both hub tiers come up");
    assert!(device.device_live(2));

    device.host_mut().nested_hubs[0].connected = false;
    device.host_mut().nested_hubs[0].root_change = PORT_CHANGE_CONNECTION;
    device.host_mut().post_hub_status_change(&[1 << 3]);
    match device.next_hub_change(&delay) {
        Ok(HubEvent::HubDetached(_)) => {}
        other => panic!("expected the hub tier to cascade down, got {other:?}"),
    }
    assert!(!device.any_device_live(), "the keyboard went with its hub");
    assert!(device.hub_watch_active(), "the root hub stays watched");

    // Re-plug the hub assembly. The old slot was disabled with the tier,
    // so the mock forgets it too; a brand-new enumeration re-addresses
    // the hub on a fresh slot and re-marks it.
    device.host_mut().nested_hubs[0].slot = 0;
    device.host_mut().nested_hubs[0].marked = false;
    device.host_mut().nested_hubs[0].connected = true;
    device.host_mut().nested_hubs[0].root_change = PORT_CHANGE_CONNECTION;
    device.host_mut().post_hub_status_change(&[1 << 3]);
    match device.next_hub_change(&delay) {
        Ok(HubEvent::HubAttached(_)) => {}
        other => panic!("expected the hub tier to rebuild, got {other:?}"),
    }
    assert!(
        device.any_device_live(),
        "the keyboard behind the re-plugged hub is served again"
    );
}

#[test]
fn bring_up_serves_a_deep_hub_fanout_beyond_any_fixed_working_set() {
    // Nine downstream hubs hanging below the root-attached hub at once —
    // ten tracked hub tiers in all — with a device behind every tier:
    // wider than the recorded reference assembly (five downstream hubs,
    // ten mass-storage bridges, fifteen concurrently addressed devices)
    // and past the fixed per-controller budgets this engine used to carry
    // (sixteen device regions, eight tracked hubs), which silently left
    // whole tiers unserved. Each downstream hub claims a device-region
    // chunk for its own contexts plus a watch chunk, and each leaf claims
    // a region chunk of its own — eighteen device regions live at once —
    // all demand-allocated, bounded only by the controller's reported
    // slots and the bank's memory. Every tier must be installed, marked a
    // hub on its own slot, and hold an armed status-change watch, and
    // every leaf must be served: a tier the engine cannot track leaves
    // every device behind it undetected.
    const FANOUT_HUBS: u8 = 9;
    let mem = shared_mem();
    let mut device = started_device(MockXhci::with_hub_fanout(&mem, 12, FANOUT_HUBS), &mem);
    let delay = TestDelay::default();
    device.bring_up(&delay).expect("every hub tier comes up");

    for i in 0..usize::from(FANOUT_HUBS) {
        let hub = &device.host_mut().nested_hubs[i];
        assert_ne!(hub.slot, 0, "downstream hub {i} is addressed");
        assert!(hub.marked, "downstream hub {i}'s slot carries the Hub bit");
        assert_ne!(
            hub.int.dci, 0,
            "downstream hub {i}'s status-change watch is configured and armed"
        );
    }
    let live = (0..device.device_table_len())
        .filter(|&i| device.device_live(i))
        .count();
    assert_eq!(
        live,
        usize::from(FANOUT_HUBS),
        "every hub tier's leaf device is served"
    );
    assert!(device.hub_watch_active());
}

#[test]
fn detaching_a_downstream_device_releases_its_dma_chunk() {
    // The engine's per-device memory is demand-allocated: a served
    // device's region chunk is returned to the bank when the device
    // detaches, so a long-running controller's footprint tracks the
    // devices actually attached rather than growing monotonically.
    let mem = shared_mem();
    let mut mock = MockXhci::with_hub(&mem, 4, 4);
    mock.hub_downstream_status = 1 << 0;
    let mut device = started_device(mock, &mem);
    let delay = TestDelay::default();
    device
        .bring_up(&delay)
        .expect("the keyboard behind the hub is reached");
    assert!(device.device_live(1));
    let with_device = device.dma_ref().live_chunks();

    // Unplug the keyboard: the detach frees its table entry *and* its
    // DMA chunk.
    device.host_mut().hub_downstream_status = 0;
    device.host_mut().hub_downstream_change = PORT_CHANGE_CONNECTION;
    device.host_mut().post_hub_status_change(&[1 << 4]);
    assert_eq!(device.next_hub_change(&delay), Ok(HubEvent::Detached(1)));
    assert!(!device.device_live(1));
    assert_eq!(
        device.dma_ref().live_chunks(),
        with_device - 1,
        "the detached device's region chunk was returned to the bank"
    );
}
