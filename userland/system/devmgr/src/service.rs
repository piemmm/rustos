//! The device manager's reactive observe loop (`AGENTS.md` §18.4).
//!
//! The `Run` service reads the discovered hardware tree, reports every
//! node, then **blocks** until the tree changes and re-reads it — the
//! reactive discovery the §18.4 hotplug model requires. That control flow
//! is pure with respect to the kernel: it reads, waits, and reports through
//! the [`HwTreeService`] seam, so the loop's logic — read once, then
//! re-read on every generation advance — is exercised on the host against a
//! scripted seam, independently of the freestanding program and the live
//! `hw_tree_read` / `hw_tree_wait` syscalls it binds in production
//! (`AGENTS.md` §2.2). The wire decode it builds on is the same
//! fail-closed [`crate::observe::for_each_node`] walk.
//!
//! The loop never busy-spins (`AGENTS.md` §2.1): [`HwTreeService::wait_for_change`]
//! blocks until the store's generation advances. A failure in any seam
//! operation ends the loop fail-closed with the reported [`Errno`]
//! (`AGENTS.md` §2.9); the supervising PID 1 decides what to do next.

use rustos_abi::{Errno, HwNode, HwTreeHeader};

use crate::observe::for_each_node;

/// The kernel-facing operations the reactive observe loop performs,
/// abstracted so the loop is host-testable against a scripted double.
///
/// The production implementation (the freestanding `devmgr` `Run` binary)
/// backs these with the `hw_tree_read` / `hw_tree_wait` `abi-v1` syscalls
/// and writes node reports to its inherited diagnostic stream (fd 2,
/// `AGENTS.md` §20).
pub trait HwTreeService {
    /// Read the current hardware-tree snapshot into `buf`, returning the
    /// number of bytes written (a [`HwTreeHeader`] followed by its node
    /// records). Fails closed with the reported [`Errno`] — an undersized
    /// buffer is [`Errno::BufferTooSmall`], never a truncated read
    /// (`AGENTS.md` §2.9 / §24.1).
    fn read_tree(&mut self, buf: &mut [u8]) -> Result<usize, Errno>;

    /// Block until the store's generation advances past `last_generation`
    /// (`AGENTS.md` §18.4 — reactive re-match and hotplug). Returns once
    /// the tree has changed, or fails closed with the reported [`Errno`].
    fn wait_for_change(&mut self, last_generation: u64) -> Result<(), Errno>;

    /// Report the decoded snapshot header (its generation and node count)
    /// after a read.
    fn on_header(&mut self, header: &HwTreeHeader);

    /// Report one decoded node of the snapshot, in wire order.
    fn on_node(&mut self, node: &HwNode);
}

/// Read the current tree through `svc`, reporting its header and every
/// node, and return the generation the snapshot was taken at (the value to
/// pass to the next [`HwTreeService::wait_for_change`]).
///
/// # Errors
///
/// Propagates the [`Errno`] from [`HwTreeService::read_tree`] or from the
/// fail-closed [`for_each_node`] decode; on any error no header is reported
/// (`AGENTS.md` §2.9).
pub fn observe_once<S: HwTreeService>(svc: &mut S, buf: &mut [u8]) -> Result<u64, Errno> {
    let len = svc.read_tree(buf)?;
    // `for_each_node` borrows `buf` immutably and the closure borrows `svc`
    // mutably; the two borrows are disjoint, so the per-node report runs
    // while the snapshot is decoded without copying it out.
    let header = for_each_node(&buf[..len], |node| svc.on_node(node))?;
    svc.on_header(&header);
    Ok(header.generation())
}

/// Run the reactive observe loop: read and report the tree, then block on
/// every generation advance and re-read it (`AGENTS.md` §18.4).
///
/// `budget` bounds the number of *reactions* (re-reads after a change):
/// [`None`] runs for the life of the service (the production device
/// manager waits forever), while [`Some(n)`](Some) returns [`Ok`] after `n`
/// reactions — the bounded form the host tests drive so the loop
/// terminates. The initial read is always performed before the first wait.
///
/// # Errors
///
/// Returns the first [`Errno`] any seam operation reports; the loop is
/// fail-closed (`AGENTS.md` §2.9) and never silently continues past an
/// error.
pub fn run<S: HwTreeService>(
    svc: &mut S,
    buf: &mut [u8],
    budget: Option<u32>,
) -> Result<(), Errno> {
    let mut last_generation = observe_once(svc, buf)?;
    let mut reactions = 0u32;
    loop {
        if budget.is_some_and(|max| reactions >= max) {
            return Ok(());
        }
        svc.wait_for_change(last_generation)?;
        last_generation = observe_once(svc, buf)?;
        reactions += 1;
    }
}

#[cfg(test)]
mod tests {
    extern crate alloc;
    use alloc::vec::Vec;

    use super::*;
    use rustos_abi::hwtree::{HwDeviceClass, HW_NODE_ROOT};

    /// Encode `[HwTreeHeader][HwNode; n]` exactly as the kernel store does.
    fn encode(generation: u64, nodes: &[HwNode]) -> Vec<u8> {
        let mut blob = Vec::new();
        blob.extend_from_slice(&HwTreeHeader::new(generation, nodes.len() as u64).to_le_bytes());
        for node in nodes {
            blob.extend_from_slice(&node.to_le_bytes());
        }
        blob
    }

