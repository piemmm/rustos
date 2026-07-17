//! Tests for the linear-framebuffer surface engine against an
//! in-process mock [`MmioMapper`] (mirrors the bus drivers' `MockMapper`).
//!
//! These live in the crate's `tests/` directory rather than beside the
//! code: minting a mock [`RegisterWindow`] requires its `unsafe`
//! constructor, and the library itself forbids `unsafe` outright — a
//! separate test crate keeps that guarantee intact.

use std::cell::{Cell, RefCell};
use std::ptr::NonNull;
use std::rc::Rc;

use tairix_abi::driver::display::{DamageRect, Display, DisplayFormat, DisplayMode, SeatGate};
use tairix_abi::driver::DriverKind;
use tairix_abi::{CapabilityId, DriverError, DriverHost, MmioMapError, MmioMapper, RegisterWindow};
use tairix_display::{Framebuffer, FramebufferConfig};

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
        // never during — the engine's writes.
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

/// Mock driver host. `mmio_map` models the load-time capability grant;
/// `mapper` is `None` to model a host with no MMIO-map facility; `gate`
/// is `None` to model a host with no seat wired (present then proceeds
/// ungated).
struct MockHost {
    mmio_map: bool,
    mapper: Option<MockMapper>,
    gate: Option<MockGate>,
}

impl MockHost {
    fn full(words: usize) -> Self {
        Self {
            mmio_map: true,
            mapper: Some(MockMapper::new(words, true)),
            gate: None,
        }
    }
}

impl DriverHost for MockHost {
    fn has_capability(&self, cap: CapabilityId) -> bool {
        match cap {
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
fn present_region_blits_only_the_damaged_span() {
    let host = MockHost::full(8);
    let mut fb = Framebuffer::open(&host, config(4, 2, 16)).expect("open");
    let frame = vec![0xEEu8; 32];
    let damage = DamageRect {
        x: 1,
        y: 1,
        width_px: 2,
        height_px: 1,
    };
    fb.present_region(&frame, damage).expect("region present");
    let mapper = host.mapper.as_ref().expect("mapper");
    // Row 1, pixels 1..3 (bytes 20..28) carry the frame; all else is 0.
    for off in 0..32 {
        let expected = if (20..28).contains(&off) { 0xEE } else { 0x00 };
        assert_eq!(mapper.byte(off), expected, "byte {off}");
    }
}

#[test]
fn present_region_fails_closed_on_bad_damage_and_short_frame() {
    let host = MockHost::full(8);
    let mut fb = Framebuffer::open(&host, config(4, 2, 16)).expect("open");
    let frame = vec![0xEEu8; 32];
    // Escaping and empty rectangles are refused before any write.
    for damage in [
        DamageRect {
            x: 3,
            y: 0,
            width_px: 2,
            height_px: 1,
        },
        DamageRect {
            x: 0,
            y: 0,
            width_px: 0,
            height_px: 1,
        },
    ] {
        assert_eq!(
            fb.present_region(&frame, damage),
            Err(DriverError::LengthOutOfRange)
        );
    }
    // A short frame is refused after the bounds pass.
    let full = DamageRect {
        x: 0,
        y: 0,
        width_px: 4,
        height_px: 2,
    };
    assert_eq!(
        fb.present_region(&frame[..16], full),
        Err(DriverError::BufferTooSmall)
    );
    let mapper = host.mapper.as_ref().expect("mapper");
    for off in 0..32 {
        assert_eq!(mapper.byte(off), 0x00, "surface untouched at byte {off}");
    }
}

#[test]
fn present_region_is_seat_gated_before_any_write() {
    let host = MockHost {
        mmio_map: true,
        mapper: Some(MockMapper::new(8, true)),
        gate: Some(MockGate {
            verdict: Cell::new(Err(DriverError::SeatRevoked)),
        }),
    };
    let mut fb = Framebuffer::open(&host, config(4, 2, 16)).expect("open");
    let frame = vec![0xEEu8; 32];
    let damage = DamageRect {
        x: 0,
        y: 0,
        width_px: 1,
        height_px: 1,
    };
    assert_eq!(
        fb.present_region(&frame, damage),
        Err(DriverError::SeatRevoked)
    );
    let mapper = host.mapper.as_ref().expect("mapper");
    assert_eq!(mapper.byte(0), 0x00, "a revoked present writes nothing");
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
        mmio_map: false,
        mapper: Some(MockMapper::new(8, true)),
        gate: None,
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
        mmio_map: true,
        mapper: Some(MockMapper::new(8, false)),
        gate: None,
    };
    assert_eq!(
        Framebuffer::open(&host, config(4, 2, 16)).err(),
        Some(DriverError::PermissionDenied)
    );
}

#[test]
fn open_without_mapper_is_unsupported() {
    let host = MockHost {
        mmio_map: true,
        mapper: None,
        gate: None,
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
fn present_gates_on_the_live_seat_lease() {
    let mut host = MockHost::full(8);
    host.gate = Some(MockGate {
        verdict: Cell::new(Ok(())),
    });
    let mut fb = Framebuffer::open(&host, config(4, 2, 16)).expect("open");
    let first = vec![0x11u8; 32];
    fb.present(&first).expect("live lease presents");

    // The lease is revoked between frames: the very next present is
    // refused with the distinct error before any surface access, so the
    // scan-out keeps the last-presented frame even though the mapping
    // still exists.
    let gate = host.gate.as_ref().expect("gate installed");
    gate.verdict.set(Err(DriverError::SeatRevoked));
    let overwrite = vec![0x22u8; 32];
    assert_eq!(fb.present(&overwrite), Err(DriverError::SeatRevoked));
    let mapper = host.mapper.as_ref().expect("mapper installed");
    assert_eq!(mapper.byte(0), 0x11);
    assert_eq!(mapper.byte(31), 0x11);

    // A stale handle also stays refused as a plain authority failure.
    gate.verdict.set(Err(DriverError::PermissionDenied));
    assert_eq!(fb.present(&overwrite), Err(DriverError::PermissionDenied));

    // Once the host's gate reports a live lease again (the new
    // foreground's grant), presents flow.
    gate.verdict.set(Ok(()));
    fb.present(&overwrite).expect("live lease presents again");
    assert_eq!(mapper.byte(0), 0x22);
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
