//! Resolution tests: `node:<id>` device names, full and partial array
//! identities, and the ambiguous / unknown / malformed refusals.

extern crate std;

use alloc::string::{String, ToString};
use alloc::vec;
use alloc::vec::Vec;

use tairix_abi::raid::{ArrayHealth, RaidLevel};
use tairix_abi::raid_admin::RaidArrayRecord;

use super::{resolve_array, resolve_device, resolve_members};
use crate::error::ResolveError;

/// A minimal array record carrying only the identity resolution needs.
fn array(id: [u8; 16]) -> RaidArrayRecord {
    RaidArrayRecord::new(
        id,
        RaidLevel::Mirror,
        ArrayHealth::Optimal,
        0,
        2,
        2,
        512,
        0,
        1024,
        1,
        1,
        1024,
        1024,
        1,
    )
}

fn id_from(prefix: &[u8]) -> [u8; 16] {
    let mut out = [0u8; 16];
    out[..prefix.len()].copy_from_slice(prefix);
    out
}

fn names(names: &[&str]) -> Vec<String> {
    names.iter().map(ToString::to_string).collect()
}

#[test]
fn a_device_is_named_node_colon_id() {
    assert_eq!(resolve_device("node:42"), Ok(42));
    assert_eq!(resolve_device("node:1"), Ok(1));
}

#[test]
fn a_non_node_device_name_is_refused() {
    for bad in [
        "42",
        "/dev/sda",
        "node:",
        "node:0",
        "node:-1",
        "node:0x10",
        "nose:1",
    ] {
        assert_eq!(
            resolve_device(bad),
            Err(ResolveError::BadDeviceName(String::from(bad))),
            "{bad}"
        );
    }
}

#[test]
fn a_full_identity_resolves() {
    let one = id_from(&[0x3f, 0x2a]);
    let arrays = vec![array(one)];
    let full = "3f2a0000000000000000000000000000";
    assert_eq!(resolve_array(&arrays, full), Ok(one));
}

#[test]
fn an_unambiguous_prefix_resolves() {
    let one = id_from(&[0x3f, 0x2a]);
    let two = id_from(&[0xb1, 0x77]);
    let arrays = vec![array(one), array(two)];
    assert_eq!(resolve_array(&arrays, "3f"), Ok(one));
    assert_eq!(resolve_array(&arrays, "b1"), Ok(two));
    // Case-insensitive on the input.
    assert_eq!(resolve_array(&arrays, "B1"), Ok(two));
}

#[test]
fn an_ambiguous_prefix_is_refused_not_guessed() {
    let one = id_from(&[0x3f, 0x2a]);
    let two = id_from(&[0x3f, 0x77]);
    let arrays = vec![array(one), array(two)];
    assert_eq!(
        resolve_array(&arrays, "3f"),
        Err(ResolveError::AmbiguousArray(String::from("3f")))
    );
}

#[test]
fn an_unknown_identity_is_refused() {
    let arrays = vec![array(id_from(&[0x3f, 0x2a]))];
    assert_eq!(
        resolve_array(&arrays, "dead"),
        Err(ResolveError::ArrayNotFound(String::from("dead")))
    );
}

#[test]
fn a_malformed_array_name_is_refused() {
    let arrays = vec![array(id_from(&[0x3f, 0x2a]))];
    for bad in ["", "xyz", "3f2g", "0123456789abcdef0123456789abcdef0"] {
        assert_eq!(
            resolve_array(&arrays, bad),
            Err(ResolveError::BadArrayName(String::from(bad))),
            "{bad}"
        );
    }
}

#[test]
fn create_members_resolve_in_order() {
    let members = resolve_members(&names(&["node:1", "node:2", "node:3"]))
        .expect("valid member set resolves");
    assert_eq!(members.as_slice(), &[1, 2, 3]);
}

#[test]
fn a_duplicate_device_is_refused() {
    assert_eq!(
        resolve_members(&names(&["node:1", "node:2", "node:1"])),
        Err(ResolveError::DuplicateDevice(String::from("node:1")))
    );
}

#[test]
fn a_bad_device_in_a_create_is_refused() {
    assert_eq!(
        resolve_members(&names(&["node:1", "sda"])),
        Err(ResolveError::BadDeviceName(String::from("sda")))
    );
}
