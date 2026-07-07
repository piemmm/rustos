//! Host unit tests for the driver-host wiring: the fail-closed paths of
//! [`open_discovered`] against the in-process mock mapper, the doorbell
//! staging it performs before a (silent) firmware times the exchange
//! out, and the happy path through [`open_with_transport`] (the shared
//! mock firmware → wiring → `present` full chain); the real doorbell
//! answer is the on-metal acceptance item (`plans/PI.md` P7).

use rustos_vcmailbox::mock::MockFirmware;

use super::*;
use crate::tests::{MockHost, MockMapper};
use crate::{PlaneConfig, DEFAULT_BUS_ALIAS};

/// Geometry every wiring test requests: 640×480 BGRA (the shared mock
/// firmware's healthy surface).
fn request() -> FramebufferRequest {
    FramebufferRequest {
        width_px: 640,
        height_px: 480,
        format: rustos_abi::driver::display::DisplayFormat::Bgra8888,
    }
}

/// ARM-physical base the tests place the doorbell block at.
const REGS_PHYS: u64 = 0x3000_0000;
/// In-aperture, 16-byte-aligned property-buffer carve.
const BUFFER_PHYS: u64 = 0x1000_0000;

/// A minimal, valid region set (never reached by the fail-closed tests).
fn regions() -> HvsRegions {
    let mut planes = [PlaneConfig {
        phys_base: 0,
        len_bytes: 0,
    }; MAX_PLANES];
    planes[0] = PlaneConfig {
        phys_base: 0x2000_0000,
        len_bytes: 32,
    };
    HvsRegions {
        dlist_phys_base: 0x1100_0000,
        dlist_len_bytes: 256,
        control_phys_base: 0x1200_0000,
        planes,
        plane_count: 1,
    }
}

fn wiring_at(buffer_phys: u64) -> MailboxWiring {
    MailboxWiring {
        regs_phys: REGS_PHYS,
        buffer_phys,
        bus_alias: DEFAULT_BUS_ALIAS,
        poll_budget: 4,
    }
}

/// A mapper backing the doorbell block and the property buffer.
fn mailbox_mapper() -> MockMapper {
    let mut mapper = MockMapper::new(true);
    mapper.add(REGS_PHYS, MAILBOX_REGS_LEN_BYTES / 4);
    mapper.add(BUFFER_PHYS, PROPERTY_LEN_BYTES / 4);
    mapper
}

#[test]
fn open_discovered_requires_the_mmio_capability() {
    let host = MockHost {
        drv_load: true,
        mmio_map: false,
        mapper: Some(mailbox_mapper()),
        gate: None,
    };
    assert_eq!(
        open_discovered(&host, &wiring_at(BUFFER_PHYS), &request(), &regions()).err(),
        Some(DriverError::PermissionDenied)
    );
}

#[test]
fn open_discovered_requires_a_mapper() {
    let host = MockHost {
        drv_load: true,
        mmio_map: true,
        mapper: None,
        gate: None,
    };
    assert_eq!(
        open_discovered(&host, &wiring_at(BUFFER_PHYS), &request(), &regions()).err(),
        Some(DriverError::Unsupported)
    );
}

#[test]
fn open_discovered_rejects_a_buffer_outside_the_aperture() {
    // The carve sits at the 30-bit aperture limit: mapping succeeds but
    // the bus translation fails closed before the doorbell rings.
    const OUT_OF_APERTURE: u64 = 0x4000_0000;
    let mut mapper = MockMapper::new(true);
    mapper.add(REGS_PHYS, MAILBOX_REGS_LEN_BYTES / 4);
    mapper.add(OUT_OF_APERTURE, PROPERTY_LEN_BYTES / 4);
    let host = MockHost {
        drv_load: true,
        mmio_map: true,
        mapper: Some(mapper),
        gate: None,
    };
    assert_eq!(
        open_discovered(&host, &wiring_at(OUT_OF_APERTURE), &request(), &regions()).err(),
        Some(DriverError::LengthOutOfRange)
    );
}

