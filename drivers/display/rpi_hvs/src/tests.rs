//! Unit tests for the Raspberry Pi HVS driver against an in-process
//! mock [`MmioMapper`] that backs several distinct physical regions
//! (scan-out, display-list RAM, control, and the per-plane buffers).

extern crate alloc;

use alloc::rc::Rc;
use alloc::vec;
use alloc::vec::Vec;
use core::cell::RefCell;
use core::ptr::NonNull;

use super::*;
use rustos_abi::driver::display::{AccelLayer, DisplayFormat};
use rustos_abi::driver::DriverKind;
use rustos_abi::{CapabilityId, MmioMapError, MmioMapper, RegisterWindow};

const SCANOUT_PHYS: u64 = 0x1000_0000;
const DLIST_PHYS: u64 = 0x1100_0000;
const CONTROL_PHYS: u64 = 0x1200_0000;
const PLANE0_PHYS: u64 = 0x2000_0000;
const PLANE_STRIDE: u64 = 0x0010_0000;

/// One backing region keyed by its physical base.
struct Region {
    phys: u64,
    backing: Rc<RefCell<Vec<u32>>>,
}

/// Multi-region mock mapper. Each registered region is backed by a
/// `u32` buffer (≥ 4-byte aligned) shared with the test for read-back.
/// `pub(crate)` so the mailbox discovery chain test reuses it (§2.2).
pub(crate) struct MockMapper {
    regions: Vec<Region>,
    granted: bool,
}

impl MockMapper {
    pub(crate) fn new(granted: bool) -> Self {
        Self {
            regions: Vec::new(),
            granted,
        }
    }

    pub(crate) fn add(&mut self, phys: u64, words: usize) {
        self.regions.push(Region {
            phys,
            backing: Rc::new(RefCell::new(vec![0u32; words])),
        });
    }

    fn region(&self, phys: u64) -> Rc<RefCell<Vec<u32>>> {
        self.regions
            .iter()
            .find(|r| r.phys == phys)
            .map(|r| Rc::clone(&r.backing))
            .expect("region registered")
    }

    fn word(&self, phys: u64, index: usize) -> u32 {
        self.region(phys).borrow()[index]
    }

    pub(crate) fn byte(&self, phys: u64, off: usize) -> u8 {
        self.region(phys).borrow()[off / 4].to_le_bytes()[off % 4]
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
        let region = self
            .regions
            .iter()
            .find(|r| r.phys == phys_base)
            .ok_or(MmioMapError::Unsupported)?;
        let mut backing = region.backing.borrow_mut();
        if len > backing.len() * 4 {
            return Err(MmioMapError::Unsupported);
        }
        let base = NonNull::new(backing.as_mut_ptr().cast::<u8>()).expect("non-null heap buffer");
        // SAFETY: `base` covers `backing.len() * 4 >= len` bytes and is
        // 4-byte aligned (the `Vec<u32>` allocation guarantee); the
        // backing lives in an `Rc` that outlives every minted window,
        // and the tests touch it through `word()` / `byte()` only
        // between — never during — the driver's writes.
        Ok(unsafe { RegisterWindow::from_mapping(phys_base, base, len) })
    }
}

