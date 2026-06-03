//! The named-port registry: a capability-checked map from
//! [`EndpointId`] to the live [`Port`] bound to it.
//!
//! A [`Port`] on its own is anonymous — [`Port::create`] performs the
//! bind-time capability check (`AGENTS.md` §5.2) and hands back an owned
//! value, but nothing yet lets a *sender* or *receiver* reach it by its
//! [`EndpointId`]. The syscall dispatcher's `ipc_send` / `ipc_recv`
//! handlers need exactly that lookup: given the endpoint number carried
//! in an [`rustos_abi::ipc::IpcMessageHeader`], find the kernel-owned
//! [`Port`] to enqueue into or drain from. [`PortRegistry`] is that map.
//!
//! # No interior mutability
//!
//! Like [`rustos_kernel_sec::CapTable`], the registry exposes a plain
//! `&self` / `&mut self` surface and owns no lock of its own; the
//! synchronisation policy lives with the kernel's `KernelState`, which
//! composes the registry with the scheduler and the capability table
//! under one lock-ordering policy (`AGENTS.md` §2.1 — no global mutable
//! static). Lookups borrow `&self` (so concurrent senders share a read
//! guard); registration and unregistration take `&mut self`.
//!
//! # Fail closed
//!
//! Every state-changing operation emits exactly one audit record before
//! returning (`AGENTS.md` §5.4):
//!
//! * [`PortRegistry::register`] refuses to overwrite a live binding. A
//!   duplicate [`EndpointId`] returns the *supplied* port back to the
//!   caller alongside [`Errno::AlreadyExists`] (so the caller can tear
//!   the rejected port down) and emits [`AuditEvent::PortRegisterDenied`];
//!   the existing binding is left untouched.
//! * [`PortRegistry::unregister`] destroys the removed port (draining any
//!   in-flight messages, `AGENTS.md` §5.4) and emits
//!   [`AuditEvent::PortUnregistered`]; unregistering an unknown endpoint
//!   returns [`Errno::NotFound`].
//!
//! The registry performs *no* capability check of its own: the authority
//! to bind an endpoint was already proven at [`Port::create`] time, and
//! the per-send authority is re-checked on every [`Port::send`]. The
//! registry is a pure ownership map, mirroring how [`CapTable`] stores
//! the output of `TaskCapabilities::derive` without re-deriving it.
//!
//! [`CapTable`]: rustos_kernel_sec::CapTable

extern crate alloc;

use alloc::boxed::Box;
use alloc::collections::BTreeMap;

use rustos_abi::Errno;
use rustos_log::{Field, Sink};
use rustos_util::fmt::format_hex_u64;

use crate::audit::{record, AuditEvent};
use crate::port::{EndpointId, Port};

/// A map from [`EndpointId`] to the kernel-owned [`Port`] bound to it.
///
/// Construct with [`PortRegistry::new`]. The registry owns each
/// registered [`Port`] for the lifetime of the binding; dropping the
/// registry drops every port it still holds.
#[derive(Default)]
pub struct PortRegistry {
    ports: BTreeMap<EndpointId, Port>,
}

impl PortRegistry {
    /// Create an empty registry.
    #[must_use]
    pub const fn new() -> Self {
        Self {
            ports: BTreeMap::new(),
        }
    }

    /// Bind `port` into the registry under its own [`Port::id`].
    ///
    /// # Errors
    ///
    /// Returns `Err((Box<port>, `[`Errno::AlreadyExists`]`))` if the
    /// port's [`EndpointId`] is already bound. The existing binding is
    /// left untouched and the *supplied* `port` is handed back (boxed so
    /// the error variant stays small) so the caller can [`Port::destroy`]
    /// it; the kernel never silently overwrites a live endpoint
    /// (`AGENTS.md` §5.4).
    ///
    /// On refusal exactly one [`AuditEvent::PortRegisterDenied`] is
    /// emitted; on success exactly one [`AuditEvent::PortRegistered`].
    pub fn register<S: Sink + ?Sized>(
        &mut self,
        port: Port,
        audit: &S,
    ) -> Result<EndpointId, (Box<Port>, Errno)> {
        let id = port.id();
        let mut id_buf = [0u8; 16];
        let id_field = Field {
            key: "port",
            value: format_hex_u64(id.0, &mut id_buf),
        };

        if self.ports.contains_key(&id) {
            record(audit, AuditEvent::PortRegisterDenied, &[id_field]);
            return Err((Box::new(port), Errno::AlreadyExists));
        }

        self.ports.insert(id, port);
        record(audit, AuditEvent::PortRegistered, &[id_field]);
        Ok(id)
    }

