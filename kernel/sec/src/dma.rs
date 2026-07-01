//! Capability gate for the per-process DMA allocator.
//!
//! `kernel/mem::dma::DmaPool` is intentionally capability-agnostic: it
//! knows how to carve, map, and zero DMA-able pages but takes no view
//! on *who* is allowed to do so (every type must
//! justify its existence, and stacking the capability check inside the
//! pool would conflate two responsibilities). This module supplies the
//! companion check.
//!
//! [`alloc_dma`] / [`free_dma`] are the only blessed entry points for
//! a user-space driver that wants to talk to a bus-master device:
//!
//! 1. Verify that `caller` holds [`CapabilityId::MEM_DMA`]. Refused
//!    callers receive [`Errno::PermissionDenied`] and the audit log
//!    records an [`AuditEvent::DmaAllocDenied`] event with the
//!    refusing `TaskId` and `UserId`.
//! 2. Delegate to the pool's `alloc` / `free`.
//! 3. On success emit [`AuditEvent::DmaAllocated`] with the granted
//!    buffer's length, physical-address, and the requesting
//!    `TaskId` — every grant must leave a trail an operator can
//!    reconcile against device traffic.
//!
//! No `unsafe`, no `unwrap`, no `panic!`:.

use rustos_abi::{CapabilityId, Errno};
use rustos_kernel_mem::{DmaBuffer, DmaError, DmaPool, PageTable};
use rustos_log::{Field, Sink};

use crate::audit::{record, AuditEvent};
use crate::captable::TaskCapabilities;
use crate::identity::{format_hex_u64, format_usize};

/// Failure modes of the capability-gated DMA entry points.
///
/// Distinct from the bare [`DmaError`] because a capability refusal is
/// a security event, not an allocator failure, and callers (and the
/// future syscall wrapper) often want to surface them differently.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
#[non_exhaustive]
pub enum DmaGateError {
    /// The calling task does not hold [`CapabilityId::MEM_DMA`].
    CapabilityMissing,
    /// The DMA pool refused the request. The inner error carries the
    /// underlying reason.
    Pool(DmaError),
}

impl DmaGateError {
    /// Map this gate error to the stable [`Errno`] surface used by the
    /// (future) `dma_alloc` syscall.
    ///
    /// `abi-v1` does not yet ship a dedicated `OutOfMemory` errno;
    /// allocator exhaustion and over-large requests therefore both
    /// surface as [`Errno::LengthOutOfRange`], which is the closest
    /// stable variant ("a length, count, or offset field exceeds its
    /// ABI-mandated maximum"). A future `abi-v2` may split these.
    ///
    /// * [`Self::CapabilityMissing`] → [`Errno::PermissionDenied`].
    /// * [`Self::Pool`]`(`[`DmaError::Alloc`]`)` →
    ///   [`Errno::LengthOutOfRange`].
    /// * [`Self::Pool`]`(`[`DmaError::SizeUnsupported`]`)` →
    ///   [`Errno::LengthOutOfRange`].
    /// * [`Self::Pool`]`(`[`DmaError::ZeroSize`]`)` →
    ///   [`Errno::BufferTooSmall`].
    /// * [`Self::Pool`]`(`[`DmaError::UnknownBuffer`]`)` →
    ///   [`Errno::OutOfRange`].
    /// * Other internal pool failures (page-table errors, guard
    ///   violations, invalid pool config) collapse to
    ///   [`Errno::OutOfRange`] — these are kernel-side bugs and the
    ///   caller has no recovery action beyond reporting them.
    #[must_use]
    pub fn as_errno(self) -> Errno {
        match self {
            Self::CapabilityMissing => Errno::PermissionDenied,
            Self::Pool(DmaError::Alloc(_) | DmaError::SizeUnsupported) => Errno::LengthOutOfRange,
            Self::Pool(DmaError::ZeroSize) => Errno::BufferTooSmall,
            Self::Pool(_) => Errno::OutOfRange,
        }
    }
}