pub(crate) struct MockHost {
    pub(crate) drv_load: bool,
    pub(crate) mmio_map: bool,
    pub(crate) mapper: Option<MockMapper>,
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

/// A 4×2 BGRA scan-out (stride 16) with `plane_count` planes and a
/// display list large enough for them. The mapper backs every region.
fn host_with(plane_count: usize) -> MockHost {
    let mut mapper = MockMapper::new(true);
    mapper.add(SCANOUT_PHYS, 8); // 32 bytes
    mapper.add(DLIST_PHYS, 64); // 256 bytes — ample
    mapper.add(CONTROL_PHYS, 2); // 8 bytes
    for i in 0..plane_count {
        let i = u64::try_from(i).expect("plane index");
        mapper.add(PLANE0_PHYS + i * PLANE_STRIDE, 8); // 32 bytes each
    }
    MockHost {
        drv_load: true,
        mmio_map: true,
        mapper: Some(mapper),
    }
}

fn config(plane_count: usize) -> HvsConfig {
    let mut planes = [PlaneConfig {
        phys_base: 0,
        len_bytes: 0,
    }; MAX_PLANES];
    for (i, plane) in planes.iter_mut().enumerate().take(plane_count) {
        let i = u64::try_from(i).expect("plane index");
        plane.phys_base = PLANE0_PHYS + i * PLANE_STRIDE;
        plane.len_bytes = 32;
    }
    HvsConfig {
        scanout: ScanoutConfig {
            phys_base: SCANOUT_PHYS,
            width_px: 4,
            height_px: 2,
            stride_bytes: 16,
            format: DisplayFormat::Bgra8888,
        },
        dlist_phys_base: DLIST_PHYS,
        dlist_len_bytes: 256,
        control_phys_base: CONTROL_PHYS,
        planes,
        plane_count,
        bus_alias: DEFAULT_BUS_ALIAS,
    }
}

fn layer(pixels: &[u8], w: u32, h: u32, stride: u32, x: i32, y: i32) -> AccelLayer<'_> {
    AccelLayer {
        pixels,
        width_px: w,
        height_px: h,
        stride_bytes: stride,
        dst_x: x,
        dst_y: y,
        opacity: 255,
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
fn open_reports_mode_and_caps() {
    let host = host_with(2);
    let hvs = RpiHvs::open(&host, config(2)).expect("open");
    assert_eq!(
        hvs.mode_info().expect("mode"),
        DisplayMode {
            width_px: 4,
            height_px: 2,
            stride_bytes: 16,
            format: DisplayFormat::Bgra8888,
        }
    );
    let caps = hvs.accel_caps().expect("caps");
    assert_eq!(caps.max_layers, 2);
    assert_eq!(caps.max_width_px, 4);
    assert_eq!(caps.max_height_px, 2);
    assert!(caps.per_layer_opacity);
}

#[test]
fn software_present_copies_frame_into_scanout() {
    let host = host_with(1);
    let mut hvs = RpiHvs::open(&host, config(1)).expect("open");
    let frame: Vec<u8> = (0..32u8).collect();
    hvs.present(&frame).expect("present");
    let mapper = host.mapper.as_ref().expect("mapper");
    for (off, expected) in frame.iter().enumerate() {
        assert_eq!(mapper.byte(SCANOUT_PHYS, off), *expected, "byte {off}");
    }
}

#[test]
fn software_present_rejects_short_frame() {
    let host = host_with(1);
    let mut hvs = RpiHvs::open(&host, config(1)).expect("open");
    assert_eq!(hvs.present(&[0u8; 31]), Err(DriverError::BufferTooSmall));
}

#[test]
fn present_layers_uploads_planes_and_builds_display_list() {
    let host = host_with(2);
    let mut hvs = RpiHvs::open(&host, config(2)).expect("open");

    // Two 4×1 BGRA layers (16 bytes each) at distinct positions.
    let bottom: Vec<u8> = (0..16u8).collect();
    let top: Vec<u8> = (0..16u8).map(|b| b.wrapping_add(100)).collect();
    let layers = [layer(&bottom, 4, 1, 16, 0, 0), layer(&top, 4, 1, 16, 1, 1)];
    hvs.present_layers(&layers).expect("present_layers");

    let mapper = host.mapper.as_ref().expect("mapper");

    // Each plane buffer received its layer's bytes verbatim.
    for (off, expected) in bottom.iter().enumerate() {
        assert_eq!(
            mapper.byte(PLANE0_PHYS, off),
            *expected,
            "plane0 byte {off}"
        );
    }
    for (off, expected) in top.iter().enumerate() {
        assert_eq!(
            mapper.byte(PLANE0_PHYS + PLANE_STRIDE, off),
            *expected,
            "plane1 byte {off}"
        );
    }

    // The display list holds two 6-word plane entries plus an end
    // marker (13 words). Spot-check the structural fields.
    let bus0 = DEFAULT_BUS_ALIAS | u32::try_from(PLANE0_PHYS).unwrap();
    let bus1 = DEFAULT_BUS_ALIAS | u32::try_from(PLANE0_PHYS + PLANE_STRIDE).unwrap();
    assert_ne!(mapper.word(DLIST_PHYS, 0) & (1 << 31), 0, "ctl0 valid bit");
    assert_eq!(mapper.word(DLIST_PHYS, 3), bus0, "plane0 pointer");
    assert_eq!(mapper.word(DLIST_PHYS, 4), 16, "plane0 pitch");
    assert_eq!(mapper.word(DLIST_PHYS, 5), 255, "plane0 alpha");
    // Second entry begins at word 6; position packs x=1,y=1.
    assert_eq!(mapper.word(DLIST_PHYS, 7), 1 | (1 << 16), "plane1 position");
    assert_eq!(mapper.word(DLIST_PHYS, 9), bus1, "plane1 pointer");
    // End marker after the two entries.
    assert_eq!(mapper.word(DLIST_PHYS, 12), 0x8000_0000, "end marker");

    // The control window records a present (generation bumped to 1).
    assert_eq!(mapper.word(CONTROL_PHYS, 0), 0, "dlist head offset");
    assert_eq!(mapper.word(CONTROL_PHYS, 1), 1, "present generation");
}

#[test]
fn present_layers_rejects_more_layers_than_planes() {
    let host = host_with(1);
    let mut hvs = RpiHvs::open(&host, config(1)).expect("open");
    let px = [0u8; 16];
    let two = [layer(&px, 4, 1, 16, 0, 0), layer(&px, 4, 1, 16, 0, 0)];
    assert_eq!(hvs.present_layers(&two), Err(DriverError::LengthOutOfRange));
}

#[test]
fn present_layers_rejects_layer_larger_than_screen() {
    let host = host_with(1);
    let mut hvs = RpiHvs::open(&host, config(1)).expect("open");
    let px = [0u8; 16];
    // width 8 > screen width 4.
    let big = [layer(&px, 8, 1, 16, 0, 0)];
    assert_eq!(hvs.present_layers(&big), Err(DriverError::LengthOutOfRange));
}

#[test]
fn present_layers_rejects_short_pixels() {
    let host = host_with(1);
    let mut hvs = RpiHvs::open(&host, config(1)).expect("open");
    let px = [0u8; 8]; // needs 16 for 4×1 BGRA.
    let short = [layer(&px, 4, 1, 16, 0, 0)];
    assert_eq!(hvs.present_layers(&short), Err(DriverError::BufferTooSmall));
}

#[test]
fn open_requires_mmio_map_capability() {
    let host = MockHost {
        drv_load: true,
        mmio_map: false,
        mapper: Some(MockMapper::new(true)),
    };
    assert_eq!(
        RpiHvs::open(&host, config(1)).err(),
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
        RpiHvs::open(&host, config(1)).err(),
        Some(DriverError::Unsupported)
    );
}

#[test]
fn config_rejects_zero_planes_and_tiny_display_list() {
    assert_eq!(config(0).validate(), Err(DriverError::LengthOutOfRange));
    let mut cfg = config(2);
    cfg.dlist_len_bytes = 16; // 4 words < the 13 two planes need.
    assert_eq!(cfg.validate(), Err(DriverError::LengthOutOfRange));
}

#[test]
fn bus_address_rejects_out_of_aperture() {
    let cfg = config(1);
    assert!(cfg.bus_address(0x3FFF_FFFF).is_ok());
    assert_eq!(
        cfg.bus_address(0x4000_0000),
        Err(DriverError::LengthOutOfRange)
    );
}
