//! Shared host-test fake for the virtio-MMIO discovery observers.
//!
//! [`FakeBus`] enumerates a fixed slot table, standing in for the live
//! `drivers/bus/mmio` reader so the hardware-discovery walks
//! ([`crate::hwdiscovery`]) and the root-block resolution they feed
//! ([`crate::root_storage`]) are host-testable without real MMIO. Both
//! modules' unit tests drive the same fake rather than each carrying its
//! own copy, so the enumeration behaviour they assert against never
//! drifts.

use rustos_abi::driver::bus::{Bus, BusDevice};
use rustos_abi::driver::virtio_mmio::VirtioMmioBus;
use rustos_abi::DriverError;

/// Window extent the fake reports for every populated slot — the `virt`
/// board's per-slot virtio-MMIO register-block size, matching the live
/// `drivers/bus/mmio` reader.
pub(crate) const FAKE_SLOT_WINDOW: u64 = 0x200;

/// A fake virtio-MMIO bus enumerating a fixed slot table (same shape as
/// the `kernel/virtio` walk's fake).
pub(crate) struct FakeBus {
    slots: alloc::vec::Vec<BusDevice>,
}

impl FakeBus {
    /// Build a bus whose populated slots report the given virtio device
    /// ids, each at a distinct, plausible per-slot register base.
    pub(crate) fn with(devices: &[u32]) -> Self {
        let slots = devices
            .iter()
            .enumerate()
            .map(|(i, &device)| BusDevice {
                vendor: 0x554D_4551,
                device,
                class: 2,
                reserved0: 0,
                // A distinct, plausible per-slot base; unused by the
                // block gate (the probed child carries only its bind key).
                address: 0x0A00_0000 + (i as u64) * 0x200,
            })
            .collect();
        Self { slots }
    }
}

impl Bus for FakeBus {
    fn enumerate(&self, out: &mut [BusDevice]) -> Result<usize, DriverError> {
        if out.len() < self.slots.len() {
            return Err(DriverError::BufferTooSmall);
        }
        out[..self.slots.len()].copy_from_slice(&self.slots);
        Ok(self.slots.len())
    }
}

impl VirtioMmioBus for FakeBus {
    fn map_slot_window(
        &self,
        _base: u64,
        _mapper: &dyn rustos_abi::MmioMapper,
    ) -> Result<rustos_abi::RegisterWindow, DriverError> {
        // The interrupt-driven discovery walk never maps a window (it only
        // reads the slot extent through `slot_window`), so the host tests
        // never reach this method; it exists only to satisfy the trait.
        Err(DriverError::NotFound)
    }

    fn slot_window(&self, base: u64) -> Result<u64, DriverError> {
        if self.slots.iter().any(|s| s.address == base) {
            Ok(FAKE_SLOT_WINDOW)
        } else {
            Err(DriverError::NotFound)
        }
    }
}
