//! The kernel call-endpoint registry: a map from [`EndpointId`] to the
//! live [`CallEndpoint`] bound to it (Design D D2b — `.junie/next-pi-prompt.md`).
//!
//! A [`CallEndpoint`] (`kernel/ipc`) is the synchronous request/reply
//! state machine the reactive driver-store file service is built on. The
//! server half lives on a never-returning kernel kthread (the disk-owning
//! driver-store service) and the caller half is the `ipc_call` syscall
//! handler (`crate::syscalls`); both must reach the *same* endpoint
//! instance. This registry is that rendezvous: the server
//! [`register`]s the endpoint it created under its well-known id,
//! and the handler [`lookup`]s the id the caller named.
//!
//! # Why a registry and not a borrowed seam
//!
//! Like [`crate::waitq::CALL_WAITQ`], the registry is global pure data
//! behind a [`SpinLock`] (never a `static mut`, so it is not the global
//! mutable static the charter forbids): the kthread server and the
//! syscall handler live in different crates and neither owns the other, so
//! a global rendezvous keyed by the well-known
//! [`rustos_abi::driver_store::DRIVER_STORE_ENDPOINT`] avoids threading an
//! [`Arc`] through `KernelState`'s cross-crate wiring. Each endpoint is held behind an [`Arc`] so the server and every
//! in-flight caller share one instance for its life.
//!
//! # Fail closed
//!
//! [`register`] refuses to overwrite a live binding (returns
//! [`Errno::AlreadyExists`]) so the kernel never silently re-points a live
//! endpoint; [`lookup`] of an unbound id yields `None`,
//! which the handler maps to [`Errno::NotFound`] and audits at that
//! boundary (mirroring [`rustos_kernel_ipc::registry::PortRegistry`]).

use alloc::collections::BTreeMap;
use alloc::sync::Arc;
use alloc::vec::Vec;

use rustos_abi::Errno;
use rustos_kernel_ipc::{CallEndpoint, EndpointId};
use rustos_log::Sink;
use rustos_sync::SpinLock;

/// The global call-endpoint registry (set-up by the boot path's kthread
/// server, read by the `ipc_call` syscall handler). Pure data behind a
/// [`SpinLock`]; see the module docs for why it is global.
static CALL_ENDPOINTS: SpinLock<BTreeMap<EndpointId, Arc<CallEndpoint>>> =
    SpinLock::new(BTreeMap::new());

/// Bind `endpoint` into the registry under its own [`CallEndpoint::id`].
///
/// # Errors
///
/// [`Errno::AlreadyExists`] if the id is already bound; the existing
/// binding is left untouched and the kernel never silently overwrites a
/// live endpoint.
pub fn register(endpoint: Arc<CallEndpoint>) -> Result<(), Errno> {
    let id = endpoint.id();
    let mut map = CALL_ENDPOINTS.lock();
    if map.contains_key(&id) {
        return Err(Errno::AlreadyExists);
    }
    map.insert(id, endpoint);
    Ok(())
}

/// Resolve `id` to the live [`CallEndpoint`] bound to it, if any.
///
/// Returns an [`Arc`] clone so the caller may post/take outside the
/// registry lock (the endpoint owns its own interior lock). A miss is not
/// a security decision and is not audited here.
#[must_use]
pub fn lookup(id: EndpointId) -> Option<Arc<CallEndpoint>> {
    CALL_ENDPOINTS.lock().get(&id).cloned()
}

/// Remove the binding for `id`, returning the endpoint that was bound (if
/// any) so the caller can [`CallEndpoint::destroy`] it. Idempotent.
pub fn unregister(id: EndpointId) -> Option<Arc<CallEndpoint>> {
    CALL_ENDPOINTS.lock().remove(&id)
}

/// Tear down every endpoint owned by the exiting task `owner`: unbind it and
/// [`CallEndpoint::destroy`] it (cancelling its in-flight calls), returning
/// how many were torn down.
///
/// A user-space service may exit (cleanly, by fault, or killed) while callers
/// are blocked in `ipc_call` awaiting its replies. Without this, those
/// callers would park forever on a dead endpoint; destroying the endpoint
/// flips every outstanding call to [`rustos_kernel_ipc::ReplyOutcome::Cancelled`]
/// so the next poll abandons fail-closed. The
/// caller wakes the parked callers (via [`crate::waitq::call_wake`]) when the
/// return value is non-zero — kept out of this registry function so it stays
/// pure registry mechanics.
pub fn unregister_owned_by(owner: u64, audit: &dyn Sink) -> usize {
    // Collect-then-remove under one lock acquisition: gather the owned ids,
    // then drop the lock before destroying (each `destroy` takes the
    // endpoint's *own* interior lock, never this registry lock).
    let removed: Vec<Arc<CallEndpoint>> = {
        let mut map = CALL_ENDPOINTS.lock();
        let ids: Vec<EndpointId> = map
            .iter()
            .filter(|(_, ep)| ep.owner() == owner)
            .map(|(id, _)| *id)
            .collect();
        ids.into_iter().filter_map(|id| map.remove(&id)).collect()
    };
    for ep in &removed {
        ep.destroy(audit);
    }
    removed.len()
}

/// `true` if `id` is currently bound. Diagnostic / test observer.
#[must_use]
pub fn contains(id: EndpointId) -> bool {
    CALL_ENDPOINTS.lock().contains_key(&id)
}

#[cfg(test)]
mod tests {
    use super::*;
    use rustos_caps::CapabilitySet;
    use rustos_kernel_ipc::CallEndpointLimits;
    use rustos_kernel_sec::captable::{TaskCapabilities, TaskId};
    use rustos_kernel_sec::identity::UserId;
    use rustos_log::Sink;

    /// A throwaway audit sink for endpoint construction in tests.
    struct NullSink;
    impl Sink for NullSink {
        fn write_event(&self, _event: &rustos_log::Event<'_>) {}
    }

    fn endpoint(id: u64) -> Arc<CallEndpoint> {
        let sink = NullSink;
        let creator = TaskCapabilities::derive(
            TaskId(1),
            UserId(1),
            CapabilitySet::empty(),
            CapabilitySet::empty(),
            &sink,
        );
        Arc::new(
            CallEndpoint::create(
                EndpointId(id),
                &creator,
                CapabilitySet::empty(),
                CapabilitySet::empty(),
                CallEndpointLimits {
                    max_request: 64,
                    max_reply: 64,
                    capacity: 4,
                },
                &sink,
            )
            .expect("unrestricted endpoint"),
        )
    }

    #[test]
    fn register_then_lookup_round_trips() {
        let id = EndpointId(0xCA11_0001);
        assert!(!contains(id));
        register(endpoint(id.0)).expect("first bind succeeds");
        assert!(contains(id));
        assert_eq!(lookup(id).map(|e| e.id()), Some(id));
        // Clean up so the global registry does not leak across tests.
        assert!(unregister(id).is_some());
        assert!(!contains(id));
    }

    #[test]
    fn duplicate_register_is_already_exists() {
        let id = EndpointId(0xCA11_0002);
        register(endpoint(id.0)).expect("first bind succeeds");
        let err = register(endpoint(id.0)).expect_err("duplicate refused");
        assert_eq!(err, Errno::AlreadyExists);
        unregister(id);
    }

    #[test]
    fn lookup_unbound_is_none_and_unregister_is_idempotent() {
        let id = EndpointId(0xCA11_0003);
        assert!(lookup(id).is_none());
        assert!(unregister(id).is_none());
    }
}