#[test]
fn open_discovered_stages_rings_and_times_out_on_a_silent_firmware() {
    // RAM-backed doorbell: the write side accepts, but nothing ever
    // answers on the property channel, so the bounded poll fails closed
    // with the mapped `Timeout` — never an unbounded spin.
    let host = MockHost {
        drv_load: true,
        mmio_map: true,
        mapper: Some(mailbox_mapper()),
        gate: None,
    };
    assert_eq!(
        open_discovered(&host, &wiring_at(BUFFER_PHYS), &request(), &regions()).err(),
        Some(DriverError::DeviceFault)
    );

    let mapper = host.mapper.as_ref().expect("mapper");
    // The request was staged into the DMA property buffer (its first
    // word is the message byte length the encoder writes).
    let staged = u32::from_le_bytes([
        mapper.byte(BUFFER_PHYS, 0),
        mapper.byte(BUFFER_PHYS, 1),
        mapper.byte(BUFFER_PHYS, 2),
        mapper.byte(BUFFER_PHYS, 3),
    ]);
    assert_eq!(staged, 30 * 4, "encoded request staged into the carve");
    // The doorbell (MBOX1 write register, offset 0x20) was rung with
    // the carve's bus address on the property channel (8).
    let posted = u32::from_le_bytes([
        mapper.byte(REGS_PHYS, 0x20),
        mapper.byte(REGS_PHYS, 0x21),
        mapper.byte(REGS_PHYS, 0x22),
        mapper.byte(REGS_PHYS, 0x23),
    ]);
    let expected_bus = arm_physical_to_bus(BUFFER_PHYS, DEFAULT_BUS_ALIAS).expect("translate");
    assert_eq!(posted, expected_bus | 8, "doorbell posted bus | channel");
}

/// The P7 emulation artefact: the shared protocol-faithful mock
/// firmware answers the property exchange, the decoded response becomes
/// the [`ScanoutConfig`], and [`RpiHvs::open`] consumes it and presents
/// a frame into the discovered surface. The real scan-out (HVS
/// hardware, HDMI) is a metal acceptance item (`plans/PI.md` P7):
/// QEMU's `virt` RAM begins at `0x4000_0000`, outside the BCM2711
/// 30-bit `VideoCore` aperture, so no honest `virt` vertical can carry
/// this chain.
#[test]
fn discovered_config_opens_the_hvs_driver() {
    extern crate alloc;
    use alloc::vec;

    use rustos_abi::driver::display::Display;
    use rustos_vcmailbox::discover_framebuffer;

    const DLIST_PHYS: u64 = 0x1100_0000;
    const CONTROL_PHYS: u64 = 0x1200_0000;
    const PLANE_PHYS: u64 = 0x2000_0000;

    let mut firmware = MockFirmware::healthy();
    let fb = discover_framebuffer(&mut firmware, &request()).expect("discover");
    let scanout = ScanoutConfig::from_firmware(&fb).expect("scanout");
    let surface_len = scanout.surface_len().expect("surface length");

    let mut mapper = MockMapper::new(true);
    mapper.add(scanout.phys_base, surface_len / 4);
    mapper.add(DLIST_PHYS, 64);
    mapper.add(CONTROL_PHYS, 2);
    mapper.add(PLANE_PHYS, 8);
    let host = MockHost {
        drv_load: true,
        mmio_map: true,
        mapper: Some(mapper),
        gate: None,
    };

    let mut planes = [PlaneConfig {
        phys_base: 0,
        len_bytes: 0,
    }; MAX_PLANES];
    planes[0] = PlaneConfig {
        phys_base: PLANE_PHYS,
        len_bytes: 32,
    };
    let regions = HvsRegions {
        dlist_phys_base: DLIST_PHYS,
        dlist_len_bytes: 256,
        control_phys_base: CONTROL_PHYS,
        planes,
        plane_count: 1,
    };

    let mut hvs = open_with_transport(&host, &mut firmware, &request(), &regions)
        .expect("open through the wiring");
    assert_eq!(hvs.mode_info().expect("mode"), scanout.mode());

    let frame = vec![0xA5u8; surface_len];
    hvs.present(&frame).expect("present");
    let mapper = host.mapper.as_ref().expect("mapper");
    for off in [0, 1, surface_len / 2, surface_len - 1] {
        assert_eq!(mapper.byte(scanout.phys_base, off), 0xA5, "byte {off}");
    }
}