impl From<DmaError> for DmaGateError {
    fn from(e: DmaError) -> Self {
        Self::Pool(e)
    }
}

/// Allocate a DMA buffer for `caller`.
///
/// `pool` is the caller's per-process DMA pool. The function performs
/// the capability check, delegates to [`DmaPool::alloc`] on success,
/// and emits the matching audit record either way.
///
/// # Errors
///
/// * [`DmaGateError::CapabilityMissing`] — `caller` does not hold
///   [`CapabilityId::MEM_DMA`].
/// * [`DmaGateError::Pool`] — propagated from the pool (out of
///   memory, oversized request, etc.).
pub fn alloc_dma<P: PageTable, S: Sink + ?Sized>(
    pool: &mut DmaPool<'_, P>,
    caller: &TaskCapabilities,
    requested: usize,
    audit: &S,
) -> Result<DmaBuffer, DmaGateError> {
    if !caller.has(CapabilityId::MEM_DMA) {
        let mut task_buf = [0u8; 16];
        let mut uid_buf = [0u8; 12];
        let mut len_buf = [0u8; 12];
        let task_str = format_hex_u64(caller.task().0, &mut task_buf);
        let uid_str = format_usize(caller.owner().0 as usize, &mut uid_buf);
        let len_str = format_usize(requested, &mut len_buf);
        record(
            audit,
            AuditEvent::DmaAllocDenied,
            &[
                Field {
                    key: "task",
                    value: rustos_log::FieldValue::Str(task_str),
                },
                Field {
                    key: "uid",
                    value: rustos_log::FieldValue::Str(uid_str),
                },
                Field {
                    key: "requested",
                    value: rustos_log::FieldValue::Str(len_str),
                },
            ],
        );
        return Err(DmaGateError::CapabilityMissing);
    }
    let buf = pool.alloc(requested)?;
    let mut task_buf = [0u8; 16];
    let mut len_buf = [0u8; 12];
    let mut phys_buf = [0u8; 16];
    let task_str = format_hex_u64(caller.task().0, &mut task_buf);
    let len_str = format_usize(buf.len(), &mut len_buf);
    let phys_str = format_hex_u64(buf.phys().as_u64(), &mut phys_buf);
    record(
        audit,
        AuditEvent::DmaAllocated,
        &[
            Field {
                key: "task",
                value: rustos_log::FieldValue::Str(task_str),
            },
            Field {
                key: "len",
                value: rustos_log::FieldValue::Str(len_str),
            },
            Field {
                key: "phys",
                value: rustos_log::FieldValue::Str(phys_str),
            },
        ],
    );
    Ok(buf)
}

/// Free a DMA buffer.
///
/// The capability check is identical to [`alloc_dma`]: the kernel
/// refuses to free a buffer for a task that no longer holds
/// [`CapabilityId::MEM_DMA`] (revocation is the explicit way to
/// terminate a misbehaving driver — its outstanding buffers stay
/// allocated until reclaimed by the supervisor process, which holds
/// the capability).
///
/// # Errors
///
/// See [`alloc_dma`].
pub fn free_dma<P: PageTable, S: Sink + ?Sized>(
    pool: &mut DmaPool<'_, P>,
    caller: &TaskCapabilities,
    buf: DmaBuffer,
    audit: &S,
) -> Result<(), DmaGateError> {
    if !caller.has(CapabilityId::MEM_DMA) {
        let mut task_buf = [0u8; 16];
        let task_str = format_hex_u64(caller.task().0, &mut task_buf);
        record(
            audit,
            AuditEvent::DmaAllocDenied,
            &[Field {
                key: "task",
                value: rustos_log::FieldValue::Str(task_str),
            }],
        );
        return Err(DmaGateError::CapabilityMissing);
    }
    pool.free(buf).map_err(DmaGateError::from)
}

#[cfg(test)]
mod tests;
