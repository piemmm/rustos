//! BCM2711 PCIe root-complex MSI controller (Raspberry Pi 4).
//!
//! The Pi 4's VL805 xHCI host is a PCIe endpoint behind the BCM2711's
//! `brcm,bcm2711-pcie` root complex. An MSI from the VL805 is **not**
//! delivered to the GIC directly: the root complex contains an internal
//! MSI controller that captures the endpoint's message-write to a fixed
//! doorbell address and **demultiplexes up to 32 message vectors onto a
//! single shared GIC SPI** (the second `interrupts` entry of the
//! `pcie@7d500000` node, which is also its `msi-controller`/`msi-parent`).
//! When that SPI fires, software reads the controller's interrupt-status
//! register to learn which vector(s) fired, services each, and clears the
//! status bit — or the level-sensitive SPI re-asserts forever.
//!
//! This module is the register-level driver for that internal controller:
//! the doorbell/target programming, the per-vector mask/unmask/clear, the
//! pending-status read, and the `msi_message` an endpoint's PCI MSI
//! capability is programmed with. It is the BCM2711 analogue of
//! [`crate::gic`]: the register-offset math and message/demux encodings are
//! pure functions unit-tested on the host through the `MsiMmio` seam,
//! while the real MMIO reads/writes are the freestanding `VolatileMsiMmio`
//! over the **discovered** root-complex register base
//! (`configure` / `current`) — never a baked-in board constant.
//!
//! It lives in the aarch64 port (board specifics have exactly one home),
//! and is consumed kernel-side: the MSI demux is a *chained interrupt
//! handler*, which a user-space driver cannot be (the kernel masks the
//! line and wakes a parked task; it never runs driver code in interrupt
//! context). The user-space `drivers/bus/pcie_brcm` trains the link and
//! enumerates the VL805 over disjoint registers; the MSI controller
//! registers below are the kernel's.
//!
//! # Hardware constants
//!
//! The register offsets, the doorbell target address, and the data-config
//! magic are taken from the published Linux `pcie-brcmstb.c` driver and the
//! BCM2711 peripherals datasheet. They are isolated here as documented
//! constants for metal verification (QEMU models no Pi PCIe).

use core::sync::atomic::{AtomicU32, AtomicUsize, Ordering};

/// Number of distinct MSI message vectors the BCM2711 root-complex MSI
/// controller demultiplexes onto its one shared GIC SPI. A hardware fact
/// (the width of the `INTR2` status/mask registers), not a tunable
/// capacity.
pub const NUM_MSI_VECTORS: u32 = 32;

/// Root-complex register base in effect before discovery runs (`0`, an
/// invalid base): the BCM2711 PCIe register block has no fail-safe default
/// — there is no second board to fall back to — so an access before
/// [`configure`] resolves the real base from the device tree targets the
/// null page and faults closed rather than poking a fabricated address.
const NO_BASE: usize = 0;

/// Currently-selected root-complex register base, resolved from the
/// discovered `brcm,bcm2711-pcie` node at boot ([`configure`]). The MSI
/// registers below are accessed at fixed offsets from it.
static RC_BASE: AtomicUsize = AtomicUsize::new(NO_BASE);

/// Low half of the currently selected MSI doorbell target. The target depends
/// on the discovered inbound PCIe aperture size, so boot configures it beside
/// [`RC_BASE`] before the first MSI allocation.
static MSI_TARGET_LO: AtomicU32 = AtomicU32::new(MSI_TARGET_ADDR_LT_4GB_LO);

/// High half of the currently selected MSI doorbell target; see
/// [`MSI_TARGET_LO`].
static MSI_TARGET_HI: AtomicU32 = AtomicU32::new(MSI_TARGET_ADDR_LT_4GB_HI);

