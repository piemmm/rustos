//! Driver register-reply record (`PLAN.md` Stage 4.HW).
//!
//! When the driver host spawns a verified driver image into its own
//! process (`AGENTS.md` §4 — drivers run in user space), the spawned
//! driver completes its `register()` entry in its own protection domain
//! and reports the outcome back to the host over IPC. This module is the
//! single wire definition of that reply: one fixed-size, versioned
//! record the driver process sends to the reply endpoint the host handed
//! it through its startup arguments.
//!
//! The record is *informational only*: the host mints its own
//! unforgeable [`DriverHandle`] on success, so a hostile driver that
//! forges a reply gains no authority — it can only mark *itself* as
//! registered or failed (`AGENTS.md` §5.2). Every field is validated on
//! decode and an inconsistent record is rejected whole (fail closed,
//! `AGENTS.md` §5.4).

use crate::le::{read_i32, read_u32, read_u64};

use super::{DriverError, DriverHandle};

/// Magic number identifying an `abi-v1` driver register reply
/// (`"DRR1"` little-endian).
pub const DRIVER_REGISTER_REPLY_MAGIC: u32 = u32::from_le_bytes(*b"DRR1");

/// `status` value carried by a successful [`DriverRegisterReply`].
///
/// Any other value is the [`DriverError::as_i32`] discriminant of the
/// failure the driver's `register()` reported.
pub const DRIVER_REGISTER_STATUS_OK: i32 = 0;

/// Outcome of a spawned driver process's `register()` entry, reported
/// to the driver host over IPC.
///
/// Field order is part of the `abi-v1` contract (mutable until the
/// first release, `AGENTS.md` §9). Construct with
/// [`DriverRegisterReply::registered`] / [`DriverRegisterReply::failed`]
/// so the `status`/`handle` consistency invariant cannot be violated;
/// [`DriverRegisterReply::from_bytes`] enforces the same invariant on
/// the receive side.
#[repr(C)]
#[derive(Copy, Clone, Debug, Eq, PartialEq)]
pub struct DriverRegisterReply {
    /// Must equal [`DRIVER_REGISTER_REPLY_MAGIC`].
    pub magic: u32,
    /// ABI version this reply targets; rejected if it does not match
    /// [`crate::ABI_VERSION_CURRENT`].
    pub abi_version: u32,
    /// [`DRIVER_REGISTER_STATUS_OK`] on success, otherwise the
    /// [`DriverError::as_i32`] discriminant `register()` reported.
    pub status: i32,
    /// Reserved; must be zero in `abi-v1`.
    pub reserved0: u32,
    /// The driver-reported handle's raw value when `status` is
    /// [`DRIVER_REGISTER_STATUS_OK`]; the [`DriverHandle::NONE`]
    /// sentinel (zero) otherwise.
    pub handle: u64,
}

impl DriverRegisterReply {
    /// Encoded size of a [`DriverRegisterReply`] on the wire.
    pub const WIRE_LEN: usize = 4 // magic
        + 4 // abi_version
        + 4 // status
        + 4 // reserved0
        + 8; // handle

    /// Build the reply for a `register()` that succeeded with `handle`.
    #[must_use]
    pub const fn registered(handle: DriverHandle) -> Self {
        Self {
            magic: DRIVER_REGISTER_REPLY_MAGIC,
            abi_version: crate::ABI_VERSION_CURRENT,
            status: DRIVER_REGISTER_STATUS_OK,
            reserved0: 0,
            handle: handle.as_u64(),
        }
    }

    /// Build the reply for a `register()` that failed with `error`.
    #[must_use]
    pub const fn failed(error: DriverError) -> Self {
        Self {
            magic: DRIVER_REGISTER_REPLY_MAGIC,
            abi_version: crate::ABI_VERSION_CURRENT,
            status: error.as_i32(),
            reserved0: 0,
            handle: DriverHandle::NONE.as_u64(),
        }
    }

    /// Encode `self` into its little-endian wire representation.
    #[must_use]
    pub fn to_le_bytes(&self) -> [u8; Self::WIRE_LEN] {
        let mut out = [0u8; Self::WIRE_LEN];
        out[0..4].copy_from_slice(&self.magic.to_le_bytes());
        out[4..8].copy_from_slice(&self.abi_version.to_le_bytes());
        out[8..12].copy_from_slice(&self.status.to_le_bytes());
        out[12..16].copy_from_slice(&self.reserved0.to_le_bytes());
        out[16..24].copy_from_slice(&self.handle.to_le_bytes());
        out
    }

