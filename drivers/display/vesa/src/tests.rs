//! Unit tests for the VESA driver: the VBE `ModeInfoBlock` decoder and
//! the linear-framebuffer surface against an in-process mock
//! [`MmioMapper`] (mirrors the framebuffer and bus drivers' `MockMapper`).

extern crate alloc;

use alloc::rc::Rc;
use alloc::vec;
use alloc::vec::Vec;
use core::cell::RefCell;
use core::ptr::NonNull;

use super::*;
use core::cell::Cell;
use tairix_abi::driver::DriverKind;
use tairix_abi::{CapabilityId, MmioMapError, MmioMapper, RegisterWindow};

/// Build a well-formed VBE `ModeInfoBlock` describing a 32-bpp
/// direct-colour linear-framebuffer mode with the `Bgra8888` layout
/// (red at bit 16, green at bit 8, blue at bit 0). Tests mutate single
/// fields to exercise each rejection path.
fn bgra_block(width: u16, height: u16, stride: u16, phys_base: u32) -> Vec<u8> {
    let mut b = vec![0u8; VBE_MODE_INFO_BLOCK_LEN];
    // ModeAttributes: supported (bit 0) + linear framebuffer (bit 7).
    let attrs: u16 = MODE_ATTR_SUPPORTED | MODE_ATTR_LINEAR_FRAMEBUFFER;
    b[field::MODE_ATTRIBUTES..field::MODE_ATTRIBUTES + 2].copy_from_slice(&attrs.to_le_bytes());
    b[field::BYTES_PER_SCAN_LINE..field::BYTES_PER_SCAN_LINE + 2]
        .copy_from_slice(&stride.to_le_bytes());
    b[field::X_RESOLUTION..field::X_RESOLUTION + 2].copy_from_slice(&width.to_le_bytes());
    b[field::Y_RESOLUTION..field::Y_RESOLUTION + 2].copy_from_slice(&height.to_le_bytes());
    b[field::BITS_PER_PIXEL] = SUPPORTED_BITS_PER_PIXEL;
    b[field::MEMORY_MODEL] = MEMORY_MODEL_DIRECT_COLOUR;
    b[field::RED_MASK_SIZE] = SUPPORTED_CHANNEL_MASK_BITS;
    b[field::GREEN_MASK_SIZE] = SUPPORTED_CHANNEL_MASK_BITS;
    b[field::BLUE_MASK_SIZE] = SUPPORTED_CHANNEL_MASK_BITS;
    b[field::RED_FIELD_POSITION] = 16;
    b[field::GREEN_FIELD_POSITION] = 8;
    b[field::BLUE_FIELD_POSITION] = 0;
    b[field::PHYS_BASE_PTR..field::PHYS_BASE_PTR + 4].copy_from_slice(&phys_base.to_le_bytes());
    b
}

/// Stand-in for the kernel's MMIO-map facility, backing every minted
/// [`RegisterWindow`] with a fixed `u32` buffer (≥ 4-byte aligned) shared
/// with the test so it can read scan-out bytes back. `granted` models the
/// mapper-side `CAP_MMIO_MAP` check.
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

/// Settable stand-in for the kernel's live seat-lease check: the test
/// flips the verdict between frames to model an administrative revoke
/// (and a fresh grant) without a kernel registry.
struct MockGate {
    verdict: Cell<Result<(), DriverError>>,
}

impl SeatGate for MockGate {
    fn check_present(&self) -> Result<(), DriverError> {
        self.verdict.get()
    }
}

/// Mock driver host. `drv_load` / `mmio_map` model the load-time
/// capability grants; `mapper` is `None` to model a host with no
/// MMIO-map facility; `gate` is `None` to model a host with no seat
/// wired (present then proceeds ungated).
struct MockHost {
    drv_load: bool,
    mmio_map: bool,
    mapper: Option<MockMapper>,
    gate: Option<MockGate>,
}

