//! Unit tests for the linear framebuffer driver against an in-process
//! mock [`MmioMapper`] (mirrors the bus drivers' `MockMapper`).

extern crate alloc;

use alloc::rc::Rc;
use alloc::vec;
use alloc::vec::Vec;
use core::cell::RefCell;
use core::ptr::NonNull;

use super::*;
use rustos_abi::driver::DriverKind;
use rustos_abi::{CapabilityId, MmioMapError, MmioMapper, RegisterWindow};

/// Stand-in for the kernel's MMIO-map facility. Backs every minted
/// [`RegisterWindow`] with a fixed `u32` buffer (≥ 4-byte aligned,
/// matching the page-aligned mapping the real mapper produces) shared
/// with the test so it can read scan-out bytes back. `granted` models
/// the mapper-side `CAP_MMIO_MAP` check.
struct MockMapper {
    backing: Rc<RefCell<Vec<u32>>>,
    granted: bool,
}

impl MockMapper {
    fn new(words: usize, granted: bool) -> Self {
        Self {
            backing: Rc::new(RefCell::new(vec![0u32; words])),
            granted,
        }
    }

    /// Read a single scan-out byte back from the shared backing. The
    /// host runs little-endian, so byte `off` of word `off / 4` sits
    /// in lane `off % 4`.
    fn byte(&self, off: usize) -> u8 {
        self.backing.borrow()[off / 4].to_le_bytes()[off % 4]
    }
}

impl MmioMapper for MockMapper {
    fn map_window(&self, phys_base: u64, len: usize) -> Result<RegisterWindow, MmioMapError> {
        if !self.granted {
            return Err(MmioMapError::CapabilityMissing);
        }
        if len == 0 {
            return Err(MmioMapError::InvalidRegion);
        }
        let mut backing = self.backing.borrow_mut();
        if len > backing.len() * 4 {
            return Err(MmioMapError::Unsupported);
        }
        let base = NonNull::new(backing.as_mut_ptr().cast::<u8>()).expect("non-null heap buffer");
        // SAFETY: `base` covers `backing.len() * 4 >= len` bytes and is
        // 4-byte aligned (the `Vec<u32>` allocation guarantee); the
        // backing is held in an `Rc` that outlives every window minted
        // here, and the tests touch it through `byte()` only between —
        // never during — the driver's writes.
        Ok(unsafe { RegisterWindow::from_mapping(phys_base, base, len) })
    }
}

/// Mock driver host. `drv_load` / `mmio_map` model the load-time
/// capability grants; `mapper` is `None` to model a host with no
/// MMIO-map facility.
struct MockHost {
    drv_load: bool,
    mmio_map: bool,
    mapper: Option<MockMapper>,
}

impl MockHost {
    fn full(words: usize) -> Self {
        Self {
            drv_load: true,
            mmio_map: true,
            mapper: Some(MockMapper::new(words, true)),
        }
    }
}

impl DriverHost for MockHost {
    fn has_capability(&self, cap: CapabilityId) -> bool {
        match cap {
            CapabilityId::DRV_LOAD => self.drv_load,
            CapabilityId::MMIO_MAP => self.mmio_map,
            _ => false,
        }
    }
    fn kind(&self) -> DriverKind {
        DriverKind::UserSpace
    }
    fn mmio_mapper(&self) -> Option<&dyn MmioMapper> {
        self.mapper.as_ref().map(|m| m as &dyn MmioMapper)
    }
}

fn config(width: u32, height: u32, stride: u32) -> FramebufferConfig {
    FramebufferConfig {
        phys_base: 0xFD00_0000,
        width_px: width,
        height_px: height,
        stride_bytes: stride,
        format: DisplayFormat::Bgra8888,
    }
}

#[test]
fn register_requires_drv_load() {
    let granted = MockHost {
        drv_load: true,
        mmio_map: false,
        mapper: None,
    };
    assert!(register(&granted).is_ok());
    let denied = MockHost {
        drv_load: false,
        mmio_map: false,
        mapper: None,
    };
    assert_eq!(register(&denied), Err(DriverError::PermissionDenied));
}

#[test]
fn open_reports_configured_mode() {
    // 4x2 BGRA, stride 16 → 32-byte surface, 8 backing words.
    let host = MockHost::full(8);
    let fb = Framebuffer::open(&host, config(4, 2, 16)).expect("open");
    assert_eq!(
        fb.mode_info().expect("mode"),
        DisplayMode {
            width_px: 4,
            height_px: 2,
            stride_bytes: 16,
            format: DisplayFormat::Bgra8888,
        }
    );
}

