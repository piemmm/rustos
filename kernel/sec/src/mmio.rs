//! Capability gate for the MMIO register-window mapper.
//!
//! `kernel/mem::mmio::MmioMap` knows how to map a device's physical
//! register block into a process address space but, like
//! [`crate::dma::alloc_dma`]'s pool, takes no view on *who* may do so
//! (`AGENTS.md` §2.3). This module supplies the companion check.
//!
//! [`map_mmio`] / [`unmap_mmio`] are the only blessed entry points for
//! a bus driver that wants a [`RegisterWindow`](rustos_abi::RegisterWindow)
//! over a device's registers:
//!
//! 1. Verify the caller holds [`CapabilityId::MMIO_MAP`]. Refused
//!    callers receive [`MmioGateError::CapabilityMissing`] and the
//!    audit log records an [`AuditEvent::MmioMapDenied`] event.
//! 2. Delegate to [`MmioMap::map`] / [`MmioMap::unmap`].
//! 3. On a successful map emit [`AuditEvent::MmioMapped`] carrying the
//!    requesting `TaskId`, the physical base, and the length — every
//!    grant leaves a trail an operator can reconcile against device
//!    traffic (`AGENTS.md` §5.4.4).
//!
//! No `unsafe`, no `unwrap`, no `panic!` (`AGENTS.md` §2.9 / §2.10).

use rustos_abi::{CapabilityId, Errno};
use rustos_kernel_mem::{MmioError, MmioMap, MmioRegion, PageTableOps};
use rustos_log::{Field, Sink};

use crate::audit::{record, AuditEvent};
use crate::captable::TaskCapabilities;
use crate::identity::{format_hex_u64, format_usize};

/// Failure modes of the capability-gated MMIO entry points.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
#[non_exhaustive]
pub enum MmioGateError {
    /// The calling task does not hold [`CapabilityId::MMIO_MAP`].
    CapabilityMissing,
    /// The MMIO mapper refused the request. The inner error carries
    /// the underlying reason.
    Map(MmioError),
}

impl MmioGateError {
    /// Map this gate error to the stable [`Errno`] surface used by the
    /// (future) MMIO-map syscall.
    ///
    /// * [`Self::CapabilityMissing`] → [`Errno::PermissionDenied`].
    /// * [`Self::Map`]`(`[`MmioError::InvalidRegion`]`)` →
    ///   [`Errno::LengthOutOfRange`].
    /// * [`Self::Map`]`(`[`MmioError::NoVirtualSpace`]`)` →
    ///   [`Errno::LengthOutOfRange`] — the closest `abi-v1` variant for
    ///   "no room to satisfy the request".
    /// * Other internal mapper failures (page-table errors, guard
    ///   violations, unknown region, invalid config) collapse to
    ///   [`Errno::OutOfRange`] — these are kernel-side bugs with no
    ///   caller recovery action.
    #[must_use]
    pub fn as_errno(self) -> Errno {
        match self {
            Self::CapabilityMissing => Errno::PermissionDenied,
            Self::Map(MmioError::InvalidRegion | MmioError::NoVirtualSpace) => {
                Errno::LengthOutOfRange
            }
            Self::Map(_) => Errno::OutOfRange,
        }
    }
}

impl From<MmioError> for MmioGateError {
    fn from(e: MmioError) -> Self {
        Self::Map(e)
    }
}

/// Map a device register window for `caller`.
///
/// `map` is the caller's per-process MMIO mapper. The function
/// performs the capability check, delegates to [`MmioMap::map`] on
/// success, and emits the matching audit record either way.
///
/// # Errors
///
/// * [`MmioGateError::CapabilityMissing`] — `caller` does not hold
///   [`CapabilityId::MMIO_MAP`].
/// * [`MmioGateError::Map`] — propagated from the mapper (malformed
///   region, no virtual space, etc.).
pub fn map_mmio<P: PageTableOps, S: Sink + ?Sized>(
    map: &mut MmioMap<'_, P>,
    caller: &TaskCapabilities,
    phys_base: u64,
    len: usize,
    audit: &S,
) -> Result<MmioRegion, MmioGateError> {
    if !caller.has(CapabilityId::MMIO_MAP) {
        let mut task_buf = [0u8; 16];
        let mut uid_buf = [0u8; 12];
        let mut phys_buf = [0u8; 16];
        let mut len_buf = [0u8; 12];
        let task_str = format_hex_u64(caller.task().0, &mut task_buf);
        let uid_str = format_usize(caller.owner().0 as usize, &mut uid_buf);
        let phys_str = format_hex_u64(phys_base, &mut phys_buf);
        let len_str = format_usize(len, &mut len_buf);
        record(
            audit,
            AuditEvent::MmioMapDenied,
            &[
                Field {
                    key: "task",
                    value: task_str,
                },
                Field {
                    key: "uid",
                    value: uid_str,
                },
                Field {
                    key: "phys",
                    value: phys_str,
                },
                Field {
                    key: "len",
                    value: len_str,
                },
            ],
        );
        return Err(MmioGateError::CapabilityMissing);
    }
    let region = map.map(phys_base, len)?;
    let mut task_buf = [0u8; 16];
    let mut phys_buf = [0u8; 16];
    let mut len_buf = [0u8; 12];
    let task_str = format_hex_u64(caller.task().0, &mut task_buf);
    let phys_str = format_hex_u64(region.phys(), &mut phys_buf);
    let len_str = format_usize(region.len(), &mut len_buf);
    record(
        audit,
        AuditEvent::MmioMapped,
        &[
            Field {
                key: "task",
                value: task_str,
            },
            Field {
                key: "phys",
                value: phys_str,
            },
            Field {
                key: "len",
                value: len_str,
            },
        ],
    );
    Ok(region)
}

/// Release a previously-mapped device register window.
///
/// The capability check is identical to [`map_mmio`]: the kernel
/// refuses to unmap a window for a task that no longer holds
/// [`CapabilityId::MMIO_MAP`] (revocation is the explicit way to
/// terminate a misbehaving driver — its windows stay mapped until
/// reclaimed by a supervisor process that holds the capability).
///
/// # Errors
///
/// See [`map_mmio`].
pub fn unmap_mmio<P: PageTableOps, S: Sink + ?Sized>(
    map: &mut MmioMap<'_, P>,
    caller: &TaskCapabilities,
    region: MmioRegion,
    audit: &S,
) -> Result<(), MmioGateError> {
    if !caller.has(CapabilityId::MMIO_MAP) {
        let mut task_buf = [0u8; 16];
        let task_str = format_hex_u64(caller.task().0, &mut task_buf);
        record(
            audit,
            AuditEvent::MmioMapDenied,
            &[Field {
                key: "task",
                value: task_str,
            }],
        );
        return Err(MmioGateError::CapabilityMissing);
    }
    map.unmap(region).map_err(MmioGateError::from)
}

#[cfg(test)]
mod tests;
