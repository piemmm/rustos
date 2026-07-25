//! Handing a discovered NIC device channel to the network stack.
//!
//! A NIC driver process, once the device manager has autoloaded it for a
//! matched network node, brings its device online and publishes a child
//! *device-channel* hardware-tree node: `compatible = "tairix,netchan"`,
//! carrying the reserved call-endpoint id it bound as an
//! [`HwResourceKind::Endpoint`] grant request. Emitting that node bumps the
//! hardware-tree generation, waking the device manager's reactive loop.
//!
//! This module is the pure policy for that reaction: recognise a `netchan`
//! node, read its endpoint, and — for each channel not already handed over —
//! ask the network stack to bind it (over the [`NetstackBind`] seam, so the
//! loop stays host-testable). The stack becomes the channel's client, sizes
//! the shared frame region, attaches, and manages the interface; the device
//! manager only names *which* endpoint and *what to call* the interface.
//!
//! Each endpoint is handed over exactly once (tracked in [`NetBindState`]):
//! the node persists across every later generation bump while the driver
//! lives, so a re-bind would provision a second, duplicate interface. A
//! hand-off that fails (the stack is not up yet, or refuses) is fail-soft —
//! logged and retried on the next bump, exactly like an unavailable driver
//! store — never fatal to the observe loop.

use alloc::collections::BTreeSet;

use tairix_abi::hwtree::{HwMatchKind, HwResourceKind};
use tairix_abi::net_ipc::{
    validate_if_name, NetBondConfigMsg, NetInterfaceConfigMsg, NetworkSettings, IF_NAME_LEN,
};
use tairix_abi::{Errno, HwNode};
use tairix_log::{log as log_event, Event, EventId, Field, FieldValue, Level, Sink};

use crate::events;

/// The `compatible` match key a NIC driver process stamps on the `netchan`
/// device-channel node it emits, so the device manager recognises it as a
/// bound NIC's frame channel rather than a device awaiting a driver.
pub const NETCHAN_COMPATIBLE: &[u8] = b"tairix,netchan";

/// The device manager's call into the network stack to bind one NIC
/// driver's device channel to a new managed interface.
///
/// The production implementation (the freestanding `devmgr` `Run` binary)
/// backs this with an `ipc_call` to the reserved
/// [`NETSTACK_ENDPOINT`](tairix_abi::net_ipc::NETSTACK_ENDPOINT) carrying a
/// [`NetstackRequest::BindDriver`](tairix_abi::net_ipc::NetstackRequest::BindDriver);
/// the kernel gates the call on the device manager's `CAP_NET_ADMIN`, so the
/// seam adds no authority. It is abstracted here so the reactive loop is
/// host-testable against a recording double.
pub trait NetstackBind {
    /// Ask the network stack to bind the NIC driver's device-channel
    /// `endpoint_id` as the managed interface `iface`, recording the NIC's
    /// stable hardware location `node_location` (the register-window base of
    /// the device manager's matched hardware-tree node, or `0` when none was
    /// resolvable) so a `network.conf` `<iface>.match.node` binding can name
    /// this physical device by where it sits on the bus.
    ///
    /// # Errors
    ///
    /// The stack's typed refusal, or a transport failure — treated
    /// fail-soft by the caller (retried on the next generation bump).
    fn bind_driver(
        &mut self,
        endpoint_id: u64,
        iface: &[u8; IF_NAME_LEN],
        node_location: u64,
    ) -> Result<(), Errno>;

    /// Deliver the stack-wide `net.*` policy to the network stack
    /// (`plans/NETWORK.md` N9b-2).
    ///
    /// The production implementation backs this with an `ipc_call` to the
    /// [`NETSTACK_ENDPOINT`](tairix_abi::net_ipc::NETSTACK_ENDPOINT)
    /// carrying a
    /// [`NetstackRequest::ApplyNetworkSettings`](tairix_abi::net_ipc::NetstackRequest::ApplyNetworkSettings);
    /// the kernel gates it on `CAP_NET_ADMIN`, so the seam adds no
    /// authority.
    ///
    /// # Errors
    ///
    /// The stack's typed refusal, or a transport failure — treated
    /// fail-soft by the caller (retried on the next generation bump).
    fn apply_settings(&mut self, settings: NetworkSettings) -> Result<(), Errno>;

