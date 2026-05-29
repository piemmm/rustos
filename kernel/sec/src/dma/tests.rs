//! Unit tests for the capability-gated DMA entry points.
//!
//! Every test pairs a synthetic [`TaskCapabilities`] with a fresh
//! [`DmaPool`] driven by `kernel/mem`'s `HostPageTable` and a small
//! `FrameAllocator`. The [`RecordingSink`] from `crate::audit` captures
//! the exact audit-event sequence so the security trail is asserted
//! alongside the functional outcome (`AGENTS.md` §5.4.4).

extern crate alloc;

use super::*;
use crate::audit::RecordingSink;
use crate::captable::{TaskCapabilities, TaskId};
use crate::identity::UserId;
use rustos_abi::CapabilityId;
use rustos_caps::CapabilitySet;
use rustos_kernel_mem::{
    bootinfo::{BootMemoryMap, MemoryRegion, RegionKind},
    AddressSpace, DmaPool, FrameAllocator, HostPageTable, PhysAddr, VirtAddr, PAGE_SIZE,
};

fn caps_of(items: &[CapabilityId]) -> CapabilitySet {
    let mut s = CapabilitySet::empty();
    for c in items {
        s.insert(*c);
    }
    s
}

fn task_with(caps: &[CapabilityId], sink: &RecordingSink) -> TaskCapabilities {
    let user_grant = caps_of(caps);
    let manifest_request = user_grant;
    TaskCapabilities::derive(TaskId(42), UserId(1000), user_grant, manifest_request, sink)
}

fn small_map(usable_pages: usize) -> BootMemoryMap {
    let mut m = BootMemoryMap::new();
    m.push(MemoryRegion {
        kind: RegionKind::Usable,
        start: PhysAddr::new(PAGE_SIZE as u64 * 16),
        length: (PAGE_SIZE * usable_pages) as u64,
    });
    m
}

fn fresh_pool(frames: &FrameAllocator) -> DmaPool<'_, HostPageTable> {
    DmaPool::new(
        AddressSpace::new(HostPageTable::new()),
        VirtAddr::new(0x2000_0000),
        16,
        frames,
    )
    .expect("pool constructs")
}

/// Recording sink that ignores the `TaskCapabilitiesDerived` event
/// emitted by [`task_with`] so each test asserts only DMA-relevant ids.
fn ids_after_derive(sink: &RecordingSink) -> alloc::vec::Vec<u32> {
    sink.ids()
        .into_iter()
        .filter(|&id| id != AuditEvent::TaskCapabilitiesDerived.id().0)
        .collect()
}

#[test]
fn alloc_succeeds_when_caller_holds_mem_dma() {
    let frames = FrameAllocator::new(&small_map(16)).unwrap();
    let mut pool = fresh_pool(&frames);
    let sink = RecordingSink::new();
    let caller = task_with(&[CapabilityId::MEM_DMA], &sink);
    let buf = alloc_dma(&mut pool, &caller, PAGE_SIZE, &sink).expect("granted");
    assert_eq!(buf.len(), PAGE_SIZE);
    assert_eq!(
        ids_after_derive(&sink),
        [AuditEvent::DmaAllocated.id().0],
        "exactly one DmaAllocated event must be emitted"
    );
}

#[test]
fn alloc_refused_without_mem_dma() {
    let frames = FrameAllocator::new(&small_map(16)).unwrap();
    let mut pool = fresh_pool(&frames);
    let sink = RecordingSink::new();
    // The caller holds *other* capabilities but not MEM_DMA, so the
    // gate is the only thing standing between it and the buffer.
    let caller = task_with(&[CapabilityId::FS_MOUNT, CapabilityId::NET_RAW], &sink);
    let err = alloc_dma(&mut pool, &caller, PAGE_SIZE, &sink).unwrap_err();
    assert_eq!(err, DmaGateError::CapabilityMissing);
    assert_eq!(err.as_errno(), Errno::PermissionDenied);
    assert_eq!(
        ids_after_derive(&sink),
        [AuditEvent::DmaAllocDenied.id().0],
        "denial must produce exactly one DmaAllocDenied event"
    );
    // Refusal must not consume any frames.
    assert_eq!(pool.live(), 0);
}

