//! Autoload walk — match every hardware-tree node and load each winner
//! through the driver-host load gate.
//!
//! The device manager owns *policy* only: which driver binds which
//! node. The load *mechanism* — signature verification, capability
//! gating under `CAP_DRV_LOAD` / `CAP_DRV_KERNEL`, spawning — stays
//! behind the [`DriverLoader`] seam, implemented by the deployment's
//! driver host (`userland/system/drvhost`). The layering keeps
//! this crate on `lib/*` only, so the seam is how the gate is
//! reached without a userland→userland production edge.
//!
//! Every outcome is audited through [`rustos_log`] with a stable
//! [`crate::events`] identifier: a bound node, an unbound node (never
//! an error), a refused unbroken tie, and a failed
//! load are all visible to external audit consumers.

use alloc::vec::Vec;

use rustos_abi::hwtree::HwResource;
use rustos_abi::{DriverHandle, Errno, HwNode};
use rustos_caps::CapabilitySet;
use rustos_log::{log as log_event, Event, EventId, Field, Level, Sink};
use rustos_util::fmt::{format_hex_u64, format_i32};

use rustos_devmatch::{resolve, DriverCandidate, MatchResolution};

use crate::events;

/// Load mechanism behind the device manager.
///
/// The production implementation adapts the drvhost `Host::load`
/// pipeline: it verifies the image's signature, checks
/// `CAP_DRV_LOAD` (and `CAP_DRV_KERNEL` where declared) against
/// `caller_caps`, and spawns the driver — failing closed on the first
/// violated gate. The device manager never inspects
/// or bypasses those checks; it only consumes the outcome.
pub trait DriverLoader {
    /// Load the image at `path` on behalf of a caller holding
    /// `caller_caps`, granting the loaded driver exactly the device
    /// resources `resources` its matched hardware-tree node requested.
    ///
    /// `resources` is the matched node's [`HwNode::resources`] — the
    /// MMIO windows, IRQ lines, and DMA constraints expressed as
    /// capability-grant *requests*. The load
    /// mechanism mints the loaded driver an unforgeable, owner-checked
    /// grant per resource and nothing more, so the driver receives only
    /// the authority its node exposed (a loaded
    /// driver receives only the resource capabilities its matched node
    /// requested). The resources originate kernel-side, from the
    /// discovered hardware tree, never from an untrusted caller (no ambient authority); the device manager only forwards them.
    ///
    /// # Errors
    ///
    /// The gate's refusal mapped onto the `abi-v1` error surface
    /// (e.g. [`Errno::PermissionDenied`] for a missing capability).
    fn load(
        &mut self,
        path: &str,
        resources: &[HwResource],
        caller_caps: &CapabilitySet,
    ) -> Result<DriverHandle, Errno>;
}

/// One node successfully bound by [`DeviceManager::autoload`].
#[derive(Copy, Clone, Debug, Eq, PartialEq)]
pub struct NodeBinding {
    /// The bound hardware-tree node id.
    pub node: u32,
    /// Handle of the driver instance serving the node.
    pub handle: DriverHandle,
}

/// Outcome summary returned by [`DeviceManager::autoload`].
#[derive(Clone, Debug, Default, Eq, PartialEq)]
pub struct AutoloadReport {
    /// Every node bound, in tree order.
    pub bindings: Vec<NodeBinding>,
    /// Nodes whose match keys matched no candidate.
    pub unbound: u32,
    /// Nodes refused because two candidates tied at the highest
    /// priority (a packaging defect).
    pub ties_rejected: u32,
    /// Nodes whose winning driver was refused by the load gate.
    pub load_failures: u32,
}

/// User-space device manager (Stage 4.HW).
pub struct DeviceManager<'m> {
    sink: &'m dyn Sink,
}

impl<'m> DeviceManager<'m> {
    /// Construct a device manager auditing through `sink`.
    #[must_use]
    pub fn new(sink: &'m dyn Sink) -> Self {
        Self { sink }
    }

