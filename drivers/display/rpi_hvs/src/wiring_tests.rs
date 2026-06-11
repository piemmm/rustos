//! Host unit tests for the driver-host wiring: the fail-closed paths of
//! [`open_discovered`] against the in-process mock mapper, and the
//! doorbell staging it performs before a (silent) firmware times the
//! exchange out. The happy path through [`open_with_transport`] is the
//! full-chain test in `mailbox_tests.rs` (mock firmware → wiring →
//! `present`); the real doorbell answer is the on-metal acceptance item
//! (`plans/PI.md` P7).

use super::*;
use crate::mailbox::tests::request;
use crate::tests::{MockHost, MockMapper};
use crate::{PlaneConfig, DEFAULT_BUS_ALIAS};

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
    // with the mapped `Timeout` — never an unbounded spin (§2.1).
    let host = MockHost {
        drv_load: true,
        mmio_map: true,
        mapper: Some(mailbox_mapper()),
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
