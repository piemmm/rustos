//! virtio-1.x PCI transport-provisioning seam (`abi-v1`).
//!
//! A modern virtio PCI device publishes its register blocks through a
//! set of vendor-specific PCI capabilities (virtio 1.1 §4.1.4): a
//! *common configuration* structure, a *notification* area, an
//! *ISR-status* byte, and a *device-specific configuration* area. To
//! drive the device the kernel needs all four as kernel-mapped
//! [`RegisterWindow`]s plus the notification capability's
//! `notify_off_multiplier`.
//!
//! The boot-time PCI walk that turns those capabilities into windows
//! lives in ring 0, but ring 0 must stay driver-agnostic: it may not
//! name the concrete `lib/pci` types (a
//! driver crate's only public surface is `register`). This module is
//! the versioned ABI seam that breaks the tension: the PCI bus driver
//! implements [`VirtioPciBus`] and the kernel calls it through a
//! `&dyn VirtioPciBus`, exactly as it already reaches a bus through
//! [`Bus`] and the MMIO-map facility through [`MmioMapper`].
//!
//! Like every other item in `lib/abi`, the trait and its `cfg_type`
//! constants are frozen for the lifetime of `abi-v1`: new behaviour
//! ships in `abi-v2` rather than mutating this surface.

use super::bus::Bus;
use super::mmio::MmioMapError;
use super::{DriverError, MmioMapper, RegisterWindow};
use crate::hwtree::{HwResource, HwResourceKind};

/// PCI vendor ID assigned to virtio devices (virtio 1.1 §4.1.2).
///
/// A modern virtio PCI function reports this vendor ID; the device ID
/// is `0x1040 + virtio_device_type` (e.g. `0x1042` for block,
/// `0x1041` for network).
pub const VIRTIO_PCI_VENDOR_ID: u16 = 0x1AF4;

/// `cfg_type` for the common configuration structure (virtio 1.1
/// §4.1.4.3).
pub const VIRTIO_PCI_CFG_COMMON: u8 = 1;
/// `cfg_type` for the notification structure (virtio 1.1 §4.1.4.4).
pub const VIRTIO_PCI_CFG_NOTIFY: u8 = 2;
/// `cfg_type` for the ISR-status structure (virtio 1.1 §4.1.4.5).
pub const VIRTIO_PCI_CFG_ISR: u8 = 3;
/// `cfg_type` for the device-specific structure (virtio 1.1 §4.1.4.6).
pub const VIRTIO_PCI_CFG_DEVICE: u8 = 4;
/// `cfg_type` for the PCI configuration-access window (virtio 1.1
/// §4.1.4.7).
pub const VIRTIO_PCI_CFG_PCI: u8 = 5;

/// A PCI bus that can provision a modern virtio function's register
/// windows for a transport.
///
/// [`Bus`] is a supertrait so the kernel walk can enumerate the bus
/// (to pick the virtio function) and provision its windows through a
/// single `&dyn VirtioPciBus`, without depending on the concrete
/// `lib/pci` crate.
///
/// # Capabilities
///
/// The window-mapping method routes through the supplied
/// [`MmioMapper`], which enforces
/// [`CapabilityId::MMIO_MAP`](crate::CapabilityId::MMIO_MAP); the
/// implementation performs no mapping itself (no
/// ambient authority).
pub trait VirtioPciBus: Bus {
    /// Resolve the virtio configuration structure of kind `cfg_type`
    /// (one of the `VIRTIO_PCI_CFG_*` constants) on function `bdf` to its
    /// CPU-physical `(base, len)` window, **without mapping it**.
    ///
    /// This is the resolve primitive the two-process driver contract is
    /// built on: the kernel walks PCI configuration space (which a
    /// user-space driver cannot do), resolves each of the four windows a
    /// virtio transport consumes ([`VIRTIO_PCI_CFG_COMMON`],
    /// [`VIRTIO_PCI_CFG_NOTIFY`], [`VIRTIO_PCI_CFG_ISR`],
    /// [`VIRTIO_PCI_CFG_DEVICE`]), and grants the `(base, len)` to the
    /// autoloaded driver — which maps it in its own address space through
    /// its capability-gated MMIO facility. [`map_virtio_window`] is the
    /// in-kernel, single-process sibling that resolves *and* maps.
    ///
    /// [`map_virtio_window`]: Self::map_virtio_window
    ///
    /// # Errors
    ///
    /// * [`DriverError::NotFound`] — the function advertises no virtio
    ///   capability of `cfg_type`, or the underlying BAR is unused.
    /// * [`DriverError::Unsupported`] — the structure lives in an
    ///   I/O-port BAR, or the function is not a type-0 header.
    /// * [`DriverError::OutOfRange`] — the structure's
    ///   `bar_offset + length` exceeds the resolved BAR size.
    /// * [`DriverError::LengthOutOfRange`] — the region length does
    ///   not fit in `usize` on this target.
    fn virtio_window_region(&self, bdf: u64, cfg_type: u8) -> Result<(u64, usize), DriverError>;

