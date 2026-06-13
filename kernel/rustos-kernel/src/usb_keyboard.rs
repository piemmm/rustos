//! VL805/xHCI USB-keyboard composition (`plans/PI.md` P10).
//!
//! On the Raspberry Pi 4 (BCM2711) the USB-A ports hang off a VL805 xHCI
//! host controller behind the `SoC`'s PCIe root complex, whose link ships
//! **down** and whose config space is windowed (not flat ECAM). Bringing a
//! USB keyboard to the video-console login therefore means composing four
//! loadable driver crates into one chain:
//!
//! 1. [`rustos_drv_bus_pcie_brcm`] resets the BCM2711 root complex and
//!    trains its link, programmed with the discovered inbound/outbound
//!    address windows;
//! 2. [`rustos_drv_bus_pci::mechanism_brcm`] enumerates the VL805 over the
//!    windowed config accessor built on the same register window;
//! 3. [`rustos_drv_bus_usb`] maps the controller's BAR, carves its
//!    device-shared DMA region, and brings the xHCI controller up; and
//! 4. [`rustos_drv_input_usb_hid`] decodes the boot keyboard's reports and
//!    turns each key into the console (tty) bytes a terminal sends.
//!
//! Each of those crates is a separate driver and may not name another
//! (`AGENTS.md` §17.4 — `deps-check` forbids driver→driver edges). The
//! image-assembly binary (`rustos-kernel`, `Layer::Tooling`) is the one
//! place permitted to name them all, so the composition lives here, exactly
//! as the virtio bring-up does (`crate::virtio_boot`). The engine itself is
//! architecture-neutral — it consumes only the `lib/abi` driver seams and
//! the discovered [`HwNode`] — so it compiles and is host-tested on the CI
//! host; the aarch64 boot path supplies the concrete [`DriverHost`]
//! (`KernelMmioMapper` + per-driver DMA host) and the generic-timer-backed
//! [`Delay`] that drive it on metal.
//!
//! # No QEMU vertical
//!
//! QEMU models no Pi PCIe link timing or USB (`AGENTS.md` §0.4 / §2.1), so
//! the host tests prove the composition, its window assembly, and its
//! fail-closed paths up to the controller hand-off, where the inert mock
//! register window faults — exactly the metal boundary. The live link
//! training, a real BAR answering a plausible `CAPLENGTH`, and a keyboard
//! driving the login are the on-metal acceptance items.

use rustos_abi::driver::dma::DmaSlab;
use rustos_abi::driver::virtio::VirtioHost;
use rustos_abi::{
    CapabilityId, DriverError, DriverHost, DriverKind, HwNode, HwResourceKind, MmioMapper,
    RegisterWindow,
};
use rustos_caps::CapabilitySet;
use rustos_drv_bus_pcie_brcm::{self as pcie_brcm, Delay, PcieWindows};
use rustos_drv_bus_usb::device::UsbDevice;
use rustos_drv_input_usb_hid::{BootKeyboard, ConsoleSink};
use rustos_kernel_core::ConsoleInput;

/// The enumerated boot keyboard the bring-up chain yields: a
/// [`BootKeyboard`] decoding reports out of the started [`UsbDevice`]
/// (the xHCI controller over its mapped register window + DMA region).
pub type KeyboardChain = BootKeyboard<UsbDevice<RegisterWindow, DmaSlab>>;

/// The discovered inputs the VL805 bring-up needs, all read from the
/// `brcm,bcm2711-pcie` [`HwNode`] (`AGENTS.md` §18.1) — never compiled-in.
#[derive(Copy, Clone, Debug, Eq, PartialEq)]
pub struct PcieBringup {
    /// CPU-physical base of the PCIe controller register block (the
    /// translated `reg` MMIO window).
    pub regs_phys: u64,
    /// The inbound (`dma-ranges`) and outbound (`ranges`) address windows
    /// the root complex is programmed with.
    pub windows: PcieWindows,
    /// Exclusive upper bound of the CPU-physical window devices behind the
    /// bridge may reach: the xHCI DMA carve must lie wholly below it
    /// (`AGENTS.md` §5.4).
    pub dma_aperture_top: u64,
}

/// Why a `brcm,bcm2711-pcie` [`HwNode`] could not be turned into a
/// [`PcieBringup`]: a required discovered resource is absent. Each is a
/// fail-closed refusal — the chain never invents a window (`AGENTS.md`
/// §2.9 / §18.5).
#[derive(Copy, Clone, Debug, Eq, PartialEq)]
pub enum BringupError {
    /// The node carries no controller register (`Mmio`) window.
    NoControllerWindow,
    /// The node carries no inbound-DMA aperture (`Dma`) resource.
    NoInboundAperture,
    /// The node carries no outbound (`BusWindow`) resource.
    NoOutboundWindow,
}