    /// Match every node of `tree` against `candidates` and load each
    /// winning driver through `loader` under `caller_caps`.
    ///
    /// Deterministic : the strictly
    /// highest-priority matching candidate binds; an unbroken tie
    /// between distinct candidates leaves the node unbound and is
    /// audited as a packaging defect. A node matching no candidate is
    /// left unbound and logged — never an error. A driver matched by
    /// several nodes is loaded once **per node**: each load spawns its
    /// own instance holding exactly that node's resource grants, so a
    /// shared instance would leave every node after the first granted
    /// to no one and silently dead (two identical virtio-input devices
    /// — a keyboard and a mouse — are the canonical case).
    /// A load refusal fails only that node, closed; the walk
    /// continues so one bad image cannot block the rest of the boot.
    pub fn autoload(
        &self,
        tree: &[HwNode],
        candidates: &[DriverCandidate<'_>],
        caller_caps: &CapabilitySet,
        loader: &mut dyn DriverLoader,
    ) -> AutoloadReport {
        let mut report = AutoloadReport::default();
        for node in tree {
            if node.is_root() {
                continue;
            }
            match resolve(node.match_keys(), candidates) {
                MatchResolution::Unmatched => {
                    // Routine, high-volume case: most nodes on a real device
                    // tree have no driver, so this is logged at `Debug` (not
                    // `Info`) to keep the slow diagnostic UART from drowning in
                    // one line per unbound node — still logged with its stable
                    // id when diagnostics are enabled. Kept identical to the user-space sibling
                    // (`crate::autoload::match_and_load`).
                    self.audit_node(events::NODE_UNBOUND, Level::Debug, node.id(), &[]);
                    report.unbound += 1;
                }
                MatchResolution::Tie { priority } => {
                    let mut pbuf = [0u8; 12];
                    let priority_str = format_i32(i32::from(priority), &mut pbuf);
                    self.audit_node(
                        events::NODE_TIE_REJECTED,
                        Level::Warn,
                        node.id(),
                        &[Field {
                            key: "priority",
                            value: rustos_log::FieldValue::Str(priority_str),
                        }],
                    );
                    report.ties_rejected += 1;
                }
                MatchResolution::Winner { candidate, .. } => {
                    let path = candidates[candidate].path;
                    // One instance per matched node: the loader spawns each
                    // load into its own process holding exactly this node's
                    // resource grants, so no cross-node cache is kept.
                    let handle = match loader.load(path, node.resources(), caller_caps) {
                        Ok(handle) => handle,
                        Err(errno) => {
                            let mut ebuf = [0u8; 12];
                            let errno_str = format_i32(errno.as_i32(), &mut ebuf);
                            self.audit_node(
                                events::NODE_LOAD_FAILED,
                                Level::Warn,
                                node.id(),
                                &[
                                    Field {
                                        key: "path",
                                        value: rustos_log::FieldValue::Str(path),
                                    },
                                    Field {
                                        key: "errno",
                                        value: rustos_log::FieldValue::Str(errno_str),
                                    },
                                ],
                            );
                            report.load_failures += 1;
                            continue;
                        }
                    };
                    let mut hbuf = [0u8; 16];
                    let handle_str = format_hex_u64(handle.as_u64(), &mut hbuf);
                    self.audit_node(
                        events::NODE_BOUND,
                        Level::Info,
                        node.id(),
                        &[
                            Field {
                                key: "path",
                                value: rustos_log::FieldValue::Str(path),
                            },
                            Field {
                                key: "handle",
                                value: rustos_log::FieldValue::Str(handle_str),
                            },
                        ],
                    );
                    report.bindings.push(NodeBinding {
                        node: node.id(),
                        handle,
                    });
                }
            }
        }
        report
    }

    fn audit_node(&self, id: EventId, level: Level, node: u32, extra: &[Field<'_>]) {
        let mut nbuf = [0u8; 16];
        let node_str = format_hex_u64(u64::from(node), &mut nbuf);
        // Stack-assemble the field slice: the node id first, then up to
        // two event-specific fields. Sized for the largest emitter.
        let mut fields = [Field {
            key: "node",
            value: rustos_log::FieldValue::Str(node_str),
        }; 3];
        let mut len = 1;
        for field in extra {
            fields[len] = *field;
            len += 1;
        }
        log_event(
            self.sink,
            &Event {
                level,
                id,
                message: event_message(id),
                fields: &fields[..len],
            },
        );
    }
}

fn event_message(id: EventId) -> &'static str {
    match id {
        x if x == events::NODE_BOUND => "node bound to driver",
        x if x == events::NODE_UNBOUND => "node left unbound: no matching driver",
        x if x == events::NODE_TIE_REJECTED => "node refused: unbroken bind-priority tie",
        x if x == events::NODE_LOAD_FAILED => "node load failed: driver-host gate refused",
        _ => "devmgr event",
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use alloc::string::{String, ToString};
    use alloc::vec::Vec;
    use core::cell::RefCell;
    use rustos_abi::{CapabilityId, DriverBindKey, HwDeviceClass, HwMatchKey, HW_NODE_ROOT};

    struct MockLoader {
        /// `Err` paths are refused with the given errno; everything
        /// else loads with the next sequential handle.
        refused: Vec<(String, Errno)>,
        calls: Vec<String>,
        /// Every load's `(path, resources)` pair, so a test can assert
        /// the matched node's requested resources reach the gate.
        resources_seen: Vec<(String, Vec<HwResource>)>,
        next: u64,
    }

    impl MockLoader {
        fn new() -> Self {
            Self {
                refused: Vec::new(),
                calls: Vec::new(),
                resources_seen: Vec::new(),
                next: 1,
            }
        }
    }

    impl DriverLoader for MockLoader {
        fn load(
            &mut self,
            path: &str,
            resources: &[HwResource],
            caller_caps: &CapabilitySet,
        ) -> Result<DriverHandle, Errno> {
            self.calls.push(path.to_string());
            self.resources_seen
                .push((path.to_string(), resources.to_vec()));
            if !caller_caps.contains(CapabilityId::DRV_LOAD) {
                return Err(Errno::PermissionDenied);
            }
            if let Some((_, errno)) = self.refused.iter().find(|(p, _)| p == path) {
                return Err(*errno);
            }
            let handle = DriverHandle::from_raw(self.next).expect("non-zero test handle");
            self.next += 1;
            Ok(handle)
        }
    }

    struct CapturedEvent {
        id: u32,
        fields: Vec<(String, String)>,
    }

    struct RecordingSink {
        events: RefCell<Vec<CapturedEvent>>,
    }

    impl RecordingSink {
        fn new() -> Self {
            Self {
                events: RefCell::new(Vec::new()),
            }
        }

        fn ids(&self) -> Vec<u32> {
            self.events.borrow().iter().map(|e| e.id).collect()
        }

        fn field_of(&self, id: u32, key: &str) -> Option<String> {
            self.events
                .borrow()
                .iter()
                .find(|e| e.id == id)
                .and_then(|e| e.fields.iter().find(|(k, _)| k == key))
                .map(|(_, v)| v.clone())
        }
    }

    impl Sink for RecordingSink {
        fn write_event(&self, event: &Event<'_>) {
            self.events.borrow_mut().push(CapturedEvent {
                id: event.id.0,
                fields: event
                    .fields
                    .iter()
                    .map(|f| (f.key.to_string(), f.value.to_string()))
                    .collect(),
            });
        }
    }

    fn compat(s: &[u8]) -> HwMatchKey {
        match HwMatchKey::compatible(s) {
            Ok(key) => key,
            Err(_) => unreachable!("test compatible strings fit HW_COMPATIBLE_MAX"),
        }
    }

    fn node(id: u32, class: HwDeviceClass, keys: &[HwMatchKey]) -> HwNode {
        let mut n = HwNode::new(id, 1, class);
        for key in keys {
            n.push_match_key(*key).expect("test node key count fits");
        }
        n
    }

    fn node_with_resources(
        id: u32,
        class: HwDeviceClass,
        keys: &[HwMatchKey],
        resources: &[HwResource],
    ) -> HwNode {
        let mut n = node(id, class, keys);
        for resource in resources {
            n.push_resource(*resource)
                .expect("test node resource count fits");
        }
        n
    }

    fn loader_caps() -> CapabilitySet {
        let mut set = CapabilitySet::empty();
        set.insert(CapabilityId::DRV_LOAD);
        set
    }

    #[test]
    fn winner_is_loaded_and_bound_and_unmatched_node_stays_unbound() {
        // `NODE_UNBOUND` is a `Debug` record (filtered out on a default `Info`
        // boot); lower the threshold so the test observes it.
        rustos_log::set_max_level(rustos_log::Level::Trace);
        let emmc = [DriverBindKey::new(5, compat(b"brcm,bcm2711-emmc2"))];
        let candidates = [DriverCandidate {
            path: "/System/Drivers/emmc2",
            bind_keys: &emmc,
        }];
        let tree = [
            HwNode::new(1, HW_NODE_ROOT, HwDeviceClass::Root),
            node(2, HwDeviceClass::Storage, &[compat(b"brcm,bcm2711-emmc2")]),
            node(3, HwDeviceClass::Serial, &[compat(b"arm,pl011")]),
        ];
        let sink = RecordingSink::new();
        let mut loader = MockLoader::new();
        let report =
            DeviceManager::new(&sink).autoload(&tree, &candidates, &loader_caps(), &mut loader);
        assert_eq!(report.bindings.len(), 1);
        assert_eq!(report.bindings[0].node, 2);
        assert_eq!(report.unbound, 1);
        assert_eq!(report.ties_rejected, 0);
        assert_eq!(report.load_failures, 0);
        assert_eq!(loader.calls, ["/System/Drivers/emmc2"]);
        let ids = sink.ids();
        assert!(ids.contains(&events::NODE_BOUND.0), "{ids:?}");
        assert!(ids.contains(&events::NODE_UNBOUND.0), "{ids:?}");
        assert_eq!(
            sink.field_of(events::NODE_BOUND.0, "path").as_deref(),
            Some("/System/Drivers/emmc2")
        );
    }

    #[test]
    fn matched_node_resources_are_forwarded_to_the_loader() {
        // a loaded driver receives only the resource capabilities
        // its matched node requested. The device manager must forward the
        // *matched* node's resources to the load mechanism verbatim.
        let keys = [compat(b"brcm,bcm2711-emmc2")];
        let candidate_keys = [DriverBindKey::new(5, compat(b"brcm,bcm2711-emmc2"))];
        let candidates = [DriverCandidate {
            path: "/System/Drivers/emmc2",
            bind_keys: &candidate_keys,
        }];
        let window = HwResource::mmio(0xfe34_0000, 0x100);
        let irq = HwResource::irq(0x7e, 1);
        let tree = [
            HwNode::new(1, HW_NODE_ROOT, HwDeviceClass::Root),
            node_with_resources(2, HwDeviceClass::Storage, &keys, &[window, irq]),
        ];
        let sink = RecordingSink::new();
        let mut loader = MockLoader::new();
        let report =
            DeviceManager::new(&sink).autoload(&tree, &candidates, &loader_caps(), &mut loader);
        assert_eq!(report.bindings.len(), 1);
        assert_eq!(
            loader.resources_seen,
            [(
                "/System/Drivers/emmc2".to_string(),
                alloc::vec![window, irq]
            )],
            "the matched node's exact resource requests reach the loader"
        );
    }

    #[test]
    fn root_node_is_skipped_silently() {
        let tree = [HwNode::new(1, HW_NODE_ROOT, HwDeviceClass::Root)];
        let sink = RecordingSink::new();
        let mut loader = MockLoader::new();
        let report = DeviceManager::new(&sink).autoload(&tree, &[], &loader_caps(), &mut loader);
        assert_eq!(report, AutoloadReport::default());
        assert!(sink.ids().is_empty());
        assert!(loader.calls.is_empty());
    }

    #[test]
    fn unbroken_tie_is_refused_without_loading() {
        let a = [DriverBindKey::new(4, HwMatchKey::virtio(16))];
        let b = [DriverBindKey::new(4, HwMatchKey::virtio(16))];
        let candidates = [
            DriverCandidate {
                path: "/d/gpu-a",
                bind_keys: &a,
            },
            DriverCandidate {
                path: "/d/gpu-b",
                bind_keys: &b,
            },
        ];
        let tree = [node(2, HwDeviceClass::Display, &[HwMatchKey::virtio(16)])];
        let sink = RecordingSink::new();
        let mut loader = MockLoader::new();
        let report =
            DeviceManager::new(&sink).autoload(&tree, &candidates, &loader_caps(), &mut loader);
        assert!(report.bindings.is_empty());
        assert_eq!(report.ties_rejected, 1);
        assert!(loader.calls.is_empty(), "tie must never reach the loader");
        assert_eq!(sink.ids(), [events::NODE_TIE_REJECTED.0]);
        assert_eq!(
            sink.field_of(events::NODE_TIE_REJECTED.0, "priority")
                .as_deref(),
            Some("4")
        );
    }

    #[test]
    fn capability_denied_load_fails_closed_and_walk_continues() {
        let blk = [DriverBindKey::new(1, HwMatchKey::virtio(2))];
        let net = [DriverBindKey::new(1, HwMatchKey::virtio(1))];
        let candidates = [
            DriverCandidate {
                path: "/d/blk",
                bind_keys: &blk,
            },
            DriverCandidate {
                path: "/d/net",
                bind_keys: &net,
            },
        ];
        let tree = [
            node(2, HwDeviceClass::Storage, &[HwMatchKey::virtio(2)]),
            node(3, HwDeviceClass::Network, &[HwMatchKey::virtio(1)]),
        ];
        let sink = RecordingSink::new();
        // Caller without CAP_DRV_LOAD: every load is denied by the gate.
        let mut loader = MockLoader::new();
        let report = DeviceManager::new(&sink).autoload(
            &tree,
            &candidates,
            &CapabilitySet::empty(),
            &mut loader,
        );
        assert!(report.bindings.is_empty());
        assert_eq!(report.load_failures, 2);
        assert_eq!(loader.calls.len(), 2, "each node's load is attempted");
        assert_eq!(
            sink.ids(),
            [events::NODE_LOAD_FAILED.0, events::NODE_LOAD_FAILED.0]
        );
        assert_eq!(
            sink.field_of(events::NODE_LOAD_FAILED.0, "errno")
                .as_deref(),
            Some("6"),
            "PermissionDenied surfaces in the audit record"
        );
    }

    #[test]
    fn refused_image_fails_only_its_node() {
        let blk = [DriverBindKey::new(1, HwMatchKey::virtio(2))];
        let net = [DriverBindKey::new(1, HwMatchKey::virtio(1))];
        let candidates = [
            DriverCandidate {
                path: "/d/blk",
                bind_keys: &blk,
            },
            DriverCandidate {
                path: "/d/net",
                bind_keys: &net,
            },
        ];
        let tree = [
            node(2, HwDeviceClass::Storage, &[HwMatchKey::virtio(2)]),
            node(3, HwDeviceClass::Network, &[HwMatchKey::virtio(1)]),
        ];
        let sink = RecordingSink::new();
        let mut loader = MockLoader::new();
        loader
            .refused
            .push(("/d/blk".to_string(), Errno::SignatureInvalid));
        let report =
            DeviceManager::new(&sink).autoload(&tree, &candidates, &loader_caps(), &mut loader);
        assert_eq!(report.load_failures, 1);
        assert_eq!(report.bindings.len(), 1);
        assert_eq!(report.bindings[0].node, 3, "the healthy node still binds");
    }

    #[test]
    fn a_driver_matched_by_two_nodes_loads_one_instance_per_node() {
        // The regression for the QEMU virtio keyboard+mouse pair: two nodes
        // matching the same driver each need their own loaded instance — the
        // loader grants each instance exactly its node's resources, so a
        // shared load would leave the second device granted to no one and
        // silently dead.
        let uart = [DriverBindKey::new(2, compat(b"arm,pl011"))];
        let candidates = [DriverCandidate {
            path: "/d/uart",
            bind_keys: &uart,
        }];
        let window_a = HwResource::mmio(0x0900_0000, 0x1000);
        let window_b = HwResource::mmio(0x0900_1000, 0x1000);
        let tree = [
            node_with_resources(
                2,
                HwDeviceClass::Serial,
                &[compat(b"arm,pl011")],
                &[window_a],
            ),
            node_with_resources(
                3,
                HwDeviceClass::Serial,
                &[compat(b"arm,pl011")],
                &[window_b],
            ),
        ];
        let sink = RecordingSink::new();
        let mut loader = MockLoader::new();
        let report =
            DeviceManager::new(&sink).autoload(&tree, &candidates, &loader_caps(), &mut loader);
        assert_eq!(
            loader.calls,
            ["/d/uart", "/d/uart"],
            "each matched node loads its own instance"
        );
        assert_eq!(
            loader.resources_seen,
            [
                ("/d/uart".to_string(), alloc::vec![window_a]),
                ("/d/uart".to_string(), alloc::vec![window_b]),
            ],
            "each instance is granted exactly its own node's resources"
        );
        assert_eq!(report.bindings.len(), 2);
        assert_ne!(
            report.bindings[0].handle, report.bindings[1].handle,
            "each node binds its own instance"
        );
        let ids = sink.ids();
        assert_eq!(
            ids.iter().filter(|&&id| id == events::NODE_BOUND.0).count(),
            2
        );
    }
}