    /// Resolve the virtio configuration structure of kind `cfg_type` on
    /// function `bdf` and ask `mapper` to map it, returning the resulting
    /// [`RegisterWindow`].
    ///
    /// The single-process, in-kernel provisioning path uses this: the
    /// kernel maps all four windows itself and builds the transport in
    /// ring 0. The two-process path instead resolves each window with
    /// [`virtio_window_region`](Self::virtio_window_region) and grants the
    /// `(base, len)` to a user-space driver, which maps it in its own
    /// address space — so this is defined once here in terms of the
    /// resolve primitive rather than duplicated per bus implementation.
    ///
    /// # Errors
    ///
    /// * [`DriverError::NotFound`] — the function advertises no virtio
    ///   capability of `cfg_type`, or the underlying BAR is unused.
    /// * [`DriverError::Unsupported`] — the structure lives in an
    ///   I/O-port BAR, or the function is not a type-0 header.
    /// * [`DriverError::OutOfRange`] — the structure's
    ///   `bar_offset + length` exceeds the resolved BAR size.
    /// * [`DriverError::LengthOutOfRange`] — the region length does not
    ///   fit in `usize` on this target.
    /// * [`DriverError::PermissionDenied`] — the caller does not hold
    ///   [`CapabilityId::MMIO_MAP`](crate::CapabilityId::MMIO_MAP)
    ///   (propagated from the mapper).
    fn map_virtio_window(
        &self,
        bdf: u64,
        cfg_type: u8,
        mapper: &dyn MmioMapper,
    ) -> Result<RegisterWindow, DriverError> {
        let (base, len) = self.virtio_window_region(bdf, cfg_type)?;
        mapper
            .map_window(base, len)
            .map_err(MmioMapError::as_driver_error)
    }

    /// Read the `notify_off_multiplier` from function `bdf`'s virtio
    /// notification capability (virtio 1.1 §4.1.4.4).
    ///
    /// # Errors
    ///
    /// * [`DriverError::NotFound`] — the function advertises no virtio
    ///   notification capability, or no capability list at all.
    /// * [`DriverError::BufferTooSmall`] / [`DriverError::DeviceFault`]
    ///   — propagated from the capability-list walk.
    fn notify_off_multiplier(&self, bdf: u64) -> Result<u32, DriverError>;
}