/// Assemble the VL805 bring-up inputs from a discovered
/// `brcm,bcm2711-pcie` [`HwNode`].
///
/// The node carries three resources the chain needs (all discovered by
/// `kernel/arch/aarch64::platform`, `AGENTS.md` §18.1):
///
/// * the controller register window — the first [`Mmio`](HwResourceKind::Mmio)
///   resource, whose base is [`PcieBringup::regs_phys`];
/// * the inbound viewport — the [`Dma`](HwResourceKind::Dma) resource,
///   whose `base` is the exclusive DMA-reachability top
///   ([`PcieBringup::dma_aperture_top`]), `length` the viewport size, and
///   `translated_base` the PCIe-space base the inbound BAR is programmed
///   at; and
/// * the outbound window — the [`BusWindow`](HwResourceKind::BusWindow)
///   resource (`base` CPU aperture, `length` size, `translated_base` the
///   PCIe-space base it maps to).
///
/// # Errors
///
/// A [`BringupError`] naming the first missing resource; the inputs are
/// never partially assembled (`AGENTS.md` §5.4).
pub fn pcie_bringup_from_node(node: &HwNode) -> Result<PcieBringup, BringupError> {
    let resources = node.resources();
    let find = |kind| resources.iter().find(|r| r.kind() == Some(kind));

    let regs = find(HwResourceKind::Mmio).ok_or(BringupError::NoControllerWindow)?;
    let inbound = find(HwResourceKind::Dma).ok_or(BringupError::NoInboundAperture)?;
    let outbound = find(HwResourceKind::BusWindow).ok_or(BringupError::NoOutboundWindow)?;

    Ok(PcieBringup {
        regs_phys: regs.base(),
        dma_aperture_top: inbound.base(),
        windows: PcieWindows {
            inbound_pcie_base: inbound.translated_base(),
            inbound_size: inbound.length(),
            outbound_cpu_base: outbound.base(),
            outbound_pcie_base: outbound.translated_base(),
            outbound_size: outbound.length(),
        },
    })
}

/// A [`DriverHost`] view assembled for the in-kernel VL805 chain: the
/// capabilities the bus-driver task holds plus the kernel's
/// capability-gated MMIO mapper and per-driver DMA host.
///
/// The bring-up driver crates consume the host only through this trait, so
/// it cannot widen its own authority (`AGENTS.md` §4 / §8): every
/// [`MmioMapper::map_window`] and [`VirtioHost::alloc_dma_zeroed`] call is
/// re-checked kernel-side against the same capabilities (`AGENTS.md`
/// §5.4). The view borrows the mapper and DMA host for `'a`; the kernel
/// reclaims every window and DMA pool when they are torn down at unload.
pub struct ChainHost<'a> {
    capabilities: CapabilitySet,
    mmio: &'a dyn MmioMapper,
    dma: &'a dyn VirtioHost,
}

impl<'a> ChainHost<'a> {
    /// Build the view over the bus-driver task's `capabilities` and the
    /// kernel's `mmio` mapper and `dma` host.
    #[must_use]
    pub fn new(
        capabilities: CapabilitySet,
        mmio: &'a dyn MmioMapper,
        dma: &'a dyn VirtioHost,
    ) -> Self {
        Self {
            capabilities,
            mmio,
            dma,
        }
    }
}

impl DriverHost for ChainHost<'_> {
    fn has_capability(&self, cap: CapabilityId) -> bool {
        self.capabilities.contains(cap)
    }

    fn kind(&self) -> DriverKind {
        // The composition runs inside the kernel image, so the chain's
        // drivers observe an in-kernel host.
        DriverKind::InKernel
    }

    fn virtio_host(&self) -> Option<&dyn VirtioHost> {
        Some(self.dma)
    }

    fn mmio_mapper(&self) -> Option<&dyn MmioMapper> {
        Some(self.mmio)
    }
}

/// A [`ConsoleSink`] that injects produced keyboard bytes into a console's
/// input queue (`AGENTS.md` §20, `plans/PI.md` P11).
///
/// The video console points both its read half and its injected-input
/// half at one [`ConsoleInput`] queue, so bytes written here are drained
/// by the login reading that console. The queue never blocks: a write that
/// does not all fit (the type-ahead ring is full because the login has not
/// yet drained it) keeps what fits and drops the remainder rather than
/// busy-spinning (`AGENTS.md` §2.1) — a bounded type-ahead overflow, the
/// same shape as a full UART receive FIFO.
pub struct QueueConsoleSink<'a> {
    queue: &'a dyn ConsoleInput,
}

