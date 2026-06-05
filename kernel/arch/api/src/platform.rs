//! Early-boot platform discovery — the Arch HAL slice that normalises a
//! target's native hardware source into the architecture-neutral hardware
//! tree (`AGENTS.md` §17.2 "early-boot platform discovery", §18.1/§18.2).
//!
//! Each architecture port reads its platform's enumerable source — ACPI on
//! x86_64, a flattened device tree on aarch64/riscv64, a host-capability
//! query on wasm32 — and emits one [`HwNode`] per
//! detected bus or device into a caller-supplied [`HwNodeSink`]. The
//! architecture-specific parsing never leaks past this trait: the rest of
//! the kernel and all of userland see only the normalised tree (§18.2,
//! enforced by `cargo xtask cfg-check`).
//!
//! # Why a sink rather than a returned collection
//!
//! `kernel/arch/api` is `no_std` and allocation-free (it never names a
//! `#[global_allocator]`). The discoverer therefore *pushes* nodes into a
//! sink the caller owns: the kernel boot path can collect into a
//! fixed-capacity on-stack buffer, while the user-space device manager can
//! collect into a growable one — without this trait choosing an allocator
//! for either (`AGENTS.md` §4 — deterministic, no hidden allocation). A
//! sink that is full fails closed with [`DiscoveryError::SinkFull`]
//! (`AGENTS.md` §2.9).

use rustos_abi::HwNode;

/// Why platform discovery, or a sink it wrote to, could not complete.
#[derive(Debug, Clone, Copy, Eq, PartialEq)]
pub enum DiscoveryError {
    /// The sink could not accept another node (its capacity is exhausted).
    /// The discoverer stops and surfaces this rather than dropping nodes
    /// silently (`AGENTS.md` §2.9 — fail closed).
    SinkFull,
    /// The platform's hardware source was missing or malformed (a bad FDT
    /// blob, an unreadable ACPI table). Discovery produced no usable tree.
    MalformedSource,
    /// This target exposes no enumerable hardware source, so a hardware
    /// tree cannot be built (an honest absence, not a silent empty tree —
    /// `plans/WIRING.md` §0.4).
    Unsupported,
}

/// A destination for the [`HwNode`]s a [`PlatformDiscovery`] emits.
///
/// Object-safe so a discoverer can write into `&mut dyn HwNodeSink`
/// without monomorphising over the collector. Implementors decide the
/// backing storage (a fixed array, a `Vec`); [`Self::emit`] returns
/// [`DiscoveryError::SinkFull`] when no more nodes fit.
pub trait HwNodeSink {
    /// Accept one node, or fail closed if the sink is full.
    ///
    /// # Errors
    ///
    /// [`DiscoveryError::SinkFull`] when the sink cannot store `node`.
    fn emit(&mut self, node: HwNode) -> Result<(), DiscoveryError>;
}

/// The Arch HAL early-boot platform-discovery surface (`AGENTS.md`
/// §17.2 / §18.2).
///
/// Every architecture port implements this trait, building the hardware
/// tree from its platform's native source. Exactly one node is a root
/// (its parent is [`rustos_abi::HW_NODE_ROOT`]); every other node names an
/// already-emitted parent by id, so a collector can reconstruct the tree
/// from the flat stream in emission order.
pub trait PlatformDiscovery {
    /// Enumerate the detected hardware into `sink`.
    ///
    /// Implementations emit the root node first, then its children, so a
    /// node's parent is always emitted before it. They must not panic on a
    /// malformed source — they return [`DiscoveryError::MalformedSource`]
    /// (`AGENTS.md` §2.9 / §18.4).
    ///
    /// # Errors
    ///
    /// * [`DiscoveryError::SinkFull`] — propagated from the sink.
    /// * [`DiscoveryError::MalformedSource`] — the platform source was
    ///   unreadable.
    /// * [`DiscoveryError::Unsupported`] — the target has no enumerable
    ///   source.
    fn discover(&self, sink: &mut dyn HwNodeSink) -> Result<(), DiscoveryError>;
}

/// The §17.2 / §18 conformance vertical for [`PlatformDiscovery`].
///
/// Like the sibling [`crate::sidechannel::conformance`] and
/// [`crate::memtag::conformance`] verticals, this is a generic, arch-
/// agnostic suite a port runs over its real discoverer so parity is
/// *enforced*, not asserted by inspection (`plans/WIRING.md` §0.3).
pub mod conformance {
    use super::{DiscoveryError, HwNodeSink, PlatformDiscovery};
    use rustos_abi::HwNode;

    /// Capacity of the id ledger the suite validates against.
    ///
    /// Generous for the shallow QEMU `virt` / PC trees the ports produce
    /// while keeping the suite allocation-free: it records only node ids
    /// (a `u32` each), never whole [`HwNode`]s, so its stack footprint is
    /// tiny (`AGENTS.md` §4 — no hidden allocation).
    const LEDGER_CAP: usize = 64;