    /// Borrow the [`Port`] bound to `id`, if any.
    ///
    /// Returns `None` for an unbound endpoint — a lookup miss is not a
    /// security decision and is not audited (the eventual `ipc_send` /
    /// `ipc_recv` handler maps the `None` to [`Errno::NotFound`] and
    /// audits at that boundary). Borrowing `&self` lets concurrent
    /// senders share a read guard over the registry while each
    /// [`Port::send`] re-checks the per-send capability.
    #[must_use]
    pub fn lookup(&self, id: EndpointId) -> Option<&Port> {
        self.ports.get(&id)
    }

    /// `true` if `id` is currently bound.
    #[must_use]
    pub fn contains(&self, id: EndpointId) -> bool {
        self.ports.contains_key(&id)
    }

    /// Remove the port bound to `id` and destroy it.
    ///
    /// The removed port is [`Port::destroy`]ed (transitioning it closed
    /// so any racing sender fails closed, and draining its mailbox) and
    /// one [`AuditEvent::PortUnregistered`] is emitted.
    ///
    /// # Errors
    ///
    /// Returns [`Errno::NotFound`] if `id` is not bound; nothing is
    /// changed and no event is emitted (an unregister miss is not a
    /// security decision, mirroring [`Self::lookup`]).
    pub fn unregister<S: Sink + ?Sized>(&mut self, id: EndpointId, audit: &S) -> Result<(), Errno> {
        let Some(port) = self.ports.remove(&id) else {
            return Err(Errno::NotFound);
        };

        // `destroy` emits PortDestroyed and drains in-flight messages;
        // the registry then records that the *binding* is gone so the
        // security trail distinguishes "port torn down" from "binding
        // removed".
        port.destroy(audit);
        let mut id_buf = [0u8; 16];
        record(
            audit,
            AuditEvent::PortUnregistered,
            &[Field {
                key: "port",
                value: format_hex_u64(id.0, &mut id_buf),
            }],
        );
        Ok(())
    }

    /// Number of currently-bound endpoints.
    #[must_use]
    pub fn len(&self) -> usize {
        self.ports.len()
    }

    /// `true` if no endpoint is currently bound.
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.ports.is_empty()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::audit::RecordingSink;
    use rustos_abi::ipc::IPC_MESSAGE_MAX_PAYLOAD_LEN;
    use rustos_abi::CapabilityId;
    use rustos_caps::CapabilitySet;
    use rustos_kernel_sec::captable::{TaskCapabilities, TaskId};
    use rustos_kernel_sec::identity::UserId;

    fn caps_of(items: &[CapabilityId]) -> CapabilitySet {
        let mut s = CapabilitySet::empty();
        for c in items {
            s.insert(*c);
        }
        s
    }

    fn task_with(task_id: u64, caps: &[CapabilityId]) -> TaskCapabilities {
        let sink = RecordingSink::new();
        let set = caps_of(caps);
        TaskCapabilities::derive(TaskId(task_id), UserId(1), set, set, &sink)
    }

    fn open_port(id: u64, sink: &RecordingSink) -> Port {
        let creator = task_with(1, &[]);
        Port::create(
            EndpointId(id),
            &creator,
            CapabilitySet::empty(),
            CapabilitySet::empty(),
            IPC_MESSAGE_MAX_PAYLOAD_LEN,
            8,
            sink,
        )
        .expect("unrestricted port creation succeeds")
    }

    #[test]
    fn new_registry_is_empty() {
        let reg = PortRegistry::new();
        assert!(reg.is_empty());
        assert_eq!(reg.len(), 0);
        assert!(reg.lookup(EndpointId(1)).is_none());
        assert!(!reg.contains(EndpointId(1)));
    }

