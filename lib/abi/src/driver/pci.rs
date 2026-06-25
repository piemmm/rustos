//! Generic PCI/PCIe transport seam (`abi-v1`).
//!
//! [`VirtioPciBus`](super::virtio_pci::VirtioPciBus) provisions the
//! *virtio*-specific register windows a virtio transport needs. A
//! non-virtio PCI device — an xHCI USB host controller, say — needs a
//! different, smaller surface: the physical window of one of its base
//! address registers (BARs), and the function's bus-mastering bit set
//! so it may issue the upstream DMA its rings live in.
//!
//! [`PciBus`] is that surface. The PCI configuration-access library
//! (`lib/pci`) implements it; a device-class driver (`drivers/bus/usb`,
//! …) or a composing host reaches the bus through a `&dyn PciBus` rather
//! than naming the concrete bus type (PCI config
//! access is shared `lib/*` logic, and one driver never names another).
//! [`Bus`] is a supertrait so a single trait object can both enumerate
//! the bus (to pick the function) and provision it.
//!
//! Like every other `lib/abi` item the trait is held to the ABI
//! discipline; while `abi-v1` is unfrozen it may still evolve in place, every caller updated in the same change.

use super::bus::Bus;
use super::msix::MsiMessage;
use super::{DriverError, MmioMapper, RegisterWindow};
use crate::HwNode;

/// A PCI bus that can provision a non-virtio function's resources.
///
/// # Capabilities
///
/// [`map_bar_window`](Self::map_bar_window) routes through the supplied
/// [`MmioMapper`], which enforces
/// [`CapabilityId::MMIO_MAP`](crate::CapabilityId::MMIO_MAP); the
/// implementation synthesises no pointer itself (no
/// ambient authority). [`enable_bus_master`](Self::enable_bus_master)
/// touches only the function's own configuration space, which the bus
/// driver already reaches by holding its [`DriverHandle`](crate::driver::DriverHandle).
pub trait PciBus: Bus {
    /// Resolve the memory BAR at `bar_index` on function `bdf` and ask
    /// `mapper` to map it, returning the resulting [`RegisterWindow`].
    ///
    /// This is the hand-off a memory-mapped device driver consumes:
    /// the bus driver reads the BAR's physical base and probed size
    /// from configuration space and asks the kernel's MMIO-map facility
    /// for a window over exactly that region. The driver never
    /// synthesises a pointer — the kernel allocates and validates the
    /// mapping.
    ///
    /// # Errors
    ///
    /// * [`DriverError::NotFound`] — no BAR with `bar_index` exists, or
    ///   the BAR is unused (probed size zero).
    /// * [`DriverError::Unsupported`] — the BAR is an I/O-port BAR
    ///   (reached through port I/O, not a mapped window), or the
    ///   function is not a type-0 header.
    /// * [`DriverError::LengthOutOfRange`] — the BAR size does not fit
    ///   in `usize` on this target.
    /// * [`DriverError::PermissionDenied`] — the caller does not hold
    ///   [`CapabilityId::MMIO_MAP`](crate::CapabilityId::MMIO_MAP)
    ///   (propagated from the mapper).
    fn map_bar_window(
        &self,
        bdf: u64,
        bar_index: u8,
        mapper: &dyn MmioMapper,
    ) -> Result<RegisterWindow, DriverError>;

    /// Enable memory-space decoding and bus-mastering on function
    /// `bdf` (PCI Local Bus 3.0 §6.2.2).
    ///
    /// Firmware leaves the Bus Master Enable bit clear, so a function
    /// whose BAR is mapped but whose bus-master bit is clear can never
    /// issue the upstream memory transactions its DMA rings depend on.
    /// A driver that programs a device for DMA calls this once before
    /// it expects the controller to touch host memory.
    ///
    /// The status half of the command/status register is RW1C, so the
    /// implementation must preserve the low command bits, write the
    /// high status bits as zero, and OR in the two enable bits.
    ///
    /// # Errors
    ///
    /// * [`DriverError::DeviceFault`] if the configuration write cannot
    ///   be completed by the bus transport.
    fn enable_bus_master(&self, bdf: u64) -> Result<(), DriverError>;

