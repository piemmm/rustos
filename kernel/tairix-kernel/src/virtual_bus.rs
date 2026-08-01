//! The synthetic **virtual bus**: the always-present parent of composed
//! block devices (`plans/FIX-IO.md` `IO6d`).
//!
//! Firmware describes physical devices, so a device that is *composed* out of
//! other devices — a RAID array assembled from its member disks — has nowhere
//! in the discovered tree to hang from. It cannot hang from a member: pulling
//! that one disk would orphan an array the survivors are still serving. It
//! cannot hang from the root either, because the process that publishes it
//! must itself be a driver matched to some node, and the root is not a device
//! any driver is loaded for.
//!
//! This module publishes that missing parent. It is one node, present on every
//! machine, directly beneath the root, whose lifetime is the machine's — the
//! equivalent of Linux's `virtual` bus. It describes no hardware, asserts
//! nothing about what is attached, and grants nothing: a driver matched to it
//! composes devices it is separately given authority over, and learns nothing
//! from the node's existence. So it is a structural feature of the tree like
//! the root itself, not a compiled-in device list standing in for discovery.
//!
//! It is published at the one arch-neutral seam where the discovered boot tree
//! becomes the live inventory ([`crate::unlock_service::record_boot`]), so a
//! new architecture port gets it without doing anything, and no port can
//! forget it.

use tairix_abi::{HwDeviceClass, HwMatchKey, HwNode, HW_NODE_ROOT_ID, HW_VIRTUAL_BUS_COMPATIBLE};

use crate::hwtree_node_ids::VIRTUAL_BUS_NODE_ID;

/// The bind key a driver matches the virtual bus by, validated at compile
/// time so the runtime path has no failure to handle: the compatible string is
/// a fixed 18 bytes and the inline limit is far wider, so a build in which
/// this did not fit could not be produced at all.
const VIRTUAL_BUS_KEY: HwMatchKey = match HwMatchKey::compatible(HW_VIRTUAL_BUS_COMPATIBLE) {
    Ok(key) => key,
    Err(_) => panic!("the virtual-bus compatible string fits HW_COMPATIBLE_MAX"),
};

/// The virtual-bus node: a bus directly beneath the root, carrying its
/// compatible string and no resources.
///
/// It declares no resource because it *has* none to declare. A driver matched
/// to it is born with no device authority at all, and reaches a composed
/// device's members only through authority those members' own drivers
/// delegate to it — so matching this node can never widen anyone's reach.
#[must_use]
pub fn node() -> HwNode {
    let mut node = HwNode::new(VIRTUAL_BUS_NODE_ID, HW_NODE_ROOT_ID, HwDeviceClass::Bus);
    // A node holds several match keys and this is the first, so the push has
    // no room to refuse; the key itself was accepted at compile time above.
    let _ = node.push_match_key(VIRTUAL_BUS_KEY);
    node
}

#[cfg(test)]
mod tests {
    use super::*;
    use tairix_abi::HwMatchKind;

    #[test]
    fn the_virtual_bus_is_a_root_child_bus_carrying_its_compatible_key() {
        // The whole node is load-bearing: a driver finds it by the compatible
        // string, the emitted array nodes are parented to it, and the
        // autoload walk skips anything parented to the root sentinel rather
        // than to the root's id.
        let node = node();
        assert_eq!(node.id(), VIRTUAL_BUS_NODE_ID);
        assert_eq!(node.parent(), HW_NODE_ROOT_ID);
        assert!(!node.is_root());
        assert_eq!(node.class(), Some(HwDeviceClass::Bus));

        let keys = node.match_keys();
        assert_eq!(keys.len(), 1, "exactly one way to match the virtual bus");
        assert_eq!(keys[0].kind(), Some(HwMatchKind::Compatible));
        assert_eq!(keys[0].compatible_bytes(), HW_VIRTUAL_BUS_COMPATIBLE);
    }

    #[test]
    fn the_virtual_bus_declares_no_resource() {
        // Matching it must not hand a driver any device authority: everything
        // a composer reaches is delegated to it later, by the processes that
        // legitimately hold it.
        assert!(node().resources().is_empty());
    }
}