    #[test]
    fn register_binds_and_lookup_returns_the_port() {
        let sink = RecordingSink::new();
        let mut reg = PortRegistry::new();
        let port = open_port(7, &sink);

        let id = reg
            .register(port, &sink)
            .map_err(|(_, e)| e)
            .expect("first bind succeeds");
        assert_eq!(id, EndpointId(7));
        assert_eq!(reg.len(), 1);
        assert!(reg.contains(EndpointId(7)));
        assert_eq!(reg.lookup(EndpointId(7)).map(Port::id), Some(EndpointId(7)));
        assert!(sink.ids().contains(&AuditEvent::PortRegistered.id().0));
    }

    #[test]
    fn register_duplicate_is_already_exists_and_hands_the_port_back() {
        let sink = RecordingSink::new();
        let mut reg = PortRegistry::new();
        reg.register(open_port(3, &sink), &sink)
            .map_err(|(_, e)| e)
            .expect("first bind succeeds");

        let dup = open_port(3, &sink);
        match reg.register(dup, &sink) {
            Ok(_) => panic!("a duplicate endpoint must be refused"),
            Err((returned, err)) => {
                assert_eq!(err, Errno::AlreadyExists);
                // The rejected port is handed back so the caller can tear it down.
                assert_eq!(returned.id(), EndpointId(3));
            }
        }
        // The original binding is untouched.
        assert_eq!(reg.len(), 1);
        assert!(sink.ids().contains(&AuditEvent::PortRegisterDenied.id().0));
    }

    #[test]
    fn lookup_finds_sent_messages_through_the_registry() {
        let sink = RecordingSink::new();
        let mut reg = PortRegistry::new();
        reg.register(open_port(5, &sink), &sink)
            .map_err(|(_, e)| e)
            .expect("bind succeeds");
        let sender = task_with(2, &[]);

        let port = reg.lookup(EndpointId(5)).expect("bound port is found");
        port.send(&sender, b"ping", &sink).expect("send succeeds");

        let msg = reg
            .lookup(EndpointId(5))
            .and_then(Port::recv)
            .expect("the delivered message is reachable through the registry");
        assert_eq!(msg.payload, b"ping");
    }

    #[test]
    fn unregister_removes_destroys_and_audits() {
        let sink = RecordingSink::new();
        let mut reg = PortRegistry::new();
        reg.register(open_port(9, &sink), &sink)
            .map_err(|(_, e)| e)
            .expect("bind succeeds");

        reg.unregister(EndpointId(9), &sink)
            .expect("unregister of a bound endpoint succeeds");
        assert!(reg.is_empty());
        assert!(reg.lookup(EndpointId(9)).is_none());
        let ids = sink.ids();
        assert!(ids.contains(&AuditEvent::PortDestroyed.id().0));
        assert!(ids.contains(&AuditEvent::PortUnregistered.id().0));
    }

    #[test]
    fn unregister_unknown_endpoint_is_not_found() {
        let sink = RecordingSink::new();
        let mut reg = PortRegistry::new();
        let err = reg
            .unregister(EndpointId(42), &sink)
            .expect_err("unregistering an unbound endpoint fails closed");
        assert_eq!(err, Errno::NotFound);
    }

    #[test]
    fn endpoint_can_be_rebound_after_unregister() {
        let sink = RecordingSink::new();
        let mut reg = PortRegistry::new();
        reg.register(open_port(11, &sink), &sink)
            .map_err(|(_, e)| e)
            .expect("first bind succeeds");
        reg.unregister(EndpointId(11), &sink)
            .expect("unregister succeeds");

        reg.register(open_port(11, &sink), &sink)
            .map_err(|(_, e)| e)
            .expect("the freed endpoint can be re-bound");
        assert!(reg.contains(EndpointId(11)));
    }

    #[test]
    fn distinct_endpoints_coexist() {
        let sink = RecordingSink::new();
        let mut reg = PortRegistry::new();
        reg.register(open_port(1, &sink), &sink)
            .map_err(|(_, e)| e)
            .expect("bind 1");
        reg.register(open_port(2, &sink), &sink)
            .map_err(|(_, e)| e)
            .expect("bind 2");
        assert_eq!(reg.len(), 2);
        assert!(reg.contains(EndpointId(1)));
        assert!(reg.contains(EndpointId(2)));
    }
}
