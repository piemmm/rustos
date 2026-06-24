//! PCI and MMIO backend adapters for the virtio [`crate::Transport`]
//! trait.
//!
//! Each backend wraps a single capability-checked
//! [`RegisterWindow`] — the device's register block, mapped into the
//! driver's address space by the kernel's MMIO-map facility
//! (`kernel/sec::map_mmio`, reached through
//! [`MmioMapper`](rustos_abi::MmioMapper)). Before Stage 4.D Item 3
//! these shells carried a *bare identification tuple* (a PCI
//! bus/device/function triple, or an MMIO window length) and the
//! driver was expected to synthesise register accesses itself; that
//! violated ("no ambient authority — a process can
//! only reach memory the kernel mapped for it"). The window is now
//! the single thing a backend holds: a driver cannot fabricate one,
//! so it can only ever touch registers the kernel chose to expose.
//!
//! The PCI and MMIO transports decode *different* register layouts
//! (virtio 1.1 §4.1 modern-PCI common-config capability vs the
//! MMIO register map), which is why two distinct backend types exist
//! rather than one (each justifies its existence).
//! Both delegate the actual load/store to the bounds-checked
//! accessors on [`RegisterWindow`]; neither performs raw pointer
//! arithmetic.

use rustos_abi::{RegisterWindow, WindowError};

/// PCI-bus backend adapter.
///
/// Owns the [`RegisterWindow`] mapped over the virtio device's modern
/// PCI capability register block (the BAR the bus driver resolved and
/// handed to [`MmioMapper::map_window`](rustos_abi::MmioMapper::map_window)).
pub struct PciBackend {
    window: RegisterWindow,
}

impl PciBackend {
    /// Wrap a kernel-mapped PCI register window.
    ///
    /// The window is obtained by the PCI bus driver from the kernel's
    /// MMIO-map facility after it resolves the device's memory BAR;
    /// this constructor never synthesises a pointer.
    #[must_use]
    pub fn new(window: RegisterWindow) -> Self {
        Self { window }
    }

    /// Borrow the underlying register window.
    #[must_use]
    pub fn window(&self) -> &RegisterWindow {
        &self.window
    }

    /// Read a 32-bit device register at byte `offset` within the
    /// window.
    ///
    /// # Errors
    ///
    /// Propagates [`WindowError`] if `offset` is misaligned or the
    /// access overruns the window.
    pub fn read_u32(&self, offset: usize) -> Result<u32, WindowError> {
        self.window.read_u32(offset)
    }

    /// Write a 32-bit device register at byte `offset` within the
    /// window.
    ///
    /// # Errors
    ///
    /// Propagates [`WindowError`] if `offset` is misaligned or the
    /// access overruns the window.
    pub fn write_u32(&self, offset: usize, value: u32) -> Result<(), WindowError> {
        self.window.write_u32(offset, value)
    }
}

/// MMIO-bus backend adapter.
///
/// Owns the [`RegisterWindow`] mapped over the virtio-MMIO transport
/// slot the MMIO bus driver discovered in the device tree and handed
/// to [`MmioMapper::map_window`](rustos_abi::MmioMapper::map_window).
pub struct MmioBackend {
    window: RegisterWindow,
}

impl MmioBackend {
    /// Wrap a kernel-mapped MMIO register window.
    ///
    /// The window is obtained by the MMIO bus driver from the
    /// kernel's MMIO-map facility after it parses the transport
    /// slot's base + length from the device tree; this constructor
    /// never synthesises a pointer.
    #[must_use]
    pub fn new(window: RegisterWindow) -> Self {
        Self { window }
    }

    /// Borrow the underlying register window.
    #[must_use]
    pub fn window(&self) -> &RegisterWindow {
        &self.window
    }

    /// Length of the device's MMIO register window, in bytes.
    #[must_use]
    pub fn window_len(&self) -> usize {
        self.window.len()
    }

    /// Read a 32-bit device register at byte `offset` within the
    /// window.
    ///
    /// # Errors
    ///
    /// Propagates [`WindowError`] if `offset` is misaligned or the
    /// access overruns the window.
    pub fn read_u32(&self, offset: usize) -> Result<u32, WindowError> {
        self.window.read_u32(offset)
    }

    /// Write a 32-bit device register at byte `offset` within the
    /// window.
    ///
    /// # Errors
    ///
    /// Propagates [`WindowError`] if `offset` is misaligned or the
    /// access overruns the window.
    pub fn write_u32(&self, offset: usize, value: u32) -> Result<(), WindowError> {
        self.window.write_u32(offset, value)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use core::ptr::NonNull;

    /// 8-byte-aligned byte buffer so a window's base satisfies
    /// `RegisterWindow::from_mapping`'s ≥ 4-byte alignment contract.
    #[repr(align(8))]
    struct Aligned<const N: usize>([u8; N]);

    /// Build a [`RegisterWindow`] over a borrowed, aligned test
    /// buffer. The buffer outlives the window inside each test.
    fn window_over(buf: &mut [u8], phys: u64) -> RegisterWindow {
        let len = buf.len();
        let base = NonNull::new(buf.as_mut_ptr()).expect("non-null");
        // SAFETY: `base` covers exactly `len` bytes of the borrowed
        // buffer, which outlives the window, and the mutable borrow
        // guarantees unique access. `phys` is a synthetic device
        // address for the test.
        unsafe { RegisterWindow::from_mapping(phys, base, len) }
    }

    #[test]
    fn pci_backend_round_trips_through_window() {
        let mut buf = Aligned([0u8; 64]);
        let backend = PciBackend::new(window_over(&mut buf.0, 0xFEBD_0000));
        backend.write_u32(0x10, 0x1AF4_1000).expect("in bounds");
        assert_eq!(backend.read_u32(0x10).expect("in bounds"), 0x1AF4_1000);
        assert_eq!(backend.window().phys_base(), 0xFEBD_0000);
    }

    #[test]
    fn pci_backend_propagates_out_of_bounds() {
        let mut buf = Aligned([0u8; 8]);
        let backend = PciBackend::new(window_over(&mut buf.0, 0));
        assert_eq!(backend.read_u32(8), Err(WindowError::OutOfBounds));
    }

    #[test]
    fn mmio_backend_round_trips_through_window() {
        let mut buf = Aligned([0u8; 0x100]);
        let backend = MmioBackend::new(window_over(&mut buf.0, 0x1000_0000));
        assert_eq!(backend.window_len(), 0x100);
        backend.write_u32(0x70, 0xCAFE_BABE).expect("in bounds");
        assert_eq!(backend.read_u32(0x70).expect("in bounds"), 0xCAFE_BABE);
    }

    #[test]
    fn mmio_backend_propagates_misaligned() {
        let mut buf = Aligned([0u8; 0x100]);
        let backend = MmioBackend::new(window_over(&mut buf.0, 0));
        assert_eq!(backend.write_u32(0x71, 0), Err(WindowError::Misaligned));
    }
}
