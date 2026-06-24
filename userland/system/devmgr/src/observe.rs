//! Decoding the wire-encoded hardware-tree snapshot the `hw_tree_read`
//! syscall returns.
//!
//! The `Run` service reads the discovered tree into a buffer and must walk
//! it node by node to observe (and, in a later tranche, match) each
//! device. That walk — decode the [`HwTreeHeader`], then decode
//! [`HwNode::node_count`](HwTreeHeader::node_count) records that follow —
//! is pure, fail-closed, and worth testing on the host independently of
//! the freestanding program, so it lives here in the library rather than
//! inside `src/run.rs`. Every length is validated
//! before use and a malformed snapshot is rejected whole, never partially
//! interpreted.

use rustos_abi::{Errno, HwNode, HwTreeHeader};

/// Decode the snapshot header and invoke `visit` once for each
/// [`HwNode`] that follows, in wire order.
///
/// `blob` is the exact byte buffer `hw_tree_read` filled: a
/// [`HwTreeHeader`] followed by [`HwTreeHeader::node_count`] records of
/// [`HwNode::WIRE_LEN`] bytes each. Returns the decoded header so the
/// caller learns the generation to pass to the subsequent `hw_tree_wait`.
///
/// # Errors
///
/// Fails closed without invoking `visit` again once an error is hit:
///
/// * [`Errno::BufferTooSmall`] if `blob` is shorter than the header, or
///   shorter than the header plus every record the header promises — a
///   truncated snapshot is never partially walked.
/// * Any error [`HwNode::from_bytes`] returns for a malformed record
///   (unknown device class, over-long count, …).
pub fn for_each_node(blob: &[u8], mut visit: impl FnMut(&HwNode)) -> Result<HwTreeHeader, Errno> {
    let header = HwTreeHeader::from_bytes(blob)?;

    // Validate the whole promised extent up front so a truncated tail is
    // rejected before any node is surfaced. `node_count`
    // is a `u64`; convert through `usize` and reject a count whose byte
    // span cannot be represented rather than wrapping.
    let count = usize::try_from(header.node_count()).map_err(|_| Errno::BufferTooSmall)?;
    let span = count
        .checked_mul(HwNode::WIRE_LEN)
        .and_then(|nodes| nodes.checked_add(HwTreeHeader::WIRE_LEN))
        .ok_or(Errno::BufferTooSmall)?;
    if blob.len() < span {
        return Err(Errno::BufferTooSmall);
    }

    let mut off = HwTreeHeader::WIRE_LEN;
    for _ in 0..count {
        let node = HwNode::from_bytes(&blob[off..off + HwNode::WIRE_LEN])?;
        visit(&node);
        off += HwNode::WIRE_LEN;
    }
    Ok(header)
}

#[cfg(test)]
mod tests {
    extern crate alloc;
    use alloc::vec::Vec;

    use super::*;
    use rustos_abi::hwtree::{HwDeviceClass, HwMatchKey, HW_NODE_ROOT};

    /// Encode `[HwTreeHeader][HwNode; n]` exactly as the kernel source does.
    fn encode(generation: u64, nodes: &[HwNode]) -> Vec<u8> {
        let mut blob = Vec::new();
        blob.extend_from_slice(&HwTreeHeader::new(generation, nodes.len() as u64).to_le_bytes());
        for node in nodes {
            blob.extend_from_slice(&node.to_le_bytes());
        }
        blob
    }

    fn sample_nodes() -> Vec<HwNode> {
        let mut hid = HwNode::new(3, 2, HwDeviceClass::Input);
        hid.push_match_key(HwMatchKey::usb(0x1234, 0x5678, 0x03_01_01))
            .expect("key fits");
        alloc::vec![
            HwNode::new(1, HW_NODE_ROOT, HwDeviceClass::Root),
            HwNode::new(2, 1, HwDeviceClass::Bus),
            hid,
        ]
    }

    #[test]
    fn walks_every_node_in_wire_order_and_returns_the_header() {
        let nodes = sample_nodes();
        let blob = encode(5, &nodes);

        let mut seen: Vec<u32> = Vec::new();
        let header = for_each_node(&blob, |node| seen.push(node.id())).expect("decodes");
        assert_eq!(header.generation(), 5);
        assert_eq!(header.node_count(), 3);
        assert_eq!(seen, alloc::vec![1, 2, 3]);
    }

    #[test]
    fn an_empty_tree_visits_nothing_but_reports_its_generation() {
        let blob = encode(9, &[]);
        let mut count = 0u32;
        let header = for_each_node(&blob, |_| count += 1).expect("decodes");
        assert_eq!(count, 0);
        assert_eq!(header.generation(), 9);
        assert_eq!(header.node_count(), 0);
    }

    #[test]
    fn a_truncated_tail_is_rejected_whole_without_visiting() {
        let nodes = sample_nodes();
        let mut blob = encode(1, &nodes);
        // Drop the last record's final byte: the header still promises 3
        // nodes but the buffer no longer holds them.
        blob.truncate(blob.len() - 1);

        let mut visited = 0u32;
        assert_eq!(
            for_each_node(&blob, |_| visited += 1),
            Err(Errno::BufferTooSmall)
        );
        assert_eq!(visited, 0, "a truncated snapshot is never partially walked");
    }

    #[test]
    fn a_header_shorter_than_its_own_size_is_buffer_too_small() {
        assert_eq!(for_each_node(&[0u8; 8], |_| {}), Err(Errno::BufferTooSmall));
    }
}
