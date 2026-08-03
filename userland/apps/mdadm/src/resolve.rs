//! Resolve the TAIRiX-specific operand spellings to what the control wire
//! names.
//!
//! There is no `/dev`, so:
//!
//! * a **device** is named by its hardware-tree node id, spelled `node:<id>`
//!   (the same id the reports print), and
//! * an **array** is named by its 128-bit identity as lower-case hexadecimal,
//!   accepting the full 32-digit identity or any unambiguous prefix of it.
//!
//! Both resolutions fail closed: an unparseable spelling, an identity that
//! matches no array, and — the important one — a prefix that matches more than
//! one array are all refused, never guessed at.

use alloc::string::{String, ToString};
use alloc::vec::Vec;

use tairix_abi::raid_admin::{
    ArrayUuidBytes, MemberNodeList, RaidArrayRecord, RAID_CREATE_MAX_MEMBERS,
};
use tairix_abi::Errno;

use crate::error::ResolveError;
use crate::render::format_identity;

/// The number of hexadecimal digits a full array identity has (16 bytes).
const IDENTITY_HEX_DIGITS: usize = 32;

/// Parse a device operand (`node:<id>`) into its hardware-tree node id.
///
/// # Errors
///
/// [`ResolveError::BadDeviceName`] if the operand is not `node:<decimal>` with
/// a non-zero id (a zero id names no discovered device).
pub fn resolve_device(operand: &str) -> Result<u32, ResolveError> {
    let bad = || ResolveError::BadDeviceName(operand.to_string());
    let digits = operand.strip_prefix("node:").ok_or_else(bad)?;
    let node = digits.parse::<u32>().map_err(|_| bad())?;
    if node == 0 {
        return Err(bad());
    }
    Ok(node)
}

/// Resolve an array operand (a full or partial hexadecimal identity) against
/// the live arrays.
///
/// # Errors
///
/// * [`ResolveError::BadArrayName`] — not lower/upper-case hexadecimal, empty,
///   or longer than a full identity.
/// * [`ResolveError::ArrayNotFound`] — a valid prefix that matches no array.
/// * [`ResolveError::AmbiguousArray`] — a prefix that matches more than one.
pub fn resolve_array(
    arrays: &[RaidArrayRecord],
    operand: &str,
) -> Result<ArrayUuidBytes, ResolveError> {
    let needle = normalise_prefix(operand)?;
    let mut found: Option<ArrayUuidBytes> = None;
    for array in arrays {
        let identity = format_identity(&array.array());
        if identity.starts_with(&needle) {
            if found.is_some() {
                return Err(ResolveError::AmbiguousArray(operand.to_string()));
            }
            found = Some(array.array());
        }
    }
    found.ok_or_else(|| ResolveError::ArrayNotFound(operand.to_string()))
}

/// Resolve a `--create` device operand list into the wire member list, in
/// slot order.
///
/// Each operand is a `node:<id>` device name; a duplicate device and a set
/// larger than any array can hold are refused here (fail closed) rather than
/// waiting for the composer, so the diagnostic names the offending operand.
///
/// # Errors
///
/// * [`ResolveError::BadDeviceName`] — an operand that is not `node:<id>`.
/// * [`ResolveError::DuplicateDevice`] — the same device named twice.
/// * [`ResolveError::TooManyDevices`] — more devices than
///   [`RAID_CREATE_MAX_MEMBERS`].
pub fn resolve_members(devices: &[String]) -> Result<MemberNodeList, ResolveError> {
    let mut nodes: Vec<u32> = Vec::with_capacity(devices.len());
    for device in devices {
        let node = resolve_device(device)?;
        if nodes.contains(&node) {
            return Err(ResolveError::DuplicateDevice(device.clone()));
        }
        nodes.push(node);
    }
    MemberNodeList::new(&nodes).map_err(|err| match err {
        Errno::AlreadyExists => {
            // The pre-check above already refused a duplicate with the
            // offending name; fall back defensively rather than losing the
            // reason.
            ResolveError::DuplicateDevice(String::from("duplicate"))
        }
        _ => ResolveError::TooManyDevices {
            got: devices.len(),
            max: RAID_CREATE_MAX_MEMBERS,
        },
    })
}

/// Lower-case and validate an identity prefix: non-empty, at most a full
/// identity, all hexadecimal digits.
fn normalise_prefix(operand: &str) -> Result<String, ResolveError> {
    if operand.is_empty()
        || operand.len() > IDENTITY_HEX_DIGITS
        || !operand.bytes().all(|b| b.is_ascii_hexdigit())
    {
        return Err(ResolveError::BadArrayName(operand.to_string()));
    }
    Ok(operand.to_ascii_lowercase())
}

#[cfg(test)]
mod tests;
