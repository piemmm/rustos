//! Host unit tests for the WASM-linear-memory isolation model.
//!
//! These exercise the same boundary the browser "memory-isolation test
//! passes" vertical does: a victim and an attacker confined to disjoint
//! regions, where the attacker faults on a victim-only address.

use super::*;

#[test]
fn region_contains_in_range_accesses() {
    let r = MemoryRegion::new(0x1000, 0x1000);
    assert!(r.contains(0x1000, 1));
    assert!(r.contains(0x1000, 0x1000));
    assert!(r.contains(0x1fff, 1));
    // A zero-length access at the boundary end is contained.
    assert!(r.contains(0x2000, 0));
}

#[test]
fn region_rejects_out_of_range_and_overflowing_accesses() {
    let r = MemoryRegion::new(0x1000, 0x1000);
    // Below the base.
    assert!(!r.contains(0x0fff, 1));
    // Straddles the end.
    assert!(!r.contains(0x1fff, 2));
    // One past the end.
    assert!(!r.contains(0x2000, 1));
    // An access whose end overflows `u64` is never contained.
    assert!(!r.contains(u64::MAX, 2));
}

#[test]
fn check_access_returns_in_region_offset() {
    let space = AddressSpace::new(MemoryRegion::new(0x4000, 0x1000));
    assert_eq!(space.check_access(0x4000, 4), Ok(0));
    assert_eq!(space.check_access(0x4010, 4), Ok(0x10));
    assert_eq!(space.check_access(0x4ffc, 4), Ok(0xffc));
}

#[test]
fn check_access_faults_outside_the_region() {
    let region = MemoryRegion::new(0x4000, 0x1000);
    let space = AddressSpace::new(region);
    let fault = space.check_access(0x9000, 8).expect_err("must fault");
    assert_eq!(
        fault,
        WasmFault {
            addr: 0x9000,
            len: 8,
            region,
        }
    );
}

#[test]
fn attacker_faults_on_victim_only_address() {
    // The model the browser memory-isolation vertical asserts: two
    // workers confined to disjoint linear memories.
    let victim = AddressSpace::new(MemoryRegion::new(0x10_0000, 0x1000));
    let attacker = AddressSpace::new(MemoryRegion::new(0x20_0000, 0x1000));

    let secret = 0x10_0800; // inside the victim, outside the attacker.
    assert!(victim.can_read(secret), "victim owns its own page");
    assert!(
        !attacker.can_read(secret),
        "attacker must not reach the victim's page"
    );
    assert!(attacker.check_access(secret, 1).is_err());
    // The attacker can still freely touch its own region.
    assert!(attacker.check_access(0x20_0000, 0x1000).is_ok());
}

#[test]
fn end_saturates_at_the_top_of_the_address_space() {
    let r = MemoryRegion::new(u64::MAX - 4, 16);
    assert_eq!(r.end(), u64::MAX);
}