    /// Assign a memory base to the BAR at `bar_index` on function
    /// `bdf` if it is currently **unassigned**, placing it inside the
    /// PCIe-bus window `[window_base, window_base + window_size)` and
    /// returning the resolved PCIe-bus base.
    ///
    /// Firmware normally programs a function's BARs, but when the OS
    /// resets and re-enumerates the host bridge (the BCM2711 PCIe
    /// bring-up) a downstream function's BAR address bits read zero: the
    /// BAR is sized and typed but carries no base, so mapping it would
    /// target physical address 0 and be refused. Assigning resources
    /// from the bridge's outbound window is the PCI core's job. A BAR
    /// that already carries a non-zero base is left untouched and its
    /// base returned (firmware's assignment is respected); the call is
    /// then a no-op that leaves configuration space unchanged. A
    /// DMA-driving driver calls this once before
    /// [`map_bar_window`](Self::map_bar_window).
    ///
    /// The returned base is a **PCIe-bus** address; the host bridge's
    /// [`MmioMapper`] translates it to CPU-physical at map time.
    ///
    /// # Errors
    ///
    /// * [`DriverError::NotFound`] — `bar_index` is out of range, or no
    ///   memory BAR is implemented at that slot.
    /// * [`DriverError::Unsupported`] — the BAR is an I/O-port BAR, or
    ///   the function is not a type-0 header.
    /// * [`DriverError::OutOfRange`] — the BAR's size-aligned placement
    ///   does not fit inside the window, or a 32-bit BAR would land
    ///   above the 4 GiB line (fail closed).
    fn assign_bar(
        &self,
        bdf: u64,
        bar_index: u8,
        window_base: u64,
        window_size: u64,
    ) -> Result<u64, DriverError>;

    /// Read the configuration-space dword at byte `offset` of function
    /// `bdf`.
    ///
    /// `offset` is a **byte** offset into the function's 256-byte
    /// configuration header and is taken modulo-4 (the dword the byte
    /// falls in); the returned value is the little-endian dword exactly
    /// as configuration space holds it. This is a read-only window onto
    /// a function's own configuration the bus driver already reaches by
    /// holding its [`DriverHandle`](crate::driver::DriverHandle), used to
    /// confirm a write took effect (a just-assigned BAR, an enabled
    /// command register, a programmed bridge window) — a diagnostic
    /// read, not a side-effecting one.
    ///
    /// # Errors
    ///
    /// * [`DriverError::DeviceFault`] if the configuration read cannot
    ///   be completed by the bus transport.
    fn read_config(&self, bdf: u64, offset: u16) -> Result<u32, DriverError>;

    /// Describe the function at `bdf` as a discovered child
    /// [`HwNode`] to attach beneath the bus's own
    /// hardware-tree node.
    ///
    /// A bus that enumerates downstream devices is responsible for
    /// growing the hardware tree at runtime: each device it finds
    /// becomes a child node carrying the match keys a driver's signed
    /// bind table is resolved against, so a device
    /// behind the bus autoloads its driver as match **data** rather than
    /// by hand-wired composition. For a PCI
    /// function the emitted node carries a single
    /// [`HwMatchKey::pci`](crate::HwMatchKey::pci) of the function's
    /// `vendor:device` and its **full 24-bit class code**
    /// `(base_class << 16) | (sub_class << 8) | prog_if` — the prog-if
    /// is part of the class so an xHCI host (`0x0C_03_30`) is
    /// distinguished from the older USB host classes, exactly as the
    /// generic xHCI driver's bind key requires.
    ///
    /// The returned node carries **no** identity: its id and parent are
    /// unassigned placeholders ([`HwNode::set_identity`] is the kernel's
    /// to call). A bus driver does not name the child's id or its own
    /// node id — when the node is published through the `hw_emit_node`
    /// syscall the kernel assigns a fresh, collision-free id and sets the
    /// parent to the emitting driver's own matched node, so a driver can
    /// neither forge its tree position nor collide with an existing id
    /// (identity is kernel-provided, never
    /// caller-supplied;). No resource capabilities are
    /// attached here either; those are minted at the load gate.
    ///
    /// # Errors
    ///
    /// * [`DriverError::NotFound`] if no function responds at `bdf` (an
    ///   absent function reads the all-ones vendor sentinel) — a
    ///   fail-closed refusal, never a fabricated node.
    /// * [`DriverError::DeviceFault`] if the configuration read cannot be
    ///   completed by the bus transport, or the node cannot be assembled.
    fn describe_function(&self, bdf: u64) -> Result<HwNode, DriverError>;