#[test]
fn alloc_zero_size_propagates_pool_error_with_audit() {
    let frames = FrameAllocator::new(&small_map(16)).unwrap();
    let mut pool = fresh_pool(&frames);
    let sink = RecordingSink::new();
    let caller = task_with(&[CapabilityId::MEM_DMA], &sink);
    let err = alloc_dma(&mut pool, &caller, 0, &sink).unwrap_err();
    assert_eq!(err, DmaGateError::Pool(DmaError::ZeroSize));
    assert_eq!(err.as_errno(), Errno::BufferTooSmall);
    // The capability check passed, the pool refused — no DmaAllocated
    // record (the alloc never succeeded) and no DmaAllocDenied (the
    // capability was held).
    assert!(ids_after_derive(&sink).is_empty());
}

#[test]
fn alloc_oversized_request_maps_to_length_out_of_range() {
    let frames = FrameAllocator::new(&small_map(16)).unwrap();
    let mut pool = fresh_pool(&frames);
    let sink = RecordingSink::new();
    let caller = task_with(&[CapabilityId::MEM_DMA], &sink);
    // Exceed MAX_ORDER ⇒ DmaError::SizeUnsupported.
    let too_big = (1usize << (rustos_kernel_mem::MAX_ORDER + 1)) * PAGE_SIZE;
    let err = alloc_dma(&mut pool, &caller, too_big, &sink).unwrap_err();
    assert_eq!(err.as_errno(), Errno::LengthOutOfRange);
}

#[test]
fn alloc_then_free_round_trip_emits_one_audit_record() {
    let frames = FrameAllocator::new(&small_map(16)).unwrap();
    let mut pool = fresh_pool(&frames);
    let sink = RecordingSink::new();
    let caller = task_with(&[CapabilityId::MEM_DMA], &sink);
    let buf = alloc_dma(&mut pool, &caller, PAGE_SIZE, &sink).expect("alloc");
    free_dma(&mut pool, &caller, buf, &sink).expect("free");
    // Free is silent on success — the audit value is in the *grant*,
    // not the matching release. One DmaAllocated event is correct.
    assert_eq!(ids_after_derive(&sink), [AuditEvent::DmaAllocated.id().0],);
    assert_eq!(pool.live(), 0);
}

#[test]
fn free_refused_without_mem_dma_and_buffer_is_retained() {
    let frames = FrameAllocator::new(&small_map(16)).unwrap();
    let mut pool = fresh_pool(&frames);
    let granted_sink = RecordingSink::new();
    let granter = task_with(&[CapabilityId::MEM_DMA], &granted_sink);
    let buf = alloc_dma(&mut pool, &granter, PAGE_SIZE, &granted_sink).expect("alloc");

    let revoked_sink = RecordingSink::new();
    let revoked = task_with(&[CapabilityId::FS_MOUNT], &revoked_sink);
    let err = free_dma(&mut pool, &revoked, buf, &revoked_sink).unwrap_err();
    assert_eq!(err, DmaGateError::CapabilityMissing);
    assert_eq!(err.as_errno(), Errno::PermissionDenied);
    assert_eq!(
        ids_after_derive(&revoked_sink),
        [AuditEvent::DmaAllocDenied.id().0]
    );
    // The buffer must still be live in the pool.
    assert_eq!(pool.live(), 1);
}

#[test]
fn as_errno_maps_every_pool_error_into_abi_v1() {
    // Every variant of `DmaError` must produce a valid `abi-v1`
    // errno; the test fails if a future variant slips through
    // without a deliberate decision in `as_errno`.
    assert_eq!(
        DmaGateError::CapabilityMissing.as_errno(),
        Errno::PermissionDenied
    );
    assert_eq!(
        DmaGateError::Pool(DmaError::ZeroSize).as_errno(),
        Errno::BufferTooSmall
    );
    assert_eq!(
        DmaGateError::Pool(DmaError::SizeUnsupported).as_errno(),
        Errno::LengthOutOfRange
    );
    assert_eq!(
        DmaGateError::Pool(DmaError::Alloc(rustos_kernel_mem::AllocError::OutOfMemory)).as_errno(),
        Errno::LengthOutOfRange
    );
    assert_eq!(
        DmaGateError::Pool(DmaError::UnknownBuffer).as_errno(),
        Errno::OutOfRange
    );
    assert_eq!(
        DmaGateError::Pool(DmaError::GuardViolation).as_errno(),
        Errno::OutOfRange
    );
    assert_eq!(
        DmaGateError::Pool(DmaError::InvalidPoolConfig).as_errno(),
        Errno::OutOfRange
    );
}