#[test]
fn present_copies_frame_bytes_into_surface() {
    let host = MockHost::full(8);
    let mut fb = Framebuffer::open(&host, config(4, 2, 16)).expect("open");
    let frame: Vec<u8> = (0..32u8).collect();
    fb.present(&frame).expect("present");
    let mapper = host.mapper.as_ref().expect("mapper");
    for (off, expected) in frame.iter().enumerate() {
        assert_eq!(mapper.byte(off), *expected, "byte {off}");
    }
}

#[test]
fn present_handles_non_word_multiple_surface() {
    // stride 13 (one byte of scanline padding past 3*4) × 2 rows = 26
    // bytes: exercises the `u32` bulk path plus the 2-byte tail.
    let host = MockHost::full(8); // 32 bytes backing ≥ 26.
    let mut fb = Framebuffer::open(&host, config(3, 2, 13)).expect("open");
    let frame: Vec<u8> = (0..26u8).map(|b| b.wrapping_mul(7)).collect();
    fb.present(&frame).expect("present");
    let mapper = host.mapper.as_ref().expect("mapper");
    for (off, expected) in frame.iter().enumerate() {
        assert_eq!(mapper.byte(off), *expected, "byte {off}");
    }
}

#[test]
fn present_rejects_short_frame() {
    let host = MockHost::full(8);
    let mut fb = Framebuffer::open(&host, config(4, 2, 16)).expect("open");
    let short = vec![0u8; 31];
    assert_eq!(fb.present(&short), Err(DriverError::BufferTooSmall));
}

#[test]
fn present_ignores_trailing_bytes_of_oversized_frame() {
    // 9 backing words (36 bytes) so the post-surface probe at byte 32
    // stays in bounds.
    let host = MockHost::full(9);
    let mut fb = Framebuffer::open(&host, config(4, 2, 16)).expect("open");
    let mut frame = vec![0xABu8; 32];
    frame.extend_from_slice(&[0xFFu8; 16]); // 48 bytes; last 16 ignored.
    fb.present(&frame).expect("present");
    let mapper = host.mapper.as_ref().expect("mapper");
    for off in 0..32 {
        assert_eq!(mapper.byte(off), 0xAB, "byte {off}");
    }
    // Bytes past the surface stay zero in the backing.
    assert_eq!(mapper.byte(32), 0x00);
}

#[test]
fn open_requires_mmio_map_capability_on_host() {
    let host = MockHost {
        drv_load: true,
        mmio_map: false,
        mapper: Some(MockMapper::new(8, true)),
    };
    assert_eq!(
        Framebuffer::open(&host, config(4, 2, 16)).err(),
        Some(DriverError::PermissionDenied)
    );
}

#[test]
fn open_surfaces_mapper_capability_denial() {
    // Host advertises the grant but the mapper itself fails closed.
    let host = MockHost {
        drv_load: true,
        mmio_map: true,
        mapper: Some(MockMapper::new(8, false)),
    };
    assert_eq!(
        Framebuffer::open(&host, config(4, 2, 16)).err(),
        Some(DriverError::PermissionDenied)
    );
}

#[test]
fn open_without_mapper_is_unsupported() {
    let host = MockHost {
        drv_load: true,
        mmio_map: true,
        mapper: None,
    };
    assert_eq!(
        Framebuffer::open(&host, config(4, 2, 16)).err(),
        Some(DriverError::Unsupported)
    );
}

#[test]
fn open_rejects_region_larger_than_platform_can_map() {
    // 4x2 stride 16 = 32 bytes, but only 4 backing words (16 bytes).
    let host = MockHost::full(4);
    assert_eq!(
        Framebuffer::open(&host, config(4, 2, 16)).err(),
        Some(DriverError::Unsupported)
    );
}

#[test]
fn open_rejects_degenerate_geometry() {
    let host = MockHost::full(8);
    assert_eq!(
        Framebuffer::open(&host, config(0, 2, 16)).err(),
        Some(DriverError::LengthOutOfRange)
    );
    assert_eq!(
        Framebuffer::open(&host, config(4, 0, 16)).err(),
        Some(DriverError::LengthOutOfRange)
    );
    // Stride too small to hold one 4-pixel BGRA scanline (needs 16).
    assert_eq!(
        Framebuffer::open(&host, config(4, 2, 12)).err(),
        Some(DriverError::LengthOutOfRange)
    );
}

#[test]
fn unload_then_reload_presents_again() {
    let host = MockHost::full(8);
    {
        let mut fb = Framebuffer::open(&host, config(4, 2, 16)).expect("first load");
        fb.present(&[0x11u8; 32]).expect("present");
        // `fb` drops here — the unload step (the window is released).
    }
    let mut reloaded = Framebuffer::open(&host, config(4, 2, 16)).expect("reload");
    reloaded.present(&[0x22u8; 32]).expect("present");
    let mapper = host.mapper.as_ref().expect("mapper");
    for off in 0..32 {
        assert_eq!(mapper.byte(off), 0x22, "byte {off}");
    }
}