/// Point the MSI controller at the discovered root-complex register base and
/// select the doorbell target matching the discovered inbound aperture.
///
/// Called once on the boot path after the `brcm,bcm2711-pcie` node is
/// discovered, before the controller is initialised. `Release`/`Acquire` pairs
/// the stores with [`current`] and [`current_msi_target`]'s loads so the
/// freestanding MMIO path observes the resolved base and target.
pub fn configure(rc_base: usize, inbound_size: u64) {
    let target = msi_target_for_inbound_size(inbound_size);
    let (target_lo, target_hi) = msi_target_halves(target);
    MSI_TARGET_LO.store(target_lo, Ordering::Release);
    MSI_TARGET_HI.store(target_hi, Ordering::Release);
    RC_BASE.store(rc_base, Ordering::Release);
}

/// The root-complex register base currently in effect, or `0` before
/// [`configure`] has run.
#[must_use]
pub fn current() -> usize {
    RC_BASE.load(Ordering::Acquire)
}

/// The MSI doorbell target selected from the discovered inbound aperture.
#[must_use]
pub fn current_msi_target() -> u64 {
    (u64::from(MSI_TARGET_HI.load(Ordering::Acquire)) << 32)
        | u64::from(MSI_TARGET_LO.load(Ordering::Acquire))
}

// --- Register offsets (from the RC register base) -------------------------
//
// brcmstb PCIe (Linux `pcie-brcmstb.c`): the MSI doorbell BAR config and
// the per-vector INTR2 interrupt block.

/// `PCIE_MISC_MSI_BAR_CONFIG_LO`: the low 32 bits of the doorbell target
/// address an endpoint's MSI write must hit, OR'd with the enable bit
/// ([`MSI_BAR_CONFIG_LO_ENABLE`]).
pub const MSI_BAR_CONFIG_LO: usize = 0x4044;

/// `PCIE_MISC_MSI_BAR_CONFIG_HI`: the high 32 bits of the doorbell target
/// address.
pub const MSI_BAR_CONFIG_HI: usize = 0x4048;

/// `PCIE_MISC_MSI_DATA_CONFIG`: programs the expected MSI data pattern; the
/// low 16 bits ([`MSI_DATA_MAGIC`]) are the base the per-vector data word is
/// OR'd into.
pub const MSI_DATA_CONFIG: usize = 0x404c;

/// Enable bit of [`MSI_BAR_CONFIG_LO`]: set so the controller captures MSI
/// writes to the doorbell address.
pub const MSI_BAR_CONFIG_LO_ENABLE: u32 = 1 << 0;

/// `PCIE_MISC_MSI_DATA_CONFIG` value for the 32-vector configuration
/// (`PCIE_MISC_MSI_DATA_CONFIG_VAL_32`): the controller matches MSI writes
/// whose data is [`MSI_DATA_MAGIC`] OR the 5-bit vector index.
pub const MSI_DATA_CONFIG_VAL_32: u32 = 0xffe0_6540;

/// Low-16-bit magic of the MSI data word (`PCIE_MISC_MSI_DATA_CONFIG_VAL_32
/// & 0xffff`): an endpoint's MSI Data register is programmed with this OR
/// the vector index, so the controller routes the write to that vector.
pub const MSI_DATA_MAGIC: u32 = 0x6540;

/// Threshold at which the BCM2711 root complex uses the high MSI target.
pub const MSI_TARGET_GT_4GB_THRESHOLD: u64 = 0x1_0000_0000;

/// Doorbell target used when the inbound aperture is smaller than 4 GiB
/// (`BRCM_MSI_TARGET_ADDR_LT_4GB`). It is an RC-internal capture address, not
/// RAM.
pub const MSI_TARGET_ADDR_LT_4GB: u64 = 0x0000_0000_FFFF_FFFC;

/// Doorbell target used when the inbound aperture is at least 4 GiB
/// (`BRCM_MSI_TARGET_ADDR_GT_4GB`). Pi 4 firmware commonly exposes an 8 GiB
/// inbound aperture, so using the below-4GiB target there leaves endpoint MSI
/// writes visible to xHCI but invisible to the root-complex INTR2 status.
pub const MSI_TARGET_ADDR_GT_4GB: u64 = 0x0000_000F_FFFF_FFFC;