    /// Deliver one managed interface's declarative configuration to the
    /// network stack (`plans/NETWORK.md` N9b-3-1).
    ///
    /// The production implementation backs this with an `ipc_call` to the
    /// [`NETSTACK_ENDPOINT`](tairix_abi::net_ipc::NETSTACK_ENDPOINT)
    /// carrying the framed [`NetInterfaceConfigMsg`]; the kernel gates it on
    /// `CAP_NET_ADMIN`, so the seam adds no authority.
    ///
    /// # Errors
    ///
    /// The stack's typed refusal ([`Errno::NotFound`] when the interface is
    /// not yet bound — the caller retries silently) or a transport failure.
    fn apply_interface_config(&mut self, config: &NetInterfaceConfigMsg) -> Result<(), Errno>;

    /// Compose (or reconfigure) a bond interface in the network stack
    /// (`plans/NETWORK.md` §6.3).
    ///
    /// The production implementation backs this with an `ipc_call` to the
    /// [`NETSTACK_ENDPOINT`](tairix_abi::net_ipc::NETSTACK_ENDPOINT)
    /// carrying the framed [`NetBondConfigMsg`]; the kernel gates it on
    /// `CAP_NET_ADMIN`, so the seam adds no authority.
    ///
    /// # Errors
    ///
    /// The stack's typed refusal ([`Errno::NotFound`] when a declared
    /// member is not yet bound — the caller retries silently) or a
    /// transport failure.
    fn apply_bond_config(&mut self, config: &NetBondConfigMsg) -> Result<(), Errno>;
}

/// The device manager's memory of which NIC channels it has already handed
/// to the network stack, plus the next interface index to name.
///
/// A `netchan` node persists across every generation bump for as long as its
/// driver lives, so binding is idempotent: an endpoint already handed over
/// is skipped, and only a *successful* hand-off consumes an interface index
/// (a refused one is retried under the same name next time).
#[derive(Default)]
pub struct NetBindState {
    bound: BTreeSet<u64>,
    next_index: u32,
}

impl NetBindState {
    /// A fresh state with nothing bound.
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// Whether the channel `endpoint_id` has already been handed over.
    #[must_use]
    pub fn is_bound(&self, endpoint_id: u64) -> bool {
        self.bound.contains(&endpoint_id)
    }
}

/// If `node` is a NIC device-channel node — its match keys carry the
/// [`NETCHAN_COMPATIBLE`] `compatible` string — return the call-endpoint id
/// it published as an [`HwResourceKind::Endpoint`] grant request.
///
/// Returns [`None`] for any other node, and for a `netchan` node that
/// carries no endpoint resource (a malformed emission — never guessed at).
#[must_use]
pub fn netchan_endpoint(node: &HwNode) -> Option<u64> {
    let is_netchan = node.match_keys().iter().any(|key| {
        key.kind() == Some(HwMatchKind::Compatible) && key.compatible_bytes() == NETCHAN_COMPATIBLE
    });
    if !is_netchan {
        return None;
    }
    node.resources()
        .iter()
        .find(|resource| resource.kind() == Some(HwResourceKind::Endpoint))
        .map(tairix_abi::HwResource::base)
}

/// The stable hardware location of the NIC that emitted the `netchan`
/// `node`: the lowest register-window base of its **parent** device node in
/// `nodes` (the driver's matched node — a `netchan` is emitted as a child of
/// it), or `0` when the parent is absent or exposes no register window.
///
/// The register-window base is the device's fixed position on the bus (an
/// MMIO aperture base, a PCI BAR base), so it is a discovery-order-
/// independent identity a `network.conf` `<iface>.match.node` key can name.
/// The *lowest* window is chosen so a multi-window PCI function yields one
/// deterministic value regardless of the order its windows were emitted.
#[must_use]
fn netchan_node_location(netchan: &HwNode, nodes: &[HwNode]) -> u64 {
    let parent = netchan.parent();
    nodes
        .iter()
        .find(|candidate| candidate.id() == parent)
        .and_then(|device| {
            device
                .resources()
                .iter()
                .filter_map(tairix_abi::HwResource::register_window_base)
                .min()
        })
        .unwrap_or(0)
}