/// Build the role-tagged MMIO device-resource grant for one modern
/// virtio-PCI configuration window.
///
/// The kernel's PCI probe resolves each of a virtio function's four
/// configuration structures to a CPU-physical `(base, len)` window
/// (without mapping it) and emits it on the device's hardware-tree node
/// as one of these grants. `cfg_type` is the window's role (a
/// `VIRTIO_PCI_CFG_*` constant); it is stored in the window's tag so the
/// autoloaded driver process knows which window is which without reading
/// PCI configuration space (which a user-space driver cannot do). Only
/// the notify window carries `notify_off_multiplier`; it is ignored (and
/// stored as `0`) for every other role.
///
/// This is the emit-side companion of [`virtio_pci_windows`], which
/// resolves the grants back on the driver side; defining both here keeps
/// the encoding in one place.
#[must_use]
pub fn virtio_pci_window_resource(
    cfg_type: u8,
    base: u64,
    len: u64,
    notify_off_multiplier: u32,
) -> HwResource {
    let aux = if cfg_type == VIRTIO_PCI_CFG_NOTIFY {
        u64::from(notify_off_multiplier)
    } else {
        0
    };
    HwResource::mmio_tagged(base, len, u32::from(cfg_type), aux)
}

/// The four modern virtio-PCI configuration windows a driver process
/// received as role-tagged device-resource grants, resolved back into
/// the CPU-physical `(base, len)` pairs the driver maps plus the
/// notification multiplier the transport needs.
///
/// A driver builds this from the grant set the kernel minted for its
/// matched node with [`virtio_pci_windows`], then maps each window
/// through its host's [`MmioMapper`] and assembles the transport's
/// window descriptor — it never reads PCI configuration space itself.
#[derive(Copy, Clone, Debug, Eq, PartialEq)]
pub struct VirtioPciWindows {
    /// Common-configuration window `(cpu_base, len)`.
    pub common: (u64, usize),
    /// Notification-area window `(cpu_base, len)`.
    pub notify: (u64, usize),
    /// ISR-status window `(cpu_base, len)`.
    pub isr: (u64, usize),
    /// Device-specific-configuration window `(cpu_base, len)`.
    pub device: (u64, usize),
    /// `notify_off_multiplier` from the notification capability
    /// (virtio 1.1 §4.1.4.4).
    pub notify_off_multiplier: u32,
}