    /// A validating sink: as each node arrives it checks the contract in
    /// emission order (decodable class, unique id, parent already seen) and
    /// records the id, so the suite never has to buffer whole nodes.
    struct ValidatingSink {
        seen_ids: [u32; LEDGER_CAP],
        len: usize,
        roots: usize,
    }

    impl ValidatingSink {
        fn new() -> Self {
            Self {
                seen_ids: [0; LEDGER_CAP],
                len: 0,
                roots: 0,
            }
        }
    }

    impl HwNodeSink for ValidatingSink {
        fn emit(&mut self, node: HwNode) -> Result<(), DiscoveryError> {
            if self.len >= LEDGER_CAP {
                return Err(DiscoveryError::SinkFull);
            }
            assert!(
                node.class().is_some(),
                "node {} carries an undecodable device class",
                node.id()
            );
            assert!(
                !self.seen_ids[..self.len].contains(&node.id()),
                "node id {} is emitted more than once",
                node.id()
            );
            if node.is_root() {
                self.roots += 1;
            } else {
                assert!(
                    self.seen_ids[..self.len].contains(&node.parent()),
                    "node {} names parent {} which was not emitted before it",
                    node.id(),
                    node.parent()
                );
            }
            self.seen_ids[self.len] = node.id();
            self.len += 1;
            Ok(())
        }
    }

    /// A sink that accepts nothing, used to prove [`DiscoveryError::SinkFull`]
    /// is honoured.
    struct FullSink;

    impl HwNodeSink for FullSink {
        fn emit(&mut self, _node: HwNode) -> Result<(), DiscoveryError> {
            Err(DiscoveryError::SinkFull)
        }
    }

    /// Run the [`PlatformDiscovery`] contract suite against `disco`.
    ///
    /// # Panics
    ///
    /// Panics (failing the test) if discovery errors on a healthy sink,
    /// emits no node, has no unique root, repeats a node id, names a parent
    /// that was not emitted before the child, carries an undecodable device
    /// class, or fails to surface a full sink.
    pub fn run<P: PlatformDiscovery + ?Sized>(disco: &P) {
        let mut sink = ValidatingSink::new();
        disco
            .discover(&mut sink)
            .expect("a conformant port discovers into a healthy sink without error");
        assert!(
            sink.len >= 1,
            "platform discovery must emit at least one node (a root)"
        );
        assert_eq!(
            sink.roots, 1,
            "the hardware tree must have exactly one root node"
        );

        // A full sink is surfaced, never silently ignored.
        assert_eq!(
            disco.discover(&mut FullSink),
            Err(DiscoveryError::SinkFull),
            "a port must propagate a full sink rather than dropping nodes"
        );
    }

    #[cfg(test)]
    mod tests {
        use super::super::{DiscoveryError, HwNodeSink, PlatformDiscovery};
        use super::run;
        use rustos_abi::{HwDeviceClass, HwNode, HW_NODE_ROOT};

        /// A faithful two-node discoverer: a root with one child.
        struct Double;

        impl PlatformDiscovery for Double {
            fn discover(&self, sink: &mut dyn HwNodeSink) -> Result<(), DiscoveryError> {
                sink.emit(HwNode::new(0, HW_NODE_ROOT, HwDeviceClass::Root))?;
                sink.emit(HwNode::new(1, 0, HwDeviceClass::Memory))?;
                Ok(())
            }
        }

        #[test]
        fn suite_accepts_an_honest_discoverer() {
            run(&Double);
            let dynamic: &dyn PlatformDiscovery = &Double;
            run(dynamic);
        }

        /// A discoverer that emits a child before its parent violates the
        /// emission-order contract.
        struct ChildBeforeParent;

        impl PlatformDiscovery for ChildBeforeParent {
            fn discover(&self, sink: &mut dyn HwNodeSink) -> Result<(), DiscoveryError> {
                sink.emit(HwNode::new(0, HW_NODE_ROOT, HwDeviceClass::Root))?;
                sink.emit(HwNode::new(1, 2, HwDeviceClass::Bus))?;
                Ok(())
            }
        }

        #[test]
        #[should_panic(expected = "was not emitted before it")]
        fn suite_rejects_a_forward_parent_reference() {
            run(&ChildBeforeParent);
        }

        /// A discoverer that reuses a node id violates the unique-id
        /// contract.
        struct DuplicateId;

        impl PlatformDiscovery for DuplicateId {
            fn discover(&self, sink: &mut dyn HwNodeSink) -> Result<(), DiscoveryError> {
                sink.emit(HwNode::new(0, HW_NODE_ROOT, HwDeviceClass::Root))?;
                sink.emit(HwNode::new(0, 0, HwDeviceClass::Bus))?;
                Ok(())
            }
        }

        #[test]
        #[should_panic(expected = "emitted more than once")]
        fn suite_rejects_duplicate_ids() {
            run(&DuplicateId);
        }
    }
}