impl<'a> QueueConsoleSink<'a> {
    /// Build a sink delivering to `queue` (the target console's injected
    /// input source).
    #[must_use]
    pub fn new(queue: &'a dyn ConsoleInput) -> Self {
        Self { queue }
    }
}

impl ConsoleSink for QueueConsoleSink<'_> {
    fn write(&mut self, bytes: &[u8]) -> Result<(), DriverError> {
        let mut offset = 0;
        while offset < bytes.len() {
            let pushed = self
                .queue
                .push(&bytes[offset..])
                .map_err(|_| DriverError::DeviceFault)?;
            if pushed == 0 {
                // The ring is full and the consumer has not drained it;
                // drop the rest of this key's bytes rather than spin
                // (the queue is the type-ahead bound, §2.1 / §24.4).
                break;
            }
            offset += pushed;
        }
        Ok(())
    }
}

/// Bring the VL805 keyboard online over `host`, from the discovered
/// `bringup` inputs, using `delay` for the link bring-up's timed waits.
///
/// Runs the full chain: train the BCM2711 root-complex link
/// ([`pcie_brcm::wiring::open_discovered`]), build the windowed PCI config
/// accessor over the same register window
/// ([`rustos_drv_bus_pci::mechanism_brcm`]), bring the VL805 xHCI up
/// ([`rustos_drv_bus_usb::wiring::open_discovered`]), and enumerate the
/// first connected root-hub port as a boot keyboard. The returned
/// [`KeyboardChain`] is then polled with [`rustos_drv_input_usb_hid::pump_once`]
/// in the driver's service loop, feeding the produced bytes to a
/// [`QueueConsoleSink`].
///
/// # Errors
///
/// * [`DriverError::PermissionDenied`] if `host` did not grant
///   [`CapabilityId::MMIO_MAP`].
/// * Any error of the link bring-up (the controller is not a root port or
///   the link never trains), the VL805 bring-up (no USB function, a DMA
///   carve above the aperture, a mapping failure), or the enumeration
///   ([`DriverError::NotFound`] for an empty root hub). Every failure is
///   fail-closed (`AGENTS.md` §5.4); nothing is left half-configured.
///
/// # Capabilities
///
/// Requires [`CapabilityId::MMIO_MAP`] (the register windows and the BAR)
/// and the host's DMA capability (the xHCI DMA carve), both re-checked
/// kernel-side at each map/allocation (`AGENTS.md` §5.4).
pub fn bring_up_keyboard(
    host: &dyn DriverHost,
    bringup: &PcieBringup,
    delay: &dyn Delay,
) -> Result<KeyboardChain, DriverError> {
    let rc = pcie_brcm::wiring::open_discovered(host, bringup.regs_phys, &bringup.windows, delay)?;
    // Recover the trained controller's register window and reach the VL805
    // through the BCM2711 windowed config accessor built over it.
    let bus = rustos_drv_bus_pci::mechanism_brcm(rc.into_regs());
    let mut usb =
        rustos_drv_bus_usb::wiring::open_discovered(host, &bus, bringup.dma_aperture_top)?;
    usb.enumerate_first_connected()?;
    Ok(BootKeyboard::new(usb))
}

#[cfg(test)]
mod tests {
    use super::*;

    use alloc::boxed::Box;
    use core::ptr::NonNull;

    use rustos_abi::driver::dma::PoolId;
    use rustos_abi::driver::mmio::MmioMapError;
    use rustos_abi::{HwDeviceClass, HwResource};
    use rustos_kernel_core::{ConsoleInputQueue, ConsoleRead};

    /// The Pi 4 discovered values: controller `reg`, inbound `dma-ranges`
    /// (PCIe base 0, 3 GiB), outbound `ranges` (CPU `0x6_0000_0000` → PCIe
    /// `0xc000_0000`, 1 GiB).
    const REGS_PHYS: u64 = 0xfd50_0000;
    const APERTURE_TOP: u64 = 0xc000_0000;
    const OUTBOUND_CPU: u64 = 0x6_0000_0000;
    const OUTBOUND_PCIE: u64 = 0xc000_0000;
    const OUTBOUND_SIZE: u64 = 0x4000_0000;