/// Resolve the four modern virtio-PCI configuration windows from a
/// driver's kernel-issued device-resource grants.
///
/// The kernel's PCI probe grants a virtio-PCI driver four role-tagged
/// MMIO windows (built with [`virtio_pci_window_resource`]) plus a DMA
/// constraint and an interrupt line. This inspects the grant set, matches
/// the four windows by their `cfg_type` tag, and returns their
/// CPU-physical `(base, len)` pairs and the notification multiplier — the
/// exact inputs a PCI virtio transport is constructed from. Grants of any
/// other kind (DMA, IRQ) are ignored.
///
/// It is the multi-window sibling of
/// [`sole_register_window`](crate::driver::sole_register_window): a driver
/// that may bind on either a single-aperture MMIO bus or a scattered PCI
/// bus tries this first and falls back to `sole_register_window` on
/// [`DriverError::NotFound`].
///
/// # Errors
///
/// Fails closed, never guessing a missing or ambiguous window:
///
/// * [`DriverError::NotFound`] if the grant set carries **no** role-tagged
///   virtio-PCI window — the signal that this is not a PCI delivery, so a
///   dual-bus driver falls back to the single-window MMIO path.
/// * [`DriverError::Unsupported`] if it carries *some* but not all four
///   distinct windows, or two windows sharing a role — a malformed
///   delivery a driver refuses rather than half-provisioning.
/// * [`DriverError::OutOfRange`] for a zero-length window or a length past
///   `usize` on the target.
///
/// # Capabilities
///
/// None. This inspects a grant set the kernel already minted; each window
/// is capability-checked kernel-side at the `mmio_map` trap.
pub fn virtio_pci_windows<'a, I>(resources: I) -> Result<VirtioPciWindows, DriverError>
where
    I: IntoIterator<Item = &'a HwResource>,
{
    let mut common: Option<(u64, usize)> = None;
    let mut notify: Option<(u64, usize)> = None;
    let mut isr: Option<(u64, usize)> = None;
    let mut device: Option<(u64, usize)> = None;
    let mut notify_off_multiplier: u32 = 0;
    let mut tagged = 0usize;

    for resource in resources {
        if resource.kind() != Some(HwResourceKind::Mmio) {
            continue;
        }
        // A plain (untagged) MMIO window is tag `0`, which is no virtio
        // role; only the four `VIRTIO_PCI_CFG_*` roles are windows here.
        let Ok(cfg_type) = u8::try_from(resource.flags()) else {
            continue;
        };
        let slot = match cfg_type {
            VIRTIO_PCI_CFG_COMMON => &mut common,
            VIRTIO_PCI_CFG_NOTIFY => &mut notify,
            VIRTIO_PCI_CFG_ISR => &mut isr,
            VIRTIO_PCI_CFG_DEVICE => &mut device,
            _ => continue,
        };
        let len = usize::try_from(resource.length()).map_err(|_| DriverError::OutOfRange)?;
        if len == 0 {
            return Err(DriverError::OutOfRange);
        }
        if slot.is_some() {
            // Two windows claiming the same role — an ambiguous delivery.
            return Err(DriverError::Unsupported);
        }
        *slot = Some((resource.base(), len));
        if cfg_type == VIRTIO_PCI_CFG_NOTIFY {
            notify_off_multiplier =
                u32::try_from(resource.translated_base()).map_err(|_| DriverError::OutOfRange)?;
        }
        tagged += 1;
    }

    if tagged == 0 {
        // No virtio-PCI window at all: a single-aperture MMIO delivery.
        return Err(DriverError::NotFound);
    }
    let (Some(common), Some(notify), Some(isr), Some(device)) = (common, notify, isr, device)
    else {
        // Some windows present but not the full set — a malformed grant.
        return Err(DriverError::Unsupported);
    };
    Ok(VirtioPciWindows {
        common,
        notify,
        isr,
        device,
        notify_off_multiplier,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::driver::bus::BusDevice;
    use crate::driver::mmio::MmioMapError;
    use core::ptr::NonNull;

    /// 4-byte-aligned (by element type) backing store so a window's
    /// base satisfies `RegisterWindow::from_mapping`'s ≥ 4-byte
    /// alignment contract; 16 × `u32` is 64 bytes.
    static mut BACKING: [u32; 16] = [0u32; 16];

    /// Mapper that hands out a window over the shared static backing
    /// store, recording the last `(phys, len)` it was asked for.
    struct FakeMapper {
        last: core::cell::Cell<Option<(u64, usize)>>,
        grant: bool,
    }

    impl MmioMapper for FakeMapper {
        fn map_window(&self, phys_base: u64, len: usize) -> Result<RegisterWindow, MmioMapError> {
            if !self.grant {
                return Err(MmioMapError::CapabilityMissing);
            }
            self.last.set(Some((phys_base, len)));
            // SAFETY: single-threaded test; the static lives for the
            // whole process and the returned window only performs
            // volatile accesses within `len <= 64` bytes.
            let base = NonNull::new(core::ptr::addr_of_mut!(BACKING).cast::<u8>())
                .expect("static is non-null");
            Ok(unsafe { RegisterWindow::from_mapping(phys_base, base, len.min(64)) })
        }
    }

    /// Minimal [`VirtioPciBus`] whose windows are fixed `(phys, len)`
    /// pairs per `cfg_type`, proving the kernel walk only needs the
    /// trait object.
    struct FakeBus;

    impl Bus for FakeBus {
        fn enumerate(&self, out: &mut [BusDevice]) -> Result<usize, DriverError> {
            if out.is_empty() {
                return Err(DriverError::BufferTooSmall);
            }
            out[0] = BusDevice {
                vendor: u32::from(VIRTIO_PCI_VENDOR_ID),
                device: 0x1042,
                class: 0x0100,
                reserved0: 0,
                address: 0x0000_0800,
            };
            Ok(1)
        }
    }

    impl VirtioPciBus for FakeBus {
        fn virtio_window_region(
            &self,
            _bdf: u64,
            cfg_type: u8,
        ) -> Result<(u64, usize), DriverError> {
            let len = match cfg_type {
                VIRTIO_PCI_CFG_COMMON => 0x38,
                VIRTIO_PCI_CFG_NOTIFY => 0x10,
                VIRTIO_PCI_CFG_ISR => 0x4,
                VIRTIO_PCI_CFG_DEVICE => 0x8,
                _ => return Err(DriverError::NotFound),
            };
            Ok((0xC000_0000 + u64::from(cfg_type), len))
        }

        fn notify_off_multiplier(&self, _bdf: u64) -> Result<u32, DriverError> {
            Ok(4)
        }
    }

    #[test]
    fn trait_object_provisions_each_cfg_type() {
        let bus: &dyn VirtioPciBus = &FakeBus;
        let mapper = FakeMapper {
            last: core::cell::Cell::new(None),
            grant: true,
        };
        let common = bus
            .map_virtio_window(0x0800, VIRTIO_PCI_CFG_COMMON, &mapper)
            .expect("common window");
        assert_eq!(common.len(), 0x38);
        assert_eq!(mapper.last.get(), Some((0xC000_0001, 0x38)));
        assert_eq!(bus.notify_off_multiplier(0x0800), Ok(4));
    }

    #[test]
    fn unknown_cfg_type_is_not_found() {
        let bus: &dyn VirtioPciBus = &FakeBus;
        let mapper = FakeMapper {
            last: core::cell::Cell::new(None),
            grant: true,
        };
        assert!(matches!(
            bus.map_virtio_window(0x0800, VIRTIO_PCI_CFG_PCI, &mapper),
            Err(DriverError::NotFound)
        ));
    }

    #[test]
    fn missing_capability_propagates_as_permission_denied() {
        let bus: &dyn VirtioPciBus = &FakeBus;
        let mapper = FakeMapper {
            last: core::cell::Cell::new(None),
            grant: false,
        };
        assert!(matches!(
            bus.map_virtio_window(0x0800, VIRTIO_PCI_CFG_COMMON, &mapper),
            Err(DriverError::PermissionDenied)
        ));
    }

    #[test]
    fn enumerate_surfaces_the_virtio_function() {
        let bus: &dyn VirtioPciBus = &FakeBus;
        let mut out = [BusDevice {
            vendor: 0,
            device: 0,
            class: 0,
            reserved0: 0,
            address: 0,
        }; 4];
        let n = bus.enumerate(&mut out).expect("enumerate");
        assert_eq!(n, 1);
        assert_eq!(out[0].vendor, u32::from(VIRTIO_PCI_VENDOR_ID));
        assert_eq!(out[0].device, 0x1042);
    }

    /// The full role-tagged grant set the kernel probe emits for one
    /// virtio-PCI device: four config windows, a DMA constraint, and an
    /// interrupt line (in a deliberately shuffled order the resolver must
    /// tolerate).
    fn pci_grant_set() -> [HwResource; 6] {
        [
            HwResource::irq(11, 1),
            virtio_pci_window_resource(VIRTIO_PCI_CFG_DEVICE, 0xC000_4000, 0x8, 0),
            virtio_pci_window_resource(VIRTIO_PCI_CFG_NOTIFY, 0xC000_1000, 0x10, 4),
            HwResource::dma(0, 0),
            virtio_pci_window_resource(VIRTIO_PCI_CFG_ISR, 0xC000_2000, 0x4, 0),
            virtio_pci_window_resource(VIRTIO_PCI_CFG_COMMON, 0xC000_0000, 0x38, 0),
        ]
    }

    #[test]
    fn window_resource_tags_role_and_carries_multiplier_only_on_notify() {
        let notify = virtio_pci_window_resource(VIRTIO_PCI_CFG_NOTIFY, 0xC000_1000, 0x10, 4);
        assert_eq!(notify.kind(), Some(HwResourceKind::Mmio));
        assert_eq!(notify.base(), 0xC000_1000);
        assert_eq!(notify.length(), 0x10);
        assert_eq!(notify.flags(), u32::from(VIRTIO_PCI_CFG_NOTIFY));
        assert_eq!(notify.translated_base(), 4);
        // A non-notify window drops the multiplier.
        let common = virtio_pci_window_resource(VIRTIO_PCI_CFG_COMMON, 0xC000_0000, 0x38, 4);
        assert_eq!(common.translated_base(), 0);
        // The mapping path still names the window by its CPU base.
        assert_eq!(common.register_window_base(), Some(0xC000_0000));
    }

    #[test]
    fn resolver_recovers_every_window_and_the_multiplier() {
        let grants = pci_grant_set();
        let windows = virtio_pci_windows(grants.iter()).expect("resolve");
        assert_eq!(
            windows,
            VirtioPciWindows {
                common: (0xC000_0000, 0x38),
                notify: (0xC000_1000, 0x10),
                isr: (0xC000_2000, 0x4),
                device: (0xC000_4000, 0x8),
                notify_off_multiplier: 4,
            }
        );
    }

    #[test]
    fn resolver_reports_no_pci_windows_as_not_found() {
        // A single-aperture MMIO delivery: the dual-bus driver falls back
        // to `sole_register_window` on this exact signal.
        let mmio = [HwResource::mmio(0x1000_0000, 0x1000), HwResource::irq(3, 1)];
        assert_eq!(virtio_pci_windows(mmio.iter()), Err(DriverError::NotFound));
    }

    #[test]
    fn resolver_refuses_a_partial_window_set() {
        // Common + notify present, ISR + device missing — a malformed
        // grant, refused rather than half-provisioned.
        let partial = [
            virtio_pci_window_resource(VIRTIO_PCI_CFG_COMMON, 0xC000_0000, 0x38, 0),
            virtio_pci_window_resource(VIRTIO_PCI_CFG_NOTIFY, 0xC000_1000, 0x10, 4),
        ];
        assert_eq!(
            virtio_pci_windows(partial.iter()),
            Err(DriverError::Unsupported)
        );
    }

    #[test]
    fn resolver_refuses_a_duplicated_role() {
        // Two windows both claiming the common-config role.
        let grants = [
            virtio_pci_window_resource(VIRTIO_PCI_CFG_COMMON, 0xC000_0000, 0x38, 0),
            virtio_pci_window_resource(VIRTIO_PCI_CFG_NOTIFY, 0xC000_1000, 0x10, 4),
            virtio_pci_window_resource(VIRTIO_PCI_CFG_ISR, 0xC000_2000, 0x4, 0),
            virtio_pci_window_resource(VIRTIO_PCI_CFG_DEVICE, 0xC000_4000, 0x8, 0),
            virtio_pci_window_resource(VIRTIO_PCI_CFG_COMMON, 0xC000_5000, 0x38, 0),
        ];
        assert_eq!(
            virtio_pci_windows(grants.iter()),
            Err(DriverError::Unsupported)
        );
    }

    #[test]
    fn resolver_refuses_a_zero_length_window() {
        let bad = [
            virtio_pci_window_resource(VIRTIO_PCI_CFG_COMMON, 0xC000_0000, 0, 0),
            virtio_pci_window_resource(VIRTIO_PCI_CFG_NOTIFY, 0xC000_1000, 0x10, 4),
            virtio_pci_window_resource(VIRTIO_PCI_CFG_ISR, 0xC000_2000, 0x4, 0),
            virtio_pci_window_resource(VIRTIO_PCI_CFG_DEVICE, 0xC000_4000, 0x8, 0),
        ];
        assert_eq!(virtio_pci_windows(bad.iter()), Err(DriverError::OutOfRange));
    }
}