/// Hand every not-yet-bound NIC device channel in `nodes` to the network
/// stack through `netstack`, recording each success in `state`.
///
/// An endpoint already in `state` is skipped (idempotent across generation
/// bumps). A hand-off that the stack refuses is fail-soft: logged and left
/// for the next bump to retry (the stack may not have bound its endpoint
/// yet), never fatal to the observe loop.
pub fn bind_new_channels(
    nodes: &[HwNode],
    state: &mut NetBindState,
    netstack: &mut dyn NetstackBind,
    sink: &dyn Sink,
) {
    for node in nodes {
        let Some(endpoint) = netchan_endpoint(node) else {
            continue;
        };
        if state.bound.contains(&endpoint) {
            continue;
        }
        let iface = iface_name(state.next_index);
        // The derived name is always valid; refuse to bind one that is not
        // rather than hand the stack a name it would reject (fail closed).
        if validate_if_name(&iface).is_err() {
            continue;
        }
        // The NIC's stable hardware location: the register-window base of
        // the *parent* device node this `netchan` was emitted under (the
        // driver's matched node). The stack records it so a `match.node`
        // configuration can bind an admin alias to this physical device.
        let node_location = netchan_node_location(node, nodes);
        match netstack.bind_driver(endpoint, &iface, node_location) {
            Ok(()) => {
                state.bound.insert(endpoint);
                state.next_index = state.next_index.saturating_add(1);
                audit(
                    sink,
                    events::NETSTACK_BOUND,
                    Level::Info,
                    "netchan device channel bound to network stack",
                    &iface,
                );
            }
            Err(_) => {
                audit(
                    sink,
                    events::NETSTACK_BIND_FAILED,
                    Level::Warn,
                    "netchan device-channel bind to network stack failed; will retry",
                    &iface,
                );
            }
        }
    }
}

/// The interface alias for channel index `n` (`net0`, `net1`, …):
/// lowercase ASCII, NUL-padded, valid per [`validate_if_name`].
fn iface_name(n: u32) -> [u8; IF_NAME_LEN] {
    let mut name = [0u8; IF_NAME_LEN];
    name[..3].copy_from_slice(b"net");
    // Decimal digits of `n`, most-significant first, appended after "net".
    // Written into a small scratch least-significant first, then reversed.
    let mut digits = [0u8; 10];
    let mut count = 0usize;
    let mut value = n;
    loop {
        // `value % 10` is 0..=9, so the byte is a valid ASCII digit.
        digits[count] = b'0' + u8::try_from(value % 10).unwrap_or(0);
        count += 1;
        value /= 10;
        if value == 0 || count == digits.len() {
            break;
        }
    }
    let mut pos = 3;
    for i in (0..count).rev() {
        if pos < IF_NAME_LEN {
            name[pos] = digits[i];
            pos += 1;
        }
    }
    name
}

/// Emit one audit record carrying the interface alias.
fn audit(
    sink: &dyn Sink,
    id: EventId,
    level: Level,
    message: &'static str,
    iface: &[u8; IF_NAME_LEN],
) {
    let len = iface.iter().position(|&b| b == 0).unwrap_or(IF_NAME_LEN);
    let name = core::str::from_utf8(&iface[..len]).unwrap_or("?");
    log_event(
        sink,
        &Event {
            level,
            id,
            message,
            fields: &[Field {
                key: "iface",
                value: FieldValue::Str(name),
            }],
        },
    );
}

#[cfg(test)]
mod tests {
    extern crate alloc;
    use alloc::vec::Vec;
    use core::cell::RefCell;

    use super::*;
    use tairix_abi::hwtree::{HwDeviceClass, HwMatchKey, HwResource, HW_NODE_ROOT};

    /// A recording [`NetstackBind`] double: captures each bind call and
    /// answers each with a scripted result.
    struct RecordingBind {
        calls: RefCell<Vec<(u64, [u8; IF_NAME_LEN], u64)>>,
        results: RefCell<Vec<Result<(), Errno>>>,
    }

    impl RecordingBind {
        fn new(results: Vec<Result<(), Errno>>) -> Self {
            Self {
                calls: RefCell::new(Vec::new()),
                results: RefCell::new(results),
            }
        }
    }

    impl NetstackBind for RecordingBind {
        fn bind_driver(
            &mut self,
            endpoint_id: u64,
            iface: &[u8; IF_NAME_LEN],
            node_location: u64,
        ) -> Result<(), Errno> {
            self.calls
                .borrow_mut()
                .push((endpoint_id, *iface, node_location));
            self.results.borrow_mut().pop().unwrap_or(Ok(()))
        }

        fn apply_settings(&mut self, _settings: NetworkSettings) -> Result<(), Errno> {
            Ok(())
        }

        fn apply_interface_config(&mut self, _config: &NetInterfaceConfigMsg) -> Result<(), Errno> {
            Ok(())
        }

        fn apply_bond_config(&mut self, _config: &NetBondConfigMsg) -> Result<(), Errno> {
            Ok(())
        }
    }

    struct NullSink;
    impl Sink for NullSink {
        fn write_event(&self, _event: &Event<'_>) {}
    }

    fn netchan_node(id: u32, endpoint: u64) -> HwNode {
        let mut node = HwNode::new(id, HW_NODE_ROOT, HwDeviceClass::Network);
        node.push_match_key(HwMatchKey::compatible(NETCHAN_COMPATIBLE).expect("key"))
            .expect("push key");
        node.push_resource(HwResource::endpoint(endpoint))
            .expect("push resource");
        node
    }

