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
//! [`tairix_abi::driver_store::DRIVER_STORE_ENDPOINT`] avoids threading an
//! [`Arc`] through `KernelState`'s cross-crate wiring. Each endpoint is held behind an [`Arc`] so the server and every
//! in-flight caller share one instance for its life.
//!
//! # Fail closed
//!
//! [`register`] refuses to overwrite a live binding (returns
//! [`Errno::AlreadyExists`] and audits the refusal as
//! [`AuditEvent::CallEndpointRegisterDenied`], mirroring the port
//! registry's register-denied event) so the kernel never silently
//! re-points a live endpoint; [`lookup`] of an unbound id yields `None`,
//! which the handler maps to [`Errno::NotFound`] and audits at that
//! boundary (mirroring [`tairix_kernel_ipc::registry::PortRegistry`]).

use alloc::collections::{BTreeMap, BTreeSet};
use alloc::sync::Arc;
use alloc::vec::Vec;

use tairix_abi::Errno;
use tairix_kernel_ipc::audit::record;
use tairix_kernel_ipc::{AuditEvent, CallEndpoint, EndpointId};
use tairix_log::{Field, Sink};
use tairix_sync::{RwLock, SpinLock};
use tairix_util::fmt::{format_hex_u64, format_u64};

use crate::aspace::AddressSpaceRegistry;

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
/// live endpoint. The refusal is the security decision this rendezvous
/// makes, so it is recorded on `audit` as
/// [`AuditEvent::CallEndpointRegisterDenied`] (a bare "endpoint created"
/// with no subsequent denial would misread as a live endpoint); the
/// emission happens after the registry lock is released.
pub fn register(endpoint: Arc<CallEndpoint>, audit: &dyn Sink) -> Result<(), Errno> {
    let id = endpoint.id();
    let clashed = match CALL_ENDPOINTS.lock().entry(id) {
        alloc::collections::btree_map::Entry::Occupied(_) => true,
        alloc::collections::btree_map::Entry::Vacant(slot) => {
            slot.insert(endpoint);
            false
        }
    };
    if clashed {
        let mut id_buf = [0u8; 16];
        let id_field = Field {
            key: "endpoint",
            value: tairix_log::FieldValue::Str(format_hex_u64(id.0, &mut id_buf)),
        };
        record(audit, AuditEvent::CallEndpointRegisterDenied, &[id_field]);
        return Err(Errno::AlreadyExists);
    }
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

/// Unbind and [`CallEndpoint::destroy`] every endpoint owned by the exiting
/// task `owner` (cancelling its in-flight calls), returning the ids that
/// were torn down. Pure registry mechanics; the wake and the vanish
/// notification live in [`teardown_owned_by`].
fn unregister_owned_by(owner: u64, audit: &dyn Sink) -> Vec<EndpointId> {
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
    removed.iter().map(|ep| ep.id()).collect()
}

/// An observer of endpoint teardown: told, after the fact, that an owner's
/// endpoints were destroyed. The runtime volume service listens here so a
/// surprise-removed disk's volume reacts the moment its serving driver
/// dies (`plans/DEVICES.md` D4).
pub trait EndpointVanishObserver: Sync {
    /// `id` was unbound and destroyed because its owning task ended. Runs
    /// in the tearing-down task's context after parked callers were woken:
    /// the observer may take sleeping locks, but every in-flight call on
    /// the endpoint has already been cancelled, so nothing it waits on can
    /// depend on the dead endpoint.
    fn endpoint_vanished(&self, id: EndpointId);
}

/// The set-once vanish observer, installed by the boot path. Fail-closed
/// `None` before install: teardown simply has no listener.
static VANISH_OBSERVER: tairix_sync::OnceCell<&'static dyn EndpointVanishObserver> =
    tairix_sync::OnceCell::new();

/// Install the endpoint-vanish observer. First-wins and idempotent, like
/// the other late-installed boot seams.
pub fn install_vanish_observer(observer: &'static dyn EndpointVanishObserver) {
    let _ = VANISH_OBSERVER.set(observer);
}

/// Tear down every endpoint owned by the exiting task `owner`, wake the
/// callers its destruction cancelled, revoke the per-endpoint grants those
/// endpoints' ids named, and notify the vanish observer.
///
/// A user-space service may exit (cleanly, by fault, or killed) while
/// callers are blocked in `ipc_call` awaiting its replies. Without this,
/// those callers would park forever on a dead endpoint; destroying the
/// endpoint flips every outstanding call to
/// [`tairix_kernel_ipc::ReplyOutcome::Cancelled`] so the next poll abandons
/// fail-closed. The observer is notified strictly **after**
/// [`crate::waitq::call_wake`]: a caller parked mid-call on the dead
/// endpoint may hold a lock the observer needs, and the wake is what lets
/// that caller finish and release it. The per-endpoint grants naming the
/// destroyed ids are revoked *before* the wake, so no woken caller can re-post
/// to an id that is about to be re-bindable by someone else.
pub fn teardown_owned_by(owner: u64, aspaces: &RwLock<AddressSpaceRegistry>, audit: &dyn Sink) {
    let removed = unregister_owned_by(owner, audit);
    if removed.is_empty() {
        return;
    }
    revoke_grants_for(owner, &removed, aspaces, audit);
    crate::waitq::call_wake();
    if let Ok(Some(observer)) = VANISH_OBSERVER.get() {
        for id in removed {
            observer.endpoint_vanished(id);
        }
    }
}

/// Withdraw every per-endpoint grant naming one of the just-destroyed
/// `endpoints`, recording what was withdrawn.
///
/// An endpoint id is a number, and a number is re-creatable: once these
/// endpoints are gone another task may bind the same ids. A holder's grant
/// that outlived its endpoint would then silently retarget onto the new
/// instance, handing it the authority to call a service it was never
/// granted. Revoking in the same teardown that destroyed the endpoints is
/// what makes id reuse safe; taking the registry as an argument is what
/// makes it impossible to destroy an endpoint without doing so.
fn revoke_grants_for(
    owner: u64,
    endpoints: &[EndpointId],
    aspaces: &RwLock<AddressSpaceRegistry>,
    audit: &dyn Sink,
) {
    let ids: BTreeSet<u64> = endpoints.iter().map(|id| id.0).collect();
    // The write guard is released with this statement, so the audit record
    // below is emitted with no registry lock held.
    let revoked = aspaces.write().revoke_endpoint_grants(&ids);
    if revoked == 0 {
        return;
    }
    let mut owner_buf = [0u8; 16];
    let mut count_buf = [0u8; 20];
    record(
        audit,
        AuditEvent::CallEndpointGrantsRevoked,
        &[
            // The task whose teardown withdrew the authority; the ids it lost
            // are the `CallEndpointDestroyed` records emitted immediately
            // before this one.
            Field {
                key: "owner",
                value: tairix_log::FieldValue::Str(format_hex_u64(owner, &mut owner_buf)),
            },
            Field {
                key: "grants",
                value: tairix_log::FieldValue::Str(format_u64(revoked as u64, &mut count_buf)),
            },
        ],
    );
}

/// Cancel every in-flight call the exiting task `sender` posted, on every
/// live endpoint — the converse of [`teardown_owned_by`]: that releases the
/// endpoints a dead task *served*; this scrubs the calls a dead task *sent*.
///
/// Without it a dead caller's queued request outlives it and is later
/// handed to the server as if live — the Pi 4 USB defect where an unloaded
/// class driver's final URB submit survived its death and wedged the
/// single-slot URB transport against the replacement driver. Each endpoint
/// with cancellations records one `CallPosterVanished` audit event. No wake
/// is needed: the poster is dead and the server parks only on *arrival* of
/// requests, never their removal.
pub fn cancel_posted_by(sender: u64, audit: &dyn Sink) {
    // Snapshot under the registry lock, cancel outside it (each
    // cancellation takes the endpoint's own interior lock, never this one).
    let endpoints: Vec<Arc<CallEndpoint>> = CALL_ENDPOINTS.lock().values().cloned().collect();
    for ep in endpoints {
        let _ = ep.cancel_posted_by(sender, audit);
    }
}

/// `true` if `id` is currently bound. Diagnostic / test observer.
#[must_use]
pub fn contains(id: EndpointId) -> bool {
    CALL_ENDPOINTS.lock().contains_key(&id)
}

#[cfg(test)]
mod tests {
    use super::*;
    use tairix_caps::CapabilitySet;
    use tairix_kernel_ipc::CallEndpointLimits;
    use tairix_kernel_sec::captable::{ProcessId, TaskCapabilities};
    use tairix_kernel_sec::identity::UserId;
    use tairix_log::Sink;

    /// A throwaway audit sink for endpoint construction in tests.
    struct NullSink;
    impl Sink for NullSink {
        fn write_event(&self, _event: &tairix_log::Event<'_>) {}
    }

    /// A recording sink capturing emitted event ids, for asserting the
    /// registration-denied audit.
    struct RecordingSink {
        ids: std::cell::RefCell<std::vec::Vec<u32>>,
    }
    impl RecordingSink {
        fn new() -> Self {
            tairix_log::set_max_level(tairix_log::Level::Trace);
            Self {
                ids: std::cell::RefCell::new(std::vec::Vec::new()),
            }
        }
    }
    impl Sink for RecordingSink {
        fn write_event(&self, event: &tairix_log::Event<'_>) {
            self.ids.borrow_mut().push(event.id.0);
        }
    }

    fn endpoint(id: u64) -> Arc<CallEndpoint> {
        let sink = NullSink;
        let creator = TaskCapabilities::derive(
            ProcessId(1),
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
        register(endpoint(id.0), &NullSink).expect("first bind succeeds");
        assert!(contains(id));
        assert_eq!(lookup(id).map(|e| e.id()), Some(id));
        // Clean up so the global registry does not leak across tests.
        assert!(unregister(id).is_some());
        assert!(!contains(id));
    }

    #[test]
    fn duplicate_register_is_already_exists_and_audited() {
        let id = EndpointId(0xCA11_0002);
        let sink = RecordingSink::new();
        register(endpoint(id.0), &sink).expect("first bind succeeds");
        assert!(
            sink.ids.borrow().is_empty(),
            "a successful bind emits no registry audit"
        );
        let err = register(endpoint(id.0), &sink).expect_err("duplicate refused");
        assert_eq!(err, Errno::AlreadyExists);
        assert_eq!(
            *sink.ids.borrow(),
            [AuditEvent::CallEndpointRegisterDenied.id().0],
            "the refused bind is the audited decision"
        );
        unregister(id);
    }

    #[test]
    fn cancel_posted_by_scrubs_the_dead_posters_calls_on_registered_endpoints() {
        let id = EndpointId(0xCA11_0004);
        let ep = endpoint(id.0);
        register(Arc::clone(&ep), &NullSink).expect("bound");
        // The poster task id must be unique to this test, not a small
        // shared constant: `cancel_posted_by` scrubs the poster's calls
        // across the whole (process-global) registry, so a sibling test
        // reusing the same id and scrubbing it in parallel would cancel
        // this call between the post and the assert. The endpoint id is
        // already made unique for the same reason.
        let poster = 0x0004_1234;
        let caller = TaskCapabilities::derive(
            ProcessId(poster),
            UserId(1),
            CapabilitySet::empty(),
            CapabilitySet::empty(),
            &NullSink,
        );
        ep.post(&caller, 0, b"q", u64::MAX, &NullSink)
            .expect("posted");
        assert!(ep.has_pending());

        cancel_posted_by(poster, &NullSink);

        // The dead poster's queued call is gone before any server sees it.
        assert!(!ep.has_pending());
        unregister(id);
    }

    #[test]
    fn lookup_unbound_is_none_and_unregister_is_idempotent() {
        let id = EndpointId(0xCA11_0003);
        assert!(lookup(id).is_none());
        assert!(unregister(id).is_none());
    }
}