    /// A scripted seam: hands out a queued snapshot on each `read_tree`,
    /// records the generations it was asked to wait past and the nodes /
    /// headers it was asked to report, and fails closed once its script is
    /// exhausted (so a loop that reads more than scripted is caught).
    struct ScriptedService {
        snapshots: Vec<Vec<u8>>,
        next: usize,
        waited_on: Vec<u64>,
        reported_headers: Vec<(u64, u64)>,
        reported_nodes: Vec<u32>,
        wait_error: Option<Errno>,
    }

    impl ScriptedService {
        fn new(snapshots: Vec<Vec<u8>>) -> Self {
            Self {
                snapshots,
                next: 0,
                waited_on: Vec::new(),
                reported_headers: Vec::new(),
                reported_nodes: Vec::new(),
                wait_error: None,
            }
        }
    }

    impl HwTreeService for ScriptedService {
        fn read_tree(&mut self, buf: &mut [u8]) -> Result<usize, Errno> {
            let snapshot = self.snapshots.get(self.next).ok_or(Errno::NotFound)?;
            self.next += 1;
            if buf.len() < snapshot.len() {
                return Err(Errno::BufferTooSmall);
            }
            buf[..snapshot.len()].copy_from_slice(snapshot);
            Ok(snapshot.len())
        }

        fn wait_for_change(&mut self, last_generation: u64) -> Result<(), Errno> {
            if let Some(err) = self.wait_error {
                return Err(err);
            }
            self.waited_on.push(last_generation);
            Ok(())
        }

        fn on_header(&mut self, header: &HwTreeHeader) {
            self.reported_headers
                .push((header.generation(), header.node_count()));
        }

        fn on_node(&mut self, node: &HwNode) {
            self.reported_nodes.push(node.id());
        }
    }

    fn root_only(generation: u64) -> Vec<u8> {
        encode(
            generation,
            &[HwNode::new(1, HW_NODE_ROOT, HwDeviceClass::Root)],
        )
    }

    fn root_plus_bus(generation: u64) -> Vec<u8> {
        encode(
            generation,
            &[
                HwNode::new(1, HW_NODE_ROOT, HwDeviceClass::Root),
                HwNode::new(2, 1, HwDeviceClass::Bus),
            ],
        )
    }

    #[test]
    fn observe_once_reports_every_node_and_returns_the_generation() {
        let mut svc = ScriptedService::new(alloc::vec![root_plus_bus(7)]);
        let mut buf = [0u8; 4096];
        let generation = observe_once(&mut svc, &mut buf).expect("decodes");
        assert_eq!(generation, 7);
        assert_eq!(svc.reported_nodes, alloc::vec![1, 2]);
        assert_eq!(svc.reported_headers, alloc::vec![(7, 2)]);
    }

    #[test]
    fn run_reads_then_reacts_to_one_generation_bump() {
        // The bounded loop: read generation 1 (root only), block once, then
        // re-read generation 2 (a bus appeared) and stop after the single
        // scripted reaction (`AGENTS.md` §18.4).
        let mut svc = ScriptedService::new(alloc::vec![root_only(1), root_plus_bus(2)]);
        let mut buf = [0u8; 4096];
        run(&mut svc, &mut buf, Some(1)).expect("one reaction");

        // It waited exactly once, on the first read's generation.
        assert_eq!(svc.waited_on, alloc::vec![1]);
        // Two reads happened: the initial one and the post-change re-read.
        assert_eq!(svc.reported_headers, alloc::vec![(1, 1), (2, 2)]);
        // The re-read observed the appeared bus node.
        assert_eq!(svc.reported_nodes, alloc::vec![1, 1, 2]);
    }

    #[test]
    fn run_stops_immediately_with_a_zero_budget() {
        // A zero reaction budget performs the initial read and returns
        // without ever waiting.
        let mut svc = ScriptedService::new(alloc::vec![root_only(3)]);
        let mut buf = [0u8; 4096];
        run(&mut svc, &mut buf, Some(0)).expect("initial read only");
        assert!(svc.waited_on.is_empty(), "a zero budget never waits");
        assert_eq!(svc.reported_headers, alloc::vec![(3, 1)]);
    }

    #[test]
    fn run_fails_closed_when_the_initial_read_fails() {
        // An empty script makes `read_tree` fail closed; the loop never
        // waits and propagates the error (`AGENTS.md` §2.9).
        let mut svc = ScriptedService::new(Vec::new());
        let mut buf = [0u8; 4096];
        assert_eq!(run(&mut svc, &mut buf, None), Err(Errno::NotFound));
        assert!(svc.waited_on.is_empty());
    }

    #[test]
    fn run_fails_closed_when_the_wait_fails() {
        let mut svc = ScriptedService::new(alloc::vec![root_only(1)]);
        svc.wait_error = Some(Errno::NotImplemented);
        let mut buf = [0u8; 4096];
        assert_eq!(run(&mut svc, &mut buf, None), Err(Errno::NotImplemented));
        // The initial read still happened before the failed wait.
        assert_eq!(svc.reported_headers, alloc::vec![(1, 1)]);
    }

    #[test]
    fn observe_once_propagates_an_undersized_buffer() {
        let mut svc = ScriptedService::new(alloc::vec![root_plus_bus(1)]);
        let mut buf = [0u8; 8];
        assert_eq!(observe_once(&mut svc, &mut buf), Err(Errno::BufferTooSmall));
    }
}