/// Low 32 bits of the below-4GiB doorbell target.
pub const MSI_TARGET_ADDR_LT_4GB_LO: u32 = 0xFFFF_FFFC;

/// High 32 bits of the below-4GiB doorbell target.
pub const MSI_TARGET_ADDR_LT_4GB_HI: u32 = 0x0000_0000;

/// Low 32 bits of the high doorbell target.
pub const MSI_TARGET_ADDR_GT_4GB_LO: u32 = 0xFFFF_FFFC;

/// High 32 bits of the high doorbell target.
pub const MSI_TARGET_ADDR_GT_4GB_HI: u32 = 0x0000_000F;

/// Select the BCM2711 MSI doorbell target for the discovered inbound aperture.
#[must_use]
pub const fn msi_target_for_inbound_size(inbound_size: u64) -> u64 {
    if inbound_size >= MSI_TARGET_GT_4GB_THRESHOLD {
        MSI_TARGET_ADDR_GT_4GB
    } else {
        MSI_TARGET_ADDR_LT_4GB
    }
}

fn msi_target_halves(target: u64) -> (u32, u32) {
    let bytes = target.to_le_bytes();
    (
        u32::from_le_bytes([bytes[0], bytes[1], bytes[2], bytes[3]]),
        u32::from_le_bytes([bytes[4], bytes[5], bytes[6], bytes[7]]),
    )
}

/// Base of the per-vector `INTR2` interrupt block within the RC register
/// window (`PCIE_MSI_INTR2_BASE`): a standard brcmstb level-2 interrupt
/// controller (status / clear / mask-set / mask-clear), one bit per vector.
pub const INTR2_BASE: usize = 0x4500;

/// `INTR2` interrupt-status register (offset from [`INTR2_BASE`]): bit `v`
/// set means MSI vector `v` is pending.
pub const INTR2_STATUS: usize = 0x00;

/// `INTR2` interrupt-clear register: write bit `v` (write-1-to-clear) to
/// clear vector `v`'s pending status after servicing it.
pub const INTR2_CLR: usize = 0x08;

/// `INTR2` mask-set register: write bit `v` to **mask** (disable) vector
/// `v` — the mask-before-wake step before a waiter observes the wake.
pub const INTR2_MASK_SET: usize = 0x10;

/// `INTR2` mask-clear register: write bit `v` to **unmask** (enable) vector
/// `v` — the re-arm after a driver has serviced a completion.
pub const INTR2_MASK_CLR: usize = 0x14;

/// All-ones word: mask or clear every vector at once during init.
const ALL_VECTORS: u32 = 0xFFFF_FFFF;

/// The architecture-built MSI message (doorbell address + data word) a PCI
/// endpoint's MSI capability is programmed with so its interrupt is routed
/// to `vector` through the BCM2711 root-complex MSI controller.
///
/// The address is the RC doorbell selected from the discovered inbound
/// aperture; the data is [`MSI_DATA_MAGIC`] OR the vector index, matching the
/// controller's programmed [`MSI_DATA_CONFIG_VAL_32`] pattern so the write
/// demultiplexes to `vector`. Returned as `(address, data)` so the kernel
/// binary can wrap it in the `abi-v1` `MsiMessage` the bus-driver
/// MSI-programming path copies verbatim, without this port depending on
/// `lib/abi`.
#[must_use]
pub fn msi_message(vector: u32) -> (u64, u32) {
    msi_message_for_target(vector, current_msi_target())
}

/// Build an MSI message for an explicitly selected BCM2711 doorbell target.
#[must_use]
pub const fn msi_message_for_target(vector: u32, target: u64) -> (u64, u32) {
    (target, MSI_DATA_MAGIC | (vector & (NUM_MSI_VECTORS - 1)))
}