    /// Program function `bdf`'s legacy **MSI** capability with `message`,
    /// force a single vector, and enable it.
    ///
    /// The non-virtio counterpart of
    /// [`MsixBus::route_msix`](super::msix::MsixBus::route_msix) for a
    /// function that advertises the legacy MSI capability rather than MSI-X
    /// (the Pi 4's VL805 xHCI host): the kernel's interrupt controller mints
    /// the opaque [`MsiMessage`] (doorbell address + data), and this writes
    /// it into the capability's Message-Address/Message-Data registers, then
    /// sets MSI Enable with Multiple-Message-Enable cleared (exactly one
    /// vector). Unlike `route_msix` it needs no [`MmioMapper`] — the MSI
    /// capability lives entirely in configuration space.
    ///
    /// The default implementation returns [`DriverError::Unsupported`], the
    /// correct shape for a bus seam that does not provision MSI (a test
    /// double, or a transport with no configuration-space MSI capability).
    ///
    /// # Errors
    ///
    /// * [`DriverError::Unsupported`] if the seam provisions no MSI (the
    ///   default).
    /// * [`DriverError::NotFound`] if the function advertises no MSI
    ///   capability.
    /// * [`DriverError::OutOfRange`] if `message.address` needs 64-bit
    ///   addressing but the capability is 32-bit only (fail closed).
    fn route_msi(&self, bdf: u64, message: MsiMessage) -> Result<(), DriverError> {
        let _ = (bdf, message);
        Err(DriverError::Unsupported)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::driver::bus::BusDevice;
    use crate::driver::mmio::MmioMapError;
    use crate::{HwDeviceClass, HwMatchKey};
    use core::cell::Cell;
    use core::ptr::NonNull;

    /// 4-byte-aligned backing so a window base satisfies
    /// `RegisterWindow::from_mapping`'s alignment contract.
    static mut BACKING: [u32; 16] = [0u32; 16];

    struct FakeMapper {
        grant: bool,
        last: Cell<Option<(u64, usize)>>,
    }

    impl MmioMapper for FakeMapper {
        fn map_window(&self, phys_base: u64, len: usize) -> Result<RegisterWindow, MmioMapError> {
            if !self.grant {
                return Err(MmioMapError::CapabilityMissing);
            }
            self.last.set(Some((phys_base, len)));
            let base = NonNull::new(core::ptr::addr_of_mut!(BACKING).cast::<u8>())
                .expect("static is non-null");
            // SAFETY: single-threaded test; the static outlives the
            // window and the window only touches `len <= 64` bytes.
            Ok(unsafe { RegisterWindow::from_mapping(phys_base, base, len.min(64)) })
        }
    }

    struct FakeBus {
        bar_base: u64,
        bar_size: u64,
        master_enabled: Cell<bool>,
    }

    impl Bus for FakeBus {
        fn enumerate(&self, out: &mut [BusDevice]) -> Result<usize, DriverError> {
            if out.is_empty() {
                return Err(DriverError::BufferTooSmall);
            }
            out[0] = BusDevice {
                vendor: 0x1106,
                device: 0x3483,
                class: 0x0C03,
                reserved0: 0,
                address: 0x0001_0000,
            };
            Ok(1)
        }
    }

    impl PciBus for FakeBus {
        fn map_bar_window(
            &self,
            _bdf: u64,
            bar_index: u8,
            mapper: &dyn MmioMapper,
        ) -> Result<RegisterWindow, DriverError> {
            if bar_index != 0 {
                return Err(DriverError::NotFound);
            }
            if self.bar_size == 0 {
                return Err(DriverError::NotFound);
            }
            let len = usize::try_from(self.bar_size).map_err(|_| DriverError::LengthOutOfRange)?;
            mapper
                .map_window(self.bar_base, len)
                .map_err(MmioMapError::as_driver_error)
        }

        fn enable_bus_master(&self, _bdf: u64) -> Result<(), DriverError> {
            self.master_enabled.set(true);
            Ok(())
        }

        fn assign_bar(
            &self,
            _bdf: u64,
            bar_index: u8,
            window_base: u64,
            window_size: u64,
        ) -> Result<u64, DriverError> {
            if bar_index != 0 || self.bar_size == 0 {
                return Err(DriverError::NotFound);
            }
            // Already-based BAR: respected unchanged.
            if self.bar_base != 0 {
                return Ok(self.bar_base);
            }
            if self.bar_size > window_size {
                return Err(DriverError::OutOfRange);
            }
            Ok(window_base)
        }

        fn read_config(&self, _bdf: u64, offset: u16) -> Result<u32, DriverError> {
            // BAR0 at byte offset 0x10 reads back the assigned base;
            // every other offset reads zero (enough for the trait test).
            match offset & !0x3 {
                0x10 => Ok((self.bar_base & 0xFFFF_FFFF) as u32),
                _ => Ok(0),
            }
        }

        fn describe_function(&self, _bdf: u64) -> Result<HwNode, DriverError> {
            // Identity is unassigned: the kernel sets it on publish. Build
            // the node with placeholder id/parent the publish path
            // overwrites.
            let mut node = HwNode::new(0, crate::hwtree::HW_NODE_ROOT, HwDeviceClass::Bus);
            node.push_match_key(HwMatchKey::pci(0x1106, 0x3483, 0x0C_03_30))
                .map_err(|_| DriverError::DeviceFault)?;
            Ok(node)
        }
    }

    fn bus() -> FakeBus {
        FakeBus {
            bar_base: 0x6000_0000,
            bar_size: 0x40,
            master_enabled: Cell::new(false),
        }
    }

    #[test]
    fn trait_object_maps_the_bar_and_enables_mastering() {
        let bus = bus();
        let dyn_bus: &dyn PciBus = &bus;
        let mapper = FakeMapper {
            grant: true,
            last: Cell::new(None),
        };
        dyn_bus
            .enable_bus_master(0x0001_0000)
            .expect("bus master enable");
        let window = dyn_bus
            .map_bar_window(0x0001_0000, 0, &mapper)
            .expect("bar window");
        assert_eq!(window.len(), 0x40);
        assert_eq!(mapper.last.get(), Some((0x6000_0000, 0x40)));
        assert!(bus.master_enabled.get());
    }

    #[test]
    fn read_config_returns_the_dword_at_the_byte_offset() {
        let bus = bus();
        let dyn_bus: &dyn PciBus = &bus;
        // BAR0 byte offset 0x10 reads back the (low 32 bits of the)
        // assigned base; the byte offset is taken to its dword.
        assert_eq!(dyn_bus.read_config(0x0001_0000, 0x10), Ok(0x6000_0000));
        assert_eq!(dyn_bus.read_config(0x0001_0000, 0x12), Ok(0x6000_0000));
        assert_eq!(dyn_bus.read_config(0x0001_0000, 0x04), Ok(0));
    }

    #[test]
    fn missing_bar_is_not_found() {
        let bus = bus();
        let dyn_bus: &dyn PciBus = &bus;
        let mapper = FakeMapper {
            grant: true,
            last: Cell::new(None),
        };
        assert!(matches!(
            dyn_bus.map_bar_window(0x0001_0000, 2, &mapper),
            Err(DriverError::NotFound)
        ));
    }

    #[test]
    fn missing_capability_propagates_as_permission_denied() {
        let bus = bus();
        let dyn_bus: &dyn PciBus = &bus;
        let mapper = FakeMapper {
            grant: false,
            last: Cell::new(None),
        };
        assert!(matches!(
            dyn_bus.map_bar_window(0x0001_0000, 0, &mapper),
            Err(DriverError::PermissionDenied)
        ));
    }

    #[test]
    fn describe_function_emits_a_child_node_with_the_pci_match_key() {
        let bus = bus();
        let dyn_bus: &dyn PciBus = &bus;
        let node = dyn_bus
            .describe_function(0x0001_0000)
            .expect("describes the function");
        // The lone key is the function's vendor:device:24-bit class, so a
        // generic xHCI bind key (class `0x0C_03_30`, vendor/device
        // wildcard) resolves against it.
        assert_eq!(node.match_keys().len(), 1);
        let bind = HwMatchKey::pci(0, 0, 0x0C_03_30);
        assert!(bind.matches(&node.match_keys()[0]));
        // A bind key naming a different class does not.
        assert!(!HwMatchKey::pci(0, 0, 0x0C_03_20).matches(&node.match_keys()[0]));
    }
}