    fn pcie_node() -> HwNode {
        let mut node = HwNode::new(9, 1, HwDeviceClass::Bus);
        node.push_resource(HwResource::mmio(REGS_PHYS, 0x9310))
            .unwrap();
        node.push_resource(HwResource::dma_translated(APERTURE_TOP, APERTURE_TOP, 0))
            .unwrap();
        node.push_resource(HwResource::bus_window(
            OUTBOUND_CPU,
            OUTBOUND_SIZE,
            OUTBOUND_PCIE,
        ))
        .unwrap();
        node
    }

    #[test]
    fn bringup_inputs_are_assembled_from_the_node() {
        let bringup = pcie_bringup_from_node(&pcie_node()).expect("all resources present");
        assert_eq!(bringup.regs_phys, REGS_PHYS);
        assert_eq!(bringup.dma_aperture_top, APERTURE_TOP);
        assert_eq!(bringup.windows.inbound_pcie_base, 0);
        assert_eq!(bringup.windows.inbound_size, APERTURE_TOP);
        assert_eq!(bringup.windows.outbound_cpu_base, OUTBOUND_CPU);
        assert_eq!(bringup.windows.outbound_pcie_base, OUTBOUND_PCIE);
        assert_eq!(bringup.windows.outbound_size, OUTBOUND_SIZE);
    }

    #[test]
    fn bringup_carries_a_nonzero_inbound_pcie_base() {
        // A viewport not anchored at PCIe address 0: the translation rides
        // the DMA resource's far-side base, distinct from the CPU top.
        let mut node = HwNode::new(9, 1, HwDeviceClass::Bus);
        node.push_resource(HwResource::mmio(REGS_PHYS, 0x9310))
            .unwrap();
        node.push_resource(HwResource::dma_translated(
            APERTURE_TOP,
            APERTURE_TOP,
            0x4000_0000,
        ))
        .unwrap();
        node.push_resource(HwResource::bus_window(
            OUTBOUND_CPU,
            OUTBOUND_SIZE,
            OUTBOUND_PCIE,
        ))
        .unwrap();
        let bringup = pcie_bringup_from_node(&node).expect("resources present");
        assert_eq!(bringup.windows.inbound_pcie_base, 0x4000_0000);
        assert_eq!(bringup.dma_aperture_top, APERTURE_TOP);
    }

    #[test]
    fn bringup_fails_closed_on_each_missing_resource() {
        // No controller register window.
        let mut node = HwNode::new(9, 1, HwDeviceClass::Bus);
        node.push_resource(HwResource::dma_translated(APERTURE_TOP, APERTURE_TOP, 0))
            .unwrap();
        node.push_resource(HwResource::bus_window(
            OUTBOUND_CPU,
            OUTBOUND_SIZE,
            OUTBOUND_PCIE,
        ))
        .unwrap();
        assert_eq!(
            pcie_bringup_from_node(&node),
            Err(BringupError::NoControllerWindow)
        );

        // No inbound aperture.
        let mut node = HwNode::new(9, 1, HwDeviceClass::Bus);
        node.push_resource(HwResource::mmio(REGS_PHYS, 0x9310))
            .unwrap();
        node.push_resource(HwResource::bus_window(
            OUTBOUND_CPU,
            OUTBOUND_SIZE,
            OUTBOUND_PCIE,
        ))
        .unwrap();
        assert_eq!(
            pcie_bringup_from_node(&node),
            Err(BringupError::NoInboundAperture)
        );

        // No outbound window.
        let mut node = HwNode::new(9, 1, HwDeviceClass::Bus);
        node.push_resource(HwResource::mmio(REGS_PHYS, 0x9310))
            .unwrap();
        node.push_resource(HwResource::dma_translated(APERTURE_TOP, APERTURE_TOP, 0))
            .unwrap();
        assert_eq!(
            pcie_bringup_from_node(&node),
            Err(BringupError::NoOutboundWindow)
        );
    }

    #[test]
    fn queue_console_sink_delivers_and_reads_back() {
        let queue = ConsoleInputQueue::new();
        let mut sink = QueueConsoleSink::new(&queue);
        sink.write(b"hi").expect("delivered");
        let mut buf = [0u8; 8];
        let read = queue.read(&mut buf).expect("read");
        assert_eq!(&buf[..read], b"hi");
    }

    #[test]
    fn queue_console_sink_drops_overflow_without_spinning() {
        let queue = ConsoleInputQueue::new();
        let mut sink = QueueConsoleSink::new(&queue);
        // Write more than the ring holds: the call returns (no spin) and
        // the ring is filled to capacity, the surplus dropped.
        let flood = alloc::vec![b'x'; rustos_kernel_core::CONSOLE_INPUT_QUEUE_CAPACITY + 32];
        sink.write(&flood).expect("returns without spinning");
        let mut buf = alloc::vec![0u8; flood.len()];
        let read = queue.read(&mut buf).expect("read");
        assert_eq!(read, rustos_kernel_core::CONSOLE_INPUT_QUEUE_CAPACITY);
    }