/// `true` iff `vector` is a valid MSI vector index (`0..NUM_MSI_VECTORS`).
#[must_use]
pub const fn vector_in_range(vector: u32) -> bool {
    vector < NUM_MSI_VECTORS
}

/// MMIO access to the root-complex MSI registers.
///
/// The seam the pure controller logic is written against: the freestanding
/// `VolatileMsiMmio` performs real reads/writes at [`current`]`+ off`,
/// while host tests drive an in-memory mock, so the register sequencing is
/// unit-tested without hardware.
pub trait MsiMmio {
    /// Read the 32-bit register at byte `offset` from the RC base.
    fn read32(&self, offset: usize) -> u32;
    /// Write `value` to the 32-bit register at byte `offset` from the RC
    /// base.
    fn write32(&self, offset: usize, value: u32);
}

/// The BCM2711 root-complex MSI controller over an [`MsiMmio`] backend.
pub struct BrcmMsi<M: MsiMmio> {
    mmio: M,
}

impl<M: MsiMmio> BrcmMsi<M> {
    /// Wrap an [`MsiMmio`] backend.
    #[must_use]
    pub const fn new(mmio: M) -> Self {
        Self { mmio }
    }

    /// Initialise the controller: unmask and clear every supported vector,
    /// program the doorbell target address (enabled) and the data-config
    /// pattern.
    ///
    /// Idempotent and safe to call once the root complex is out of reset
    /// (the user-space `pcie_brcm` driver has trained the link before any
    /// endpoint that uses MSI is enumerated). This mirrors the BCM2711 Linux
    /// setup sequence: the INTR2 block must be able to latch supported vector
    /// messages, while the kernel IRQ table still masks each virtual vector
    /// before waking its waiter and the waiter re-arms it after draining.
    pub fn init(&self) {
        self.init_with_target(current_msi_target());
    }

    /// Initialise the controller with an explicitly selected MSI doorbell
    /// target. Host tests exercise both hardware aperture shapes through this
    /// entry point; production uses [`init`](Self::init), which reads the
    /// target selected by [`configure`].
    pub fn init_with_target(&self, target: u64) {
        let (target_lo, target_hi) = msi_target_halves(target);
        // Unmask the supported vectors and clear any stale pending status a
        // prior owner (firmware) left, before enabling the doorbell.
        self.mmio.write32(INTR2_BASE + INTR2_MASK_CLR, ALL_VECTORS);
        self.mmio.write32(INTR2_BASE + INTR2_CLR, ALL_VECTORS);
        // Program the doorbell target address (low half carries the enable
        // bit) and the data-config pattern the per-vector data matches.
        self.mmio
            .write32(MSI_BAR_CONFIG_LO, target_lo | MSI_BAR_CONFIG_LO_ENABLE);
        self.mmio.write32(MSI_BAR_CONFIG_HI, target_hi);
        self.mmio.write32(MSI_DATA_CONFIG, MSI_DATA_CONFIG_VAL_32);
    }

    /// Mask (disable delivery of) MSI `vector`. A no-op for an
    /// out-of-range vector (fail closed — never touch a foreign bit).
    pub fn mask(&self, vector: u32) {
        if vector_in_range(vector) {
            self.mmio.write32(INTR2_BASE + INTR2_MASK_SET, 1 << vector);
        }
    }

    /// Unmask (re-arm) MSI `vector` for the next interrupt. A no-op for an
    /// out-of-range vector.
    pub fn unmask(&self, vector: u32) {
        if vector_in_range(vector) {
            self.mmio.write32(INTR2_BASE + INTR2_MASK_CLR, 1 << vector);
        }
    }

    /// Clear MSI `vector`'s pending status after it has been serviced, so
    /// the level-sensitive shared GIC SPI deasserts. A no-op for an
    /// out-of-range vector.
    pub fn clear(&self, vector: u32) {
        if vector_in_range(vector) {
            self.mmio.write32(INTR2_BASE + INTR2_CLR, 1 << vector);
        }
    }