    fn name(text: &str) -> [u8; IF_NAME_LEN] {
        let mut out = [0u8; IF_NAME_LEN];
        out[..text.len()].copy_from_slice(text.as_bytes());
        out
    }

    #[test]
    fn iface_names_count_up_and_are_valid() {
        assert_eq!(iface_name(0), name("net0"));
        assert_eq!(iface_name(9), name("net9"));
        assert_eq!(iface_name(15), name("net15"));
        for n in 0..16 {
            validate_if_name(&iface_name(n)).expect("derived name is valid");
        }
    }

    #[test]
    fn a_netchan_node_yields_its_endpoint_others_yield_none() {
        assert_eq!(
            netchan_endpoint(&netchan_node(2, 0x4E43_4841_4E00)),
            Some(0x4E43_4841_4E00)
        );
        // A non-netchan node (a plain network device awaiting its driver).
        let mut plain = HwNode::new(3, HW_NODE_ROOT, HwDeviceClass::Network);
        plain
            .push_match_key(HwMatchKey::compatible(b"virtio,mmio").expect("key"))
            .expect("push");
        assert_eq!(netchan_endpoint(&plain), None);
        // A netchan node with no endpoint resource is malformed → None.
        let mut no_ep = HwNode::new(4, HW_NODE_ROOT, HwDeviceClass::Network);
        no_ep
            .push_match_key(HwMatchKey::compatible(NETCHAN_COMPATIBLE).expect("key"))
            .expect("push");
        assert_eq!(netchan_endpoint(&no_ep), None);
    }

    #[test]
    fn each_channel_is_bound_once_and_named_in_order() {
        let nodes = alloc::vec![netchan_node(2, 100), netchan_node(3, 200)];
        let mut state = NetBindState::new();
        let mut bind = RecordingBind::new(alloc::vec![Ok(()), Ok(())]);
        bind_new_channels(&nodes, &mut state, &mut bind, &NullSink);
        // First pass binds both, names net0/net1. No parent device node is
        // present, so each reports no hardware location (`0`).
        assert_eq!(
            *bind.calls.borrow(),
            alloc::vec![(100, name("net0"), 0), (200, name("net1"), 0)]
        );
        // A second pass over the same (persistent) nodes binds nothing.
        bind_new_channels(&nodes, &mut state, &mut bind, &NullSink);
        assert_eq!(bind.calls.borrow().len(), 2, "no channel re-bound");
        assert!(state.is_bound(100) && state.is_bound(200));
    }

    #[test]
    fn a_refused_bind_is_retried_and_does_not_consume_a_name() {
        let nodes = alloc::vec![netchan_node(2, 100)];
        let mut state = NetBindState::new();
        // First call refused, second (retry) accepted.
        let mut bind = RecordingBind::new(alloc::vec![Ok(()), Err(Errno::NotConnected)]);
        bind_new_channels(&nodes, &mut state, &mut bind, &NullSink);
        assert!(!state.is_bound(100), "a refused bind is not recorded");
        bind_new_channels(&nodes, &mut state, &mut bind, &NullSink);
        assert!(state.is_bound(100), "retried and bound");
        // Both attempts used net0 — a refusal never consumed the index.
        assert_eq!(
            *bind.calls.borrow(),
            alloc::vec![(100, name("net0"), 0), (100, name("net0"), 0)]
        );
    }

    #[test]
    fn a_netchan_reports_its_parent_device_register_base_as_the_location() {
        // A discovered NIC device node with an MMIO register window at
        // 0x0a00_0000, and the `netchan` its driver emitted as a child.
        let mut device = HwNode::new(7, HW_NODE_ROOT, HwDeviceClass::Network);
        device
            .push_match_key(HwMatchKey::virtio(1))
            .expect("push key");
        device
            .push_resource(HwResource::mmio(0x0a00_0000, 0x200))
            .expect("push mmio");
        let mut channel = netchan_node(8, 100);
        channel.set_identity(8, 7);
        let nodes = alloc::vec![device, channel];
        let mut state = NetBindState::new();
        let mut bind = RecordingBind::new(alloc::vec![Ok(())]);
        bind_new_channels(&nodes, &mut state, &mut bind, &NullSink);
        // The interface is bound carrying the parent's register-window base
        // as its stable hardware location.
        assert_eq!(
            *bind.calls.borrow(),
            alloc::vec![(100, name("net0"), 0x0a00_0000)]
        );
    }
}