    /// Leak a `len`-byte, 4-byte-aligned buffer (the mock host's `'static`
    /// storage, mirroring the usb `wiring_tests` strategy).
    fn leak_aligned(len: usize) -> NonNull<u8> {
        let words = len.div_ceil(4).max(1);
        let buf: Box<[u32]> = alloc::vec![0u32; words].into_boxed_slice();
        NonNull::new(Box::leak(buf).as_mut_ptr().cast::<u8>()).expect("non-null")
    }

    struct MockMapper {
        grant: bool,
    }
    impl MmioMapper for MockMapper {
        fn map_window(&self, phys_base: u64, len: usize) -> Result<RegisterWindow, MmioMapError> {
            if !self.grant {
                return Err(MmioMapError::CapabilityMissing);
            }
            let base = leak_aligned(len);
            // SAFETY: `base` covers `len` zeroed bytes, is 4-byte aligned,
            // lives for the whole test process (leaked), and is unaliased.
            Ok(unsafe { RegisterWindow::from_mapping(phys_base, base, len) })
        }
    }

    struct MockDmaHost;
    impl VirtioHost for MockDmaHost {
        fn alloc_dma_zeroed(&self, size: usize) -> Result<DmaSlab, DriverError> {
            let ptr = leak_aligned(size);
            // SAFETY: `ptr` covers `size` zeroed bytes and lives for the
            // whole test process; the device-visible base is in-aperture
            // (below `APERTURE_TOP`). Drop is a no-op (`from_leaked`).
            Ok(unsafe { DmaSlab::from_leaked(0x1000_0000, ptr, size, PoolId::MOCK, 0) })
        }
        fn notify_wait(&self, _queue_index: u16) {}
    }

    fn caps(set: &[CapabilityId]) -> CapabilitySet {
        let mut caps = CapabilitySet::empty();
        for c in set {
            caps.insert(*c);
        }
        caps
    }

    struct NoDelay;
    impl Delay for NoDelay {
        fn delay_us(&self, _us: u32) {}
    }

    #[test]
    fn chain_host_reports_caps_mapper_and_dma() {
        let mapper = MockMapper { grant: true };
        let dma = MockDmaHost;
        let host = ChainHost::new(
            caps(&[CapabilityId::MMIO_MAP, CapabilityId::MEM_DMA]),
            &mapper,
            &dma,
        );
        assert!(host.has_capability(CapabilityId::MMIO_MAP));
        assert!(host.has_capability(CapabilityId::MEM_DMA));
        assert!(!host.has_capability(CapabilityId::DRV_LOAD));
        assert_eq!(host.kind(), DriverKind::InKernel);
        assert!(host.virtio_host().is_some());
        assert!(host.mmio_mapper().is_some());
    }

    #[test]
    fn bring_up_requires_the_mmio_capability() {
        // A host without MMIO_MAP fails the chain closed at the very first
        // step (the PCIe controller-window map), before any hardware.
        let mapper = MockMapper { grant: false };
        let dma = MockDmaHost;
        let host = ChainHost::new(caps(&[CapabilityId::MEM_DMA]), &mapper, &dma);
        let bringup = pcie_bringup_from_node(&pcie_node()).unwrap();
        // `.err()` drops the unenumerated keyboard (which is neither
        // `Debug` nor `PartialEq`) and compares only the error.
        assert_eq!(
            bring_up_keyboard(&host, &bringup, &NoDelay).err(),
            Some(DriverError::PermissionDenied)
        );
    }

    #[test]
    fn bring_up_reaches_the_pcie_link_bringup_over_a_mapped_window() {
        // With the capability granted the chain maps the controller window
        // and runs the BCM2711 root-complex bring-up; over the inert zeroed
        // mock window the root-port status check reads 0 and fails closed
        // with DeviceFault — exactly the metal boundary the host test can
        // reach (`AGENTS.md` §0.4). That the chain got this far proves the
        // window was assembled and mapped and pcie_brcm was driven.
        let mapper = MockMapper { grant: true };
        let dma = MockDmaHost;
        let host = ChainHost::new(
            caps(&[CapabilityId::MMIO_MAP, CapabilityId::MEM_DMA]),
            &mapper,
            &dma,
        );
        let bringup = pcie_bringup_from_node(&pcie_node()).unwrap();
        assert_eq!(
            bring_up_keyboard(&host, &bringup, &NoDelay).err(),
            Some(DriverError::DeviceFault)
        );
    }
}