impl MockHost {
    fn full(words: usize) -> Self {
        Self {
            drv_load: true,
            mmio_map: true,
            mapper: Some(MockMapper::new(words, true)),
            gate: None,
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
    fn seat_gate(&self) -> Option<&dyn SeatGate> {
        self.gate.as_ref().map(|g| g as &dyn SeatGate)
    }
}

#[test]
fn register_requires_drv_load() {
    let granted = MockHost {
        drv_load: true,
        mmio_map: false,
        mapper: None,
        gate: None,
    };
    assert!(register(&granted).is_ok());
    let denied = MockHost {
        drv_load: false,
        mmio_map: false,
        mapper: None,
        gate: None,
    };
    assert_eq!(register(&denied), Err(DriverError::PermissionDenied));
}

#[test]
fn parse_decodes_bgra_geometry() {
    let block = bgra_block(4, 2, 16, 0xFD00_0000);
    let info = VbeModeInfo::parse(&block).expect("valid block");
    assert_eq!(
        info,
        VbeModeInfo {
            phys_base: 0xFD00_0000,
            width_px: 4,
            height_px: 2,
            stride_bytes: 16,
            format: DisplayFormat::Bgra8888,
        }
    );
}

#[test]
fn parse_decodes_rgba_layout() {
    let mut block = bgra_block(4, 2, 16, 0xFD00_0000);
    // Swap red/blue field positions: red at byte 0 → Rgba8888.
    block[field::RED_FIELD_POSITION] = 0;
    block[field::BLUE_FIELD_POSITION] = 16;
    let info = VbeModeInfo::parse(&block).expect("valid block");
    assert_eq!(info.format, DisplayFormat::Rgba8888);
}

#[test]
fn parse_rejects_short_block() {
    let short = vec![0u8; VBE_MODE_INFO_BLOCK_LEN - 1];
    assert_eq!(VbeModeInfo::parse(&short), Err(DriverError::BufferTooSmall));
}

#[test]
fn parse_rejects_unsupported_mode_attribute() {
    let mut block = bgra_block(4, 2, 16, 0xFD00_0000);
    // Clear the "mode supported" bit (bit 0); keep the LFB bit.
    let attrs = MODE_ATTR_LINEAR_FRAMEBUFFER;
    block[field::MODE_ATTRIBUTES..field::MODE_ATTRIBUTES + 2].copy_from_slice(&attrs.to_le_bytes());
    assert_eq!(VbeModeInfo::parse(&block), Err(DriverError::Unsupported));
}

#[test]
fn parse_rejects_non_linear_framebuffer() {
    let mut block = bgra_block(4, 2, 16, 0xFD00_0000);
    // Keep supported, drop the linear-framebuffer bit.
    let attrs = MODE_ATTR_SUPPORTED;
    block[field::MODE_ATTRIBUTES..field::MODE_ATTRIBUTES + 2].copy_from_slice(&attrs.to_le_bytes());
    assert_eq!(VbeModeInfo::parse(&block), Err(DriverError::Unsupported));
}

#[test]
fn parse_rejects_non_direct_colour_model() {
    let mut block = bgra_block(4, 2, 16, 0xFD00_0000);
    block[field::MEMORY_MODEL] = 4; // packed-pixel, not direct colour.
    assert_eq!(VbeModeInfo::parse(&block), Err(DriverError::Unsupported));
}

#[test]
fn parse_rejects_non_32bpp() {
    let mut block = bgra_block(4, 2, 16, 0xFD00_0000);
    block[field::BITS_PER_PIXEL] = 24;
    assert_eq!(VbeModeInfo::parse(&block), Err(DriverError::Unsupported));
}

#[test]
fn parse_rejects_non_8bit_channel_masks() {
    let mut block = bgra_block(4, 2, 16, 0xFD00_0000);
    block[field::GREEN_MASK_SIZE] = 6; // 5-6-5-style mask.
    assert_eq!(VbeModeInfo::parse(&block), Err(DriverError::Unsupported));
}

#[test]
fn parse_rejects_unknown_channel_layout() {
    let mut block = bgra_block(4, 2, 16, 0xFD00_0000);
    // Green at byte 0 is neither Bgra8888 nor Rgba8888.
    block[field::RED_FIELD_POSITION] = 8;
    block[field::GREEN_FIELD_POSITION] = 0;
    block[field::BLUE_FIELD_POSITION] = 16;
    assert_eq!(VbeModeInfo::parse(&block), Err(DriverError::Unsupported));
}

#[test]
fn parse_rejects_zero_phys_base() {
    let block = bgra_block(4, 2, 16, 0);
    assert_eq!(VbeModeInfo::parse(&block), Err(DriverError::DeviceFault));
}

#[test]
fn parse_rejects_degenerate_geometry() {
    assert_eq!(
        VbeModeInfo::parse(&bgra_block(0, 2, 16, 0xFD00_0000)),
        Err(DriverError::LengthOutOfRange)
    );
    assert_eq!(
        VbeModeInfo::parse(&bgra_block(4, 0, 16, 0xFD00_0000)),
        Err(DriverError::LengthOutOfRange)
    );
    // Stride too small to hold one 4-pixel 32-bpp scanline (needs 16).
    assert_eq!(
        VbeModeInfo::parse(&bgra_block(4, 2, 12, 0xFD00_0000)),
        Err(DriverError::LengthOutOfRange)
    );
}

#[test]
fn open_reports_decoded_mode() {
    // 4x2 32-bpp, stride 16 → 32-byte surface, 8 backing words.
    let host = MockHost::full(8);
    let fb = VesaFramebuffer::open(&host, &bgra_block(4, 2, 16, 0xFD00_0000)).expect("open");
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
    let mut fb = VesaFramebuffer::open(&host, &bgra_block(4, 2, 16, 0xFD00_0000)).expect("open");
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
    // bytes: exercises the `u32` bulk path plus the 2-byte tail. The
    // stride still holds a 3-pixel 32-bpp scanline (needs 12).
    let host = MockHost::full(8); // 32 bytes backing ≥ 26.
    let mut fb = VesaFramebuffer::open(&host, &bgra_block(3, 2, 13, 0xFD00_0000)).expect("open");
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
    let mut fb = VesaFramebuffer::open(&host, &bgra_block(4, 2, 16, 0xFD00_0000)).expect("open");
    let short = vec![0u8; 31];
    assert_eq!(fb.present(&short), Err(DriverError::BufferTooSmall));
}

#[test]
fn present_ignores_trailing_bytes_of_oversized_frame() {
    // 9 backing words (36 bytes) so the post-surface probe at byte 32
    // stays in bounds.
    let host = MockHost::full(9);
    let mut fb = VesaFramebuffer::open(&host, &bgra_block(4, 2, 16, 0xFD00_0000)).expect("open");
    let mut frame = vec![0xABu8; 32];
    frame.extend_from_slice(&[0xFFu8; 16]); // 48 bytes; last 16 ignored.
    fb.present(&frame).expect("present");
    let mapper = host.mapper.as_ref().expect("mapper");
    for off in 0..32 {
        assert_eq!(mapper.byte(off), 0xAB, "byte {off}");
    }
    assert_eq!(mapper.byte(32), 0x00);
}

#[test]
fn open_requires_mmio_map_capability_on_host() {
    let host = MockHost {
        drv_load: true,
        mmio_map: false,
        mapper: Some(MockMapper::new(8, true)),
        gate: None,
    };
    assert_eq!(
        VesaFramebuffer::open(&host, &bgra_block(4, 2, 16, 0xFD00_0000)).err(),
        Some(DriverError::PermissionDenied)
    );
}

#[test]
fn open_surfaces_mapper_capability_denial() {
    let host = MockHost {
        drv_load: true,
        mmio_map: true,
        mapper: Some(MockMapper::new(8, false)),
        gate: None,
    };
    assert_eq!(
        VesaFramebuffer::open(&host, &bgra_block(4, 2, 16, 0xFD00_0000)).err(),
        Some(DriverError::PermissionDenied)
    );
}

#[test]
fn open_without_mapper_is_unsupported() {
    let host = MockHost {
        drv_load: true,
        mmio_map: true,
        mapper: None,
        gate: None,
    };
    assert_eq!(
        VesaFramebuffer::open(&host, &bgra_block(4, 2, 16, 0xFD00_0000)).err(),
        Some(DriverError::Unsupported)
    );
}

#[test]
fn open_rejects_region_larger_than_platform_can_map() {
    // 4x2 stride 16 = 32 bytes, but only 4 backing words (16 bytes).
    let host = MockHost::full(4);
    assert_eq!(
        VesaFramebuffer::open(&host, &bgra_block(4, 2, 16, 0xFD00_0000)).err(),
        Some(DriverError::Unsupported)
    );
}

#[test]
fn open_surfaces_parse_failure() {
    let host = MockHost::full(8);
    // Zero phys base fails the parse before any mapping is attempted.
    assert_eq!(
        VesaFramebuffer::open(&host, &bgra_block(4, 2, 16, 0)).err(),
        Some(DriverError::DeviceFault)
    );
}

#[test]
fn present_gates_on_the_live_seat_lease() {
    let mut host = MockHost::full(8);
    host.gate = Some(MockGate {
        verdict: Cell::new(Ok(())),
    });
    let block = bgra_block(4, 2, 16, 0xFD00_0000);
    let mut fb = VesaFramebuffer::open(&host, &block).expect("open");
    fb.present(&[0x11u8; 32]).expect("live lease presents");

    // The lease is revoked between frames: the very next present is
    // refused with the distinct error before any surface access, so the
    // scan-out keeps the last-presented frame even though the mapping
    // still exists.
    let gate = host.gate.as_ref().expect("gate installed");
    gate.verdict.set(Err(DriverError::SeatRevoked));
    assert_eq!(fb.present(&[0x22u8; 32]), Err(DriverError::SeatRevoked));
    let mapper = host.mapper.as_ref().expect("mapper installed");
    assert_eq!(mapper.byte(0), 0x11);
    assert_eq!(mapper.byte(31), 0x11);

    // Once the host's gate reports a live lease again (the new
    // foreground's grant), presents flow.
    gate.verdict.set(Ok(()));
    fb.present(&[0x22u8; 32])
        .expect("live lease presents again");
    assert_eq!(mapper.byte(0), 0x22);
}

#[test]
fn unload_then_reload_presents_again() {
    let host = MockHost::full(8);
    let block = bgra_block(4, 2, 16, 0xFD00_0000);
    {
        let mut fb = VesaFramebuffer::open(&host, &block).expect("first load");
        fb.present(&[0x11u8; 32]).expect("present");
        // `fb` drops here — the unload step (the window is released).
    }
    let mut reloaded = VesaFramebuffer::open(&host, &block).expect("reload");
    reloaded.present(&[0x22u8; 32]).expect("present");
    let mapper = host.mapper.as_ref().expect("mapper");
    for off in 0..32 {
        assert_eq!(mapper.byte(off), 0x22, "byte {off}");
    }
}