    /// Decode `bytes` into a [`DriverRegisterReply`].
    ///
    /// # Errors
    ///
    /// * [`DriverError::BufferTooSmall`] if `bytes.len() < WIRE_LEN`.
    /// * [`DriverError::BadMagic`] if the magic word does not match or
    ///   `reserved0` is non-zero.
    /// * [`DriverError::AbiVersionUnsupported`] if `abi_version` is not
    ///   [`crate::ABI_VERSION_CURRENT`].
    /// * [`DriverError::OutOfRange`] if `status` names neither
    ///   [`DRIVER_REGISTER_STATUS_OK`] nor a known [`DriverError`]
    ///   discriminant, if a success reply carries the
    ///   [`DriverHandle::NONE`] sentinel, or if a failure reply carries
    ///   a non-zero handle.
    ///
    /// # Capabilities
    ///
    /// None. Decoding is pure; the *sending* of the reply is gated by
    /// the reply port's required send capability (`AGENTS.md` §5.2).
    pub fn from_bytes(bytes: &[u8]) -> Result<Self, DriverError> {
        if bytes.len() < Self::WIRE_LEN {
            return Err(DriverError::BufferTooSmall);
        }
        let magic = read_u32(bytes, 0);
        if magic != DRIVER_REGISTER_REPLY_MAGIC {
            return Err(DriverError::BadMagic);
        }
        let abi_version = read_u32(bytes, 4);
        if abi_version != crate::ABI_VERSION_CURRENT {
            return Err(DriverError::AbiVersionUnsupported);
        }
        let status = read_i32(bytes, 8);
        let reserved0 = read_u32(bytes, 12);
        if reserved0 != 0 {
            return Err(DriverError::BadMagic);
        }
        let handle = read_u64(bytes, 16);
        if status == DRIVER_REGISTER_STATUS_OK {
            if handle == DriverHandle::NONE.as_u64() {
                return Err(DriverError::OutOfRange);
            }
        } else {
            // The status must decode to a known failure, and a failed
            // registration carries no handle.
            DriverError::from_i32(status)?;
            if handle != DriverHandle::NONE.as_u64() {
                return Err(DriverError::OutOfRange);
            }
        }
        Ok(Self {
            magic,
            abi_version,
            status,
            reserved0,
            handle,
        })
    }

    /// The registration outcome this reply reports.
    ///
    /// # Errors
    ///
    /// The [`DriverError`] the driver's `register()` reported, when
    /// `status` is not [`DRIVER_REGISTER_STATUS_OK`].
    pub fn outcome(&self) -> Result<DriverHandle, DriverError> {
        if self.status == DRIVER_REGISTER_STATUS_OK {
            return DriverHandle::from_raw(self.handle);
        }
        Err(DriverError::from_i32(self.status)?)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn handle() -> DriverHandle {
        DriverHandle::from_raw(0x00C0_FFEE).expect("non-zero raw handle")
    }

    #[test]
    fn wire_size_is_twenty_four() {
        assert_eq!(DriverRegisterReply::WIRE_LEN, 24);
        assert_eq!(
            core::mem::size_of::<DriverRegisterReply>(),
            DriverRegisterReply::WIRE_LEN
        );
    }

    #[test]
    fn registered_reply_round_trips() {
        let reply = DriverRegisterReply::registered(handle());
        let decoded = DriverRegisterReply::from_bytes(&reply.to_le_bytes()).expect("valid wire");
        assert_eq!(decoded, reply);
        assert_eq!(decoded.outcome(), Ok(handle()));
    }

    #[test]
    fn failed_reply_round_trips() {
        let reply = DriverRegisterReply::failed(DriverError::DeviceFault);
        let decoded = DriverRegisterReply::from_bytes(&reply.to_le_bytes()).expect("valid wire");
        assert_eq!(decoded, reply);
        assert_eq!(decoded.outcome(), Err(DriverError::DeviceFault));
    }

    #[test]
    fn rejects_short_buffer() {
        let bytes = DriverRegisterReply::registered(handle()).to_le_bytes();
        assert_eq!(
            DriverRegisterReply::from_bytes(&bytes[..DriverRegisterReply::WIRE_LEN - 1]),
            Err(DriverError::BufferTooSmall)
        );
    }

    #[test]
    fn rejects_bad_magic() {
        let mut bytes = DriverRegisterReply::registered(handle()).to_le_bytes();
        bytes[0] ^= 0xFF;
        assert_eq!(
            DriverRegisterReply::from_bytes(&bytes),
            Err(DriverError::BadMagic)
        );
    }

    #[test]
    fn rejects_bad_abi_version() {
        let mut bytes = DriverRegisterReply::registered(handle()).to_le_bytes();
        bytes[4] = bytes[4].wrapping_add(1);
        assert_eq!(
            DriverRegisterReply::from_bytes(&bytes),
            Err(DriverError::AbiVersionUnsupported)
        );
    }

    #[test]
    fn rejects_nonzero_reserved() {
        let mut bytes = DriverRegisterReply::registered(handle()).to_le_bytes();
        bytes[12] = 1;
        assert_eq!(
            DriverRegisterReply::from_bytes(&bytes),
            Err(DriverError::BadMagic)
        );
    }

    #[test]
    fn rejects_unknown_status() {
        let mut reply = DriverRegisterReply::failed(DriverError::DeviceFault);
        reply.status = 999;
        assert_eq!(
            DriverRegisterReply::from_bytes(&reply.to_le_bytes()),
            Err(DriverError::OutOfRange)
        );
    }

    #[test]
    fn rejects_success_with_sentinel_handle() {
        let mut reply = DriverRegisterReply::registered(handle());
        reply.handle = 0;
        assert_eq!(
            DriverRegisterReply::from_bytes(&reply.to_le_bytes()),
            Err(DriverError::OutOfRange)
        );
    }

    #[test]
    fn rejects_failure_with_nonzero_handle() {
        let mut reply = DriverRegisterReply::failed(DriverError::Busy);
        reply.handle = 7;
        assert_eq!(
            DriverRegisterReply::from_bytes(&reply.to_le_bytes()),
            Err(DriverError::OutOfRange)
        );
    }

    #[test]
    fn outcome_of_forged_success_status_fails_closed() {
        // A hand-built reply (not via `from_bytes`) with an unknown
        // failure status still decodes to an error, never a handle.
        let reply = DriverRegisterReply {
            magic: DRIVER_REGISTER_REPLY_MAGIC,
            abi_version: crate::ABI_VERSION_CURRENT,
            status: -1,
            reserved0: 0,
            handle: 0,
        };
        assert_eq!(reply.outcome(), Err(DriverError::OutOfRange));
    }
}