    /// Read the pending-vector bitmap (`INTR2 STATUS`): bit `v` set means
    /// MSI vector `v` fired and is awaiting service.
    #[must_use]
    pub fn pending(&self) -> u32 {
        self.mmio.read32(INTR2_BASE + INTR2_STATUS)
    }
}

/// Iterate the vector indices set in a pending-status bitmap, lowest first.
///
/// The chained GIC-SPI handler calls [`BrcmMsi::pending`] then walks this to
/// fire and clear each pending vector. A pure function so the fan-out order
/// is unit-tested without hardware.
pub fn pending_vectors(status: u32) -> impl Iterator<Item = u32> {
    (0..NUM_MSI_VECTORS).filter(move |v| status & (1 << v) != 0)
}

/// Bare-metal [`MsiMmio`] over the **discovered** root-complex register
/// base ([`current`]).
///
/// A zero-sized handle, like [`crate::gic::VolatileGicMmio`]: each access
/// reads the base [`configure`] resolved from the device tree, so the
/// driver always targets the discovered window and the handle is
/// const-constructible (it can live in a `static`). Compiled only for the
/// freestanding aarch64 target; host builds use the test mock.
#[cfg(all(target_arch = "aarch64", target_os = "none"))]
pub struct VolatileMsiMmio;

