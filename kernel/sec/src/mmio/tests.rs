//! Unit tests for the capability-gated MMIO entry points.
//!
//! Each test pairs a synthetic [`TaskCapabilities`] with a fresh
//! [`MmioMap`] driven by `kernel/mem`'s `HostPageTable`. The
//! [`RecordingSink`] from `crate::audit` captures the exact audit
//! sequence so the security trail is asserted alongside the
//! functional outcome.

extern crate alloc;

use super::*;
use crate::audit::RecordingSink;
use crate::captable::{TaskCapabilities, TaskId};
use crate::identity::UserId;
use tairix_abi::CapabilityId;
use tairix_caps::CapabilitySet;
use tairix_kernel_mem::{AddressSpace, HostPageTable, MmioError, PhysAddr, SimPhysMap, VirtAddr};

fn caps_of(items: &[CapabilityId]) -> CapabilitySet {
    let mut s = CapabilitySet::empty();
    for c in items {
        s.insert(*c);
    }
    s
}

fn task_with(caps: &[CapabilityId], sink: &RecordingSink) -> TaskCapabilities {
    let grant = caps_of(caps);
    TaskCapabilities::derive(TaskId(42), UserId(1000), grant, grant, sink)
}

/// Simulated register block covering the BAR addresses the tests map.
fn fresh_sim() -> SimPhysMap {
    SimPhysMap::new(PhysAddr::new(0xFEBD_0000), 0x1_0000)
}

fn fresh_map(phys: &SimPhysMap) -> MmioMap<'_, HostPageTable> {
    MmioMap::new(
        AddressSpace::new(HostPageTable::new()),
        VirtAddr::new(0x5000_0000),
        16,
        phys,
    )
    .expect("mapper constructs")
}

fn ids_after_derive(sink: &RecordingSink) -> alloc::vec::Vec<u32> {
    sink.ids()
        .into_iter()
        .filter(|&id| id != AuditEvent::TaskCapabilitiesDerived.id().0)
        .collect()
}

#[test]
fn map_succeeds_when_caller_holds_mmio_map() {
    let sim = fresh_sim();
    let mut map = fresh_map(&sim);
    let sink = RecordingSink::new();
    let caller = task_with(&[CapabilityId::MMIO_MAP], &sink);
    let region = map_mmio(&mut map, &caller, 0xFEBD_0000, 0x1000, &sink).expect("granted");
    assert_eq!(region.phys(), 0xFEBD_0000);
    assert_eq!(region.len(), 0x1000);
    assert_eq!(
        ids_after_derive(&sink),
        [AuditEvent::MmioMapped.id().0],
        "exactly one MmioMapped event must be emitted"
    );
    assert_eq!(map.live(), 1);
}

#[test]
fn map_refused_without_mmio_map() {
    let sim = fresh_sim();
    let mut map = fresh_map(&sim);
    let sink = RecordingSink::new();
    // The caller holds other capabilities but not MMIO_MAP.
    let caller = task_with(&[CapabilityId::FS_MOUNT, CapabilityId::MEM_DMA], &sink);
    let err = map_mmio(&mut map, &caller, 0xFEBD_0000, 0x1000, &sink).unwrap_err();
    assert_eq!(err, MmioGateError::CapabilityMissing);
    assert_eq!(err.as_errno(), Errno::PermissionDenied);
    assert_eq!(
        ids_after_derive(&sink),
        [AuditEvent::MmioMapDenied.id().0],
        "denial must produce exactly one MmioMapDenied event"
    );
    // Refusal must not map anything.
    assert_eq!(map.live(), 0);
}

#[test]
fn map_zero_length_propagates_mapper_error_without_denial() {
    let sim = fresh_sim();
    let mut map = fresh_map(&sim);
    let sink = RecordingSink::new();
    let caller = task_with(&[CapabilityId::MMIO_MAP], &sink);
    let err = map_mmio(&mut map, &caller, 0xFEBD_0000, 0, &sink).unwrap_err();
    assert_eq!(err, MmioGateError::Map(MmioError::InvalidRegion));
    assert_eq!(err.as_errno(), Errno::LengthOutOfRange);
    // Capability held, mapper refused — neither audit event fires.
    assert!(ids_after_derive(&sink).is_empty());
}

#[test]
fn map_then_unmap_round_trip_emits_one_grant_record() {
    let sim = fresh_sim();
    let mut map = fresh_map(&sim);
    let sink = RecordingSink::new();
    let caller = task_with(&[CapabilityId::MMIO_MAP], &sink);
    let region = map_mmio(&mut map, &caller, 0xFEBD_0000, 0x1000, &sink).expect("map");
    unmap_mmio(&mut map, &caller, region, &sink).expect("unmap");
    // Unmap is silent on success — the audit value is in the grant.
    assert_eq!(ids_after_derive(&sink), [AuditEvent::MmioMapped.id().0]);
    assert_eq!(map.live(), 0);
}

#[test]
fn unmap_refused_without_mmio_map_and_window_is_retained() {
    let sim = fresh_sim();
    let mut map = fresh_map(&sim);
    let grant_sink = RecordingSink::new();
    let granter = task_with(&[CapabilityId::MMIO_MAP], &grant_sink);
    let region = map_mmio(&mut map, &granter, 0xFEBD_0000, 0x1000, &grant_sink).expect("map");

    let revoked_sink = RecordingSink::new();
    let revoked = task_with(&[CapabilityId::FS_MOUNT], &revoked_sink);
    let err = unmap_mmio(&mut map, &revoked, region, &revoked_sink).unwrap_err();
    assert_eq!(err, MmioGateError::CapabilityMissing);
    assert_eq!(
        ids_after_derive(&revoked_sink),
        [AuditEvent::MmioMapDenied.id().0]
    );
    // The window must still be mapped.
    assert_eq!(map.live(), 1);
}

#[test]
fn as_errno_maps_every_mapper_error_into_abi_v1() {
    assert_eq!(
        MmioGateError::CapabilityMissing.as_errno(),
        Errno::PermissionDenied
    );
    assert_eq!(
        MmioGateError::Map(MmioError::InvalidRegion).as_errno(),
        Errno::LengthOutOfRange
    );
    assert_eq!(
        MmioGateError::Map(MmioError::NoVirtualSpace).as_errno(),
        Errno::LengthOutOfRange
    );
    assert_eq!(
        MmioGateError::Map(MmioError::UnknownRegion).as_errno(),
        Errno::OutOfRange
    );
    assert_eq!(
        MmioGateError::Map(MmioError::DirectMap).as_errno(),
        Errno::OutOfRange
    );
    assert_eq!(
        MmioGateError::Map(MmioError::InvalidMapConfig).as_errno(),
        Errno::OutOfRange
    );
}