#[cfg(all(target_arch = "aarch64", target_os = "none"))]
impl MsiMmio for VolatileMsiMmio {
    fn read32(&self, offset: usize) -> u32 {
        // SAFETY: `offset` addresses an MSI register within the discovered
        // BCM2711 root-complex MMIO window the kernel owns; `current()` is
        // the base resolved from the device tree before any access.
        unsafe { core::ptr::read_volatile((current() + offset) as *const u32) }
    }
    fn write32(&self, offset: usize, value: u32) {
        // SAFETY: as `read32`, but a 32-bit store.
        unsafe { core::ptr::write_volatile((current() + offset) as *mut u32, value) }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use alloc::vec::Vec;
    use core::cell::RefCell;

    extern crate alloc;

    /// In-memory [`MsiMmio`] recording every write and answering reads from
    /// a settable status register, so the init/mask/clear sequencing and
    /// the demux are asserted without hardware.
    #[derive(Default)]
    struct MockMsiMmio {
        writes: RefCell<Vec<(usize, u32)>>,
        status: RefCell<u32>,
    }

    impl MsiMmio for MockMsiMmio {
        fn read32(&self, offset: usize) -> u32 {
            if offset == INTR2_BASE + INTR2_STATUS {
                *self.status.borrow()
            } else {
                0
            }
        }
        fn write32(&self, offset: usize, value: u32) {
            self.writes.borrow_mut().push((offset, value));
        }
    }

    impl MockMsiMmio {
        fn wrote(&self, offset: usize, value: u32) -> bool {
            self.writes.borrow().contains(&(offset, value))
        }
    }

    #[test]
    fn msi_message_encodes_target_and_vector() {
        // The data is the magic OR the vector index, matching the programmed
        // data-config pattern; the address comes from the discovered aperture.
        assert_eq!(
            msi_message_for_target(0, MSI_TARGET_ADDR_GT_4GB),
            (MSI_TARGET_ADDR_GT_4GB, MSI_DATA_MAGIC)
        );
        assert_eq!(
            msi_message_for_target(1, MSI_TARGET_ADDR_GT_4GB),
            (MSI_TARGET_ADDR_GT_4GB, MSI_DATA_MAGIC | 1)
        );
        assert_eq!(
            msi_message_for_target(31, MSI_TARGET_ADDR_GT_4GB),
            (MSI_TARGET_ADDR_GT_4GB, MSI_DATA_MAGIC | 31)
        );
    }

    #[test]
    fn msi_target_follows_discovered_inbound_aperture() {
        assert_eq!(msi_target_for_inbound_size(0), MSI_TARGET_ADDR_LT_4GB);
        assert_eq!(
            msi_target_for_inbound_size(MSI_TARGET_GT_4GB_THRESHOLD - 1),
            MSI_TARGET_ADDR_LT_4GB
        );
        assert_eq!(
            msi_target_for_inbound_size(MSI_TARGET_GT_4GB_THRESHOLD),
            MSI_TARGET_ADDR_GT_4GB
        );
        assert_eq!(
            msi_target_for_inbound_size(0x2_0000_0000),
            MSI_TARGET_ADDR_GT_4GB
        );
    }

    #[test]
    fn init_unmasks_clears_then_enables_the_below_4g_doorbell() {
        let msi = BrcmMsi::new(MockMsiMmio::default());
        msi.init_with_target(MSI_TARGET_ADDR_LT_4GB);
        let m = &msi.mmio;
        // Every supported vector unmasked and cleared.
        assert!(m.wrote(INTR2_BASE + INTR2_MASK_CLR, ALL_VECTORS));
        assert!(m.wrote(INTR2_BASE + INTR2_CLR, ALL_VECTORS));
        // Doorbell target programmed with the enable bit, and the data
        // pattern set.
        assert!(m.wrote(
            MSI_BAR_CONFIG_LO,
            MSI_TARGET_ADDR_LT_4GB_LO | MSI_BAR_CONFIG_LO_ENABLE
        ));
        assert!(m.wrote(MSI_BAR_CONFIG_HI, MSI_TARGET_ADDR_LT_4GB_HI));
        assert!(m.wrote(MSI_DATA_CONFIG, MSI_DATA_CONFIG_VAL_32));
    }

    #[test]
    fn init_programs_the_high_doorbell_for_large_inbound_apertures() {
        let msi = BrcmMsi::new(MockMsiMmio::default());
        msi.init_with_target(MSI_TARGET_ADDR_GT_4GB);
        let m = &msi.mmio;
        assert!(m.wrote(
            MSI_BAR_CONFIG_LO,
            MSI_TARGET_ADDR_GT_4GB_LO | MSI_BAR_CONFIG_LO_ENABLE
        ));
        assert!(m.wrote(MSI_BAR_CONFIG_HI, MSI_TARGET_ADDR_GT_4GB_HI));
        assert!(m.wrote(MSI_DATA_CONFIG, MSI_DATA_CONFIG_VAL_32));
    }

    #[test]
    fn mask_unmask_clear_touch_only_the_addressed_vector() {
        let msi = BrcmMsi::new(MockMsiMmio::default());
        msi.mask(5);
        msi.unmask(5);
        msi.clear(5);
        let m = &msi.mmio;
        assert!(m.wrote(INTR2_BASE + INTR2_MASK_SET, 1 << 5));
        assert!(m.wrote(INTR2_BASE + INTR2_MASK_CLR, 1 << 5));
        assert!(m.wrote(INTR2_BASE + INTR2_CLR, 1 << 5));
    }

    #[test]
    fn out_of_range_vector_writes_nothing_fail_closed() {
        let msi = BrcmMsi::new(MockMsiMmio::default());
        msi.mask(NUM_MSI_VECTORS);
        msi.unmask(NUM_MSI_VECTORS + 10);
        msi.clear(u32::MAX);
        assert!(msi.mmio.writes.borrow().is_empty());
    }

    #[test]
    fn pending_reads_the_status_register() {
        let msi = BrcmMsi::new(MockMsiMmio::default());
        *msi.mmio.status.borrow_mut() = 0b1010;
        assert_eq!(msi.pending(), 0b1010);
    }

    #[test]
    fn pending_vectors_yields_set_bits_lowest_first() {
        let got: Vec<u32> = pending_vectors(0b1001_0010).collect();
        assert_eq!(got, alloc::vec![1, 4, 7]);
    }

    #[test]
    fn pending_vectors_empty_status_yields_nothing() {
        assert_eq!(pending_vectors(0).count(), 0);
    }
}
