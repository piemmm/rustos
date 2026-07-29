//! Capability-checked synchronous call/reply endpoints.
//!
//! A [`Port`](crate::port::Port) is fire-and-forget: a sender enqueues a
//! message and never hears back. The reactive driver-store file service
//! (Design D, D2b — `.junie/next-pi-prompt.md`) and any future request/reply
//! system service instead need **synchronous** semantics: a caller posts a
//! request, blocks until exactly one matching reply arrives, and reads it.
//! [`CallEndpoint`] is that primitive.
//!
//! Like [`Port`](crate::port::Port) it is a kernel-owned endpoint identified
//! by a stable [`EndpointId`] and gated by capabilities (checked at create
//! and on every post; the single bound server does not re-check ). Unlike a port it correlates each request with one
//! reply through an opaque, unforgeable [`CallTicket`]:
//!
//! * a caller [`post`](CallEndpoint::post)s a request and receives a ticket;
//! * the server [`recv_call`](CallEndpoint::recv_call)s the oldest pending
//!   request (moving it to an in-service table keyed by its ticket);
//! * the server [`reply`](CallEndpoint::reply)s with that ticket;
//! * the caller [`take_reply`](CallEndpoint::take_reply)s its ticket to claim
//!   the reply.
//!
//! # Not a scheduler primitive
//!
//! This type is the request/reply *state machine* only; it never blocks. The
//! blocking — the caller parking until its ticket is replied, the server
//! parking until a request arrives — is layered above through the same
//! cooperative yield/park seam the IRQ wait and `wait` syscalls use
//! (`kernel/core`), so the primitive stays synchronous-test-friendly and
//! free of any scheduler dependency. A parker
//! polls [`CallEndpoint::recv_call`] / [`CallEndpoint::take_reply`] (both
//! return immediately) between parks, exactly as `block_until_ready` polls
//! IRQ readiness.
//!
//! # Fail closed
//!
//! Every refused operation emits exactly one [`crate::audit`] event before
//! returning a stable [`Errno`]. A destroyed endpoint
//! cancels every in-flight ticket: an outstanding
//! [`CallEndpoint::take_reply`] reports [`ReplyOutcome::Cancelled`] so a
//! parked caller wakes and abandons rather than waiting forever.

extern crate alloc;

use alloc::collections::{BTreeMap, VecDeque};
use alloc::vec::Vec;

use tairix_abi::ipc::IPC_MESSAGE_MAX_PAYLOAD_LEN;
use tairix_abi::{Errno, Origin};
use tairix_caps::CapabilitySet;
use tairix_kernel_sec::captable::TaskCapabilities;
use tairix_log::{Field, Sink};
use tairix_util::fmt::{format_hex_u64, format_usize};

use crate::audit::{record, AuditEvent};
use crate::loom_compat::{AtomicU32, AtomicU64, Ordering};
use crate::port::EndpointId;

/// Fixed atomic states a [`CallEndpoint`] can be in, encoded into one
/// `AtomicU32` so the post fast path observes liveness without the lock.
mod state {
    /// Open and accepting requests.
    pub(super) const OPEN: u32 = 0;
    /// `destroy()` has begun; posters fail closed and in-flight callers
    /// observe [`super::ReplyOutcome::Cancelled`].
    pub(super) const CLOSED: u32 = 1;
}

/// An opaque, unforgeable handle correlating a posted request with its
/// reply.
///
/// Minted by [`CallEndpoint::post`] from a per-endpoint monotonic counter
/// and surrendered to [`CallEndpoint::take_reply`]. The newtype keeps call
/// tickets distinct from endpoint, task, and capability identifiers.
#[repr(transparent)]
#[derive(Copy, Clone, Debug, Eq, PartialEq, Hash, Ord, PartialOrd)]
pub struct CallTicket(pub u64);

/// The outcome of a server-side [`CallEndpoint::recv_call`].
///
/// `recv_call` is *size-bounded*: it dequeues the front request only when it
/// fits the server's buffer, so a too-small buffer never silently drops a
/// queued request. The in-kernel server passes
/// [`usize::MAX`] and so only ever observes [`RecvCall::Empty`] or
/// [`RecvCall::Received`].
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum RecvCall {
    /// No request is pending; the server should park and retry (this never
    /// blocks).
    Empty,
    /// The front request is larger than the server's buffer; it is left
    /// queued and `request_len` is reported so the server can resize and
    /// retry (or fail the call closed). The kernel surfaces this as
    /// [`Errno::BufferTooSmall`].
    TooLarge {
        /// Byte length of the front request the buffer could not hold.
        request_len: usize,
    },
    /// A request was dequeued for service.
    Received(ReceivedCall),
}

/// A request handed to the server by [`CallEndpoint::recv_call`].
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ReceivedCall {
    /// The ticket the server must [`reply`](CallEndpoint::reply) with.
    pub ticket: CallTicket,
    /// Task identifier of the caller that posted the request.
    pub sender: u64,
    /// The request payload (kernel-owned copy).
    pub request: Vec<u8>,
}

/// The result of [`CallEndpoint::take_reply`] for a given ticket.
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum ReplyOutcome {
    /// The request has been posted (and perhaps received) but the server
    /// has not replied yet. The caller should park and retry.
    Pending,
    /// The reply is ready; its bytes are returned and the ticket retired.
    Ready(Vec<u8>),
    /// The request's per-ticket deadline elapsed before a reply arrived; the
    /// ticket is retired so the caller fails closed with a timeout rather
    /// than parking forever on a wedged callee.
    TimedOut,
    /// The endpoint was destroyed before a reply arrived; the caller must
    /// abandon the call.
    Cancelled,
    /// The ticket is unknown to this (open) endpoint — never posted here,
    /// or already claimed. Fail closed.
    Unknown,
}

/// The size and capacity bounds a [`CallEndpoint`] is created with.
///
/// Grouped into one value so [`CallEndpoint::create`] stays a small,
/// reviewable constructor rather than a long positional argument list, and
/// so a caller cannot transpose the two payload caps.
#[derive(Copy, Clone, Debug, Eq, PartialEq)]
pub struct CallEndpointLimits {
    /// Maximum request payload (bytes) [`CallEndpoint::post`] accepts.
    pub max_request: u32,
    /// Maximum reply payload (bytes) [`CallEndpoint::reply`] accepts.
    pub max_reply: u32,
    /// Maximum number of outstanding calls before [`CallEndpoint::post`]
    /// fails closed (a fail-closed memory bound, not a scaling capacity ).
    pub capacity: usize,
}

/// One posted request awaiting receipt or reply.
struct PendingCall {
    ticket: u64,
    sender: u64,
    /// The posting task's *scheduler* id, captured at post time so the
    /// reply path can wake exactly this caller off the run queue instead
    /// of broadcasting to every parked caller (the wake-one discipline —
    /// a wake-all here is a thundering herd that keeps unrelated tasks
    /// runnable and distorts the load census).
    poster_sched: u64,
    /// The caller's kernel-attested identity, captured at post time so the
    /// server can later retrieve it by ticket through
    /// [`CallEndpoint::peer_origin`]. Snapshotting it at post (rather than
    /// resolving the caller again at receive) ties the origin to the exact
    /// principal that made *this* call, immune to later changes to that
    /// task's capabilities or to PID reuse.
    origin: Origin,
    /// Absolute monotonic deadline (ns) after which an unanswered request is
    /// reaped as [`ReplyOutcome::TimedOut`]. [`u64::MAX`] means no deadline
    /// (the `waitset_wait`/`irq_wait` convention), so `ipc_call`'s
    /// deadline-less park is unchanged. Time is supplied by the caller
    /// (`now` in [`CallEndpoint::take_reply`]) because the pure endpoint
    /// holds no clock.
    deadline: u64,
    request: Vec<u8>,
}

/// The lock-guarded request/reply state machine of a [`CallEndpoint`].
struct Inner {
    /// Next ticket value; monotonic for the life of the endpoint.
    next_ticket: u64,
    /// Posted, not yet received by the server (FIFO).
    pending: VecDeque<PendingCall>,
    /// Received by the server, awaiting [`CallEndpoint::reply`]. Keyed by
    /// ticket; the value is the posting caller's task id (so only that task
    /// may later claim the reply), its kernel-attested [`Origin`] (which
    /// [`CallEndpoint::peer_origin`] hands the server), and its scheduler
    /// id (which [`CallEndpoint::reply`] returns so the syscall layer wakes
    /// exactly the poster).
    in_service: BTreeMap<u64, (u64, Origin, u64, u64)>,
    /// Replied, awaiting [`CallEndpoint::take_reply`]. Keyed by ticket; the
    /// value is the posting caller's task id and the reply bytes.
    completed: BTreeMap<u64, (u64, Vec<u8>)>,
}

impl Inner {
    /// Total outstanding tickets, bounding endpoint memory at post time.
    fn outstanding(&self) -> usize {
        self.pending.len() + self.in_service.len() + self.completed.len()
    }
}

/// A kernel-owned synchronous call/reply endpoint.
///
/// Construct with [`CallEndpoint::create`] (which performs the bind-time
/// capability check) and tear down with [`CallEndpoint::destroy`]. Callers
/// [`post`](Self::post) requests; the single bound server drains them with
/// [`recv_call`](Self::recv_call) and answers with [`reply`](Self::reply);
/// callers claim answers with [`take_reply`](Self::take_reply).
///
/// `CallEndpoint` is [`Sync`]: the [`SpinLock`](tairix_sync::SpinLock) makes
/// interior access exclusive and the state word is atomic, so it may be
/// shared by `&` across CPUs exactly like a [`Port`](crate::port::Port).
pub struct CallEndpoint {
    id: EndpointId,
    required_send_caps: CapabilitySet,
    required_recv_caps: CapabilitySet,
    max_request: u32,
    max_reply: u32,
    capacity: usize,
    /// Task that created and serves this endpoint. The kernel tears the
    /// endpoint down when this task exits so in-flight callers are released
    /// fail-closed rather than blocked forever.
    owner: u64,
    /// The serving task's *scheduler* id, recorded the first time the server
    /// receives on this endpoint (`0` until then). The post path wakes
    /// exactly this task instead of broadcasting to every parked server
    /// (the wake-one discipline); a plain store/load suffices because the
    /// value is a wake *hint* — correctness is carried by the server's
    /// register-before-poll park loop, which re-polls after every wake.
    server_task: AtomicU64,
    /// Liveness read on the post fast path before taking the lock.
    state: AtomicU32,
    inner: tairix_sync::SpinLock<Inner>,
}

impl CallEndpoint {
    /// Create a new capability-checked synchronous call endpoint.
    ///
    /// The authority model is identical to [`Port::create`](crate::port::Port::create):
    /// `creator` must already hold every capability in `required_recv_caps`
    /// (no ambient authority), and must additionally hold
    /// [`tairix_abi::CapabilityId::IPC_BIND_PRIVILEGED`] when
    /// `required_send_caps` is non-empty (a restricted-sender endpoint is by
    /// definition privileged) **or** when `id` is a reserved well-known
    /// service rendezvous ([`tairix_abi::ipc::is_reserved_endpoint`]): even
    /// an open bind on a reserved id would let an unprivileged squatter
    /// claim the rendezvous and receive traffic meant for the service (an
    /// elevation request carries an offered password), so it fails closed.
    ///
    /// `capacity` bounds the number of *outstanding* calls (posted, in
    /// service, or replied-but-unclaimed) so a misbehaving caller or server
    /// cannot grow the endpoint without bound (fail-closed
    /// bound, not a scaling capacity).
    ///
    /// # Errors
    ///
    /// * [`Errno::LengthOutOfRange`] if `max_request` or `max_reply` exceeds
    ///   [`IPC_MESSAGE_MAX_PAYLOAD_LEN`], or `capacity == 0`.
    /// * [`Errno::PermissionDenied`] if `creator` lacks the bind authority
    ///   above.
    ///
    /// On any failure exactly one [`AuditEvent::CallEndpointCreateDenied`] is
    /// emitted; on success exactly one [`AuditEvent::CallEndpointCreated`].
    pub fn create<S: Sink + ?Sized>(
        id: EndpointId,
        creator: &TaskCapabilities,
        required_send_caps: CapabilitySet,
        required_recv_caps: CapabilitySet,
        limits: CallEndpointLimits,
        audit: &S,
    ) -> Result<Self, Errno> {
        Self::create_gated(
            id,
            creator,
            required_send_caps,
            required_recv_caps,
            limits,
            false,
            audit,
        )
    }

    /// Create a call endpoint on a **reserved** id whose bind authority is
    /// a fact the *syscall handler* kernel-attested instead of
    /// `CAP_IPC_BIND_PRIVILEGED` — today exactly one such fact exists: the
    /// creator holds a live seat lease, which entitles it to the
    /// seat-scoped window rendezvous (`tairix_abi::window_ipc`,
    /// `plans/APPWIN.md` AW3). The attestation must come from kernel
    /// state the handler resolved itself (the seat registry), never from
    /// anything the caller supplied.
    ///
    /// Everything else is [`Self::create`]: the limits are re-bounded, the
    /// creator must hold every required receive capability, and a
    /// restricted-**sender** endpoint still demands
    /// `CAP_IPC_BIND_PRIVILEGED` — the attestation substitutes only for
    /// the reserved-id half of the gate.
    ///
    /// # Errors
    ///
    /// As [`Self::create`].
    pub fn create_seat_attested<S: Sink + ?Sized>(
        id: EndpointId,
        creator: &TaskCapabilities,
        required_send_caps: CapabilitySet,
        required_recv_caps: CapabilitySet,
        limits: CallEndpointLimits,
        audit: &S,
    ) -> Result<Self, Errno> {
        Self::create_gated(
            id,
            creator,
            required_send_caps,
            required_recv_caps,
            limits,
            true,
            audit,
        )
    }

    /// The one create path both public constructors share.
    /// `reserved_bind_attested` is the handler's kernel-attested
    /// alternative authority for a reserved id (see
    /// [`Self::create_seat_attested`]); it never relaxes the
    /// restricted-sender gate.
    fn create_gated<S: Sink + ?Sized>(
        id: EndpointId,
        creator: &TaskCapabilities,
        required_send_caps: CapabilitySet,
        required_recv_caps: CapabilitySet,
        limits: CallEndpointLimits,
        reserved_bind_attested: bool,
        audit: &S,
    ) -> Result<Self, Errno> {
        let CallEndpointLimits {
            max_request,
            max_reply,
            capacity,
        } = limits;
        let mut id_buf = [0u8; 16];
        let id_field = Field {
            key: "endpoint",
            value: tairix_log::FieldValue::Str(format_hex_u64(id.0, &mut id_buf)),
        };

        if max_request > IPC_MESSAGE_MAX_PAYLOAD_LEN
            || max_reply > IPC_MESSAGE_MAX_PAYLOAD_LEN
            || capacity == 0
        {
            record(audit, AuditEvent::CallEndpointCreateDenied, &[id_field]);
            return Err(Errno::LengthOutOfRange);
        }

        if !required_recv_caps.is_subset_of(creator.effective()) {
            record(audit, AuditEvent::CallEndpointCreateDenied, &[id_field]);
            return Err(Errno::PermissionDenied);
        }

        // A restricted-sender endpoint is privileged unconditionally; a
        // reserved id is privileged unless the handler kernel-attested the
        // alternative bind authority (the live seat lease).
        let reserved_unauthorized =
            tairix_abi::ipc::is_reserved_endpoint(id.0) && !reserved_bind_attested;
        if (!required_send_caps.is_empty() || reserved_unauthorized)
            && !creator.has(tairix_abi::CapabilityId::IPC_BIND_PRIVILEGED)
        {
            record(audit, AuditEvent::CallEndpointCreateDenied, &[id_field]);
            return Err(Errno::PermissionDenied);
        }

        if reserved_bind_attested {
            // The seat-attested bind is a distinct security decision:
            // record it distinguishably from a capability-authorised bind.
            record(
                audit,
                AuditEvent::CallEndpointCreated,
                &[
                    id_field,
                    Field {
                        key: "bind",
                        value: tairix_log::FieldValue::Str("seat-lease"),
                    },
                ],
            );
        } else {
            record(audit, AuditEvent::CallEndpointCreated, &[id_field]);
        }

        Ok(Self {
            id,
            required_send_caps,
            required_recv_caps,
            max_request,
            max_reply,
            capacity,
            owner: creator.task().0,
            server_task: AtomicU64::new(0),
            state: AtomicU32::new(state::OPEN),
            inner: tairix_sync::SpinLock::new(Inner {
                next_ticket: 0,
                pending: VecDeque::new(),
                in_service: BTreeMap::new(),
                completed: BTreeMap::new(),
            }),
        })
    }

    /// Endpoint identifier this endpoint was created with.
    #[must_use]
    pub fn id(&self) -> EndpointId {
        self.id
    }

    /// Maximum request payload (bytes) [`post`](Self::post) will accept.
    #[must_use]
    pub fn max_request(&self) -> u32 {
        self.max_request
    }

    /// Maximum reply payload (bytes) [`reply`](Self::reply) will accept.
    #[must_use]
    pub fn max_reply(&self) -> u32 {
        self.max_reply
    }

    /// Capability set required of every caller.
    #[must_use]
    pub fn required_send_caps(&self) -> &CapabilitySet {
        &self.required_send_caps
    }

    /// Capability set required of the binder/server at create time.
    #[must_use]
    pub fn required_recv_caps(&self) -> &CapabilitySet {
        &self.required_recv_caps
    }

    /// Task id that created and serves this endpoint.
    ///
    /// The call-endpoint registry indexes by this so a task's endpoints can
    /// be torn down when it exits.
    #[must_use]
    pub fn owner(&self) -> u64 {
        self.owner
    }

    /// Record the serving task's *scheduler* id so a post can wake exactly
    /// this server (see the `server_task` field). Called by the receive
    /// paths (the `call_recv` syscall handler and the in-kernel store serve
    /// loop) with the kernel-attested current task — never a caller-supplied
    /// value — after the endpoint-ownership gate has already passed, so it
    /// can only ever name the endpoint's own server. Idempotent; `0` is
    /// never a valid scheduler id and is rejected so the "unrecorded"
    /// sentinel cannot be forged back in.
    pub fn record_server_task(&self, sched_task: u64) {
        if sched_task != 0 {
            self.server_task.store(sched_task, Ordering::Release);
        }
    }

    /// The serving task's recorded *scheduler* id, or [`None`] before the
    /// server's first receive. The post path uses this to wake exactly the
    /// bound server; `None` falls back to the broadcast wake (fail-safe —
    /// a server that never received yet drains its queue on its first
    /// poll regardless).
    #[must_use]
    pub fn server_task(&self) -> Option<u64> {
        match self.server_task.load(Ordering::Acquire) {
            0 => None,
            id => Some(id),
        }
    }

    /// `true` once [`Self::destroy`] has run.
    #[must_use]
    pub fn is_closed(&self) -> bool {
        self.state.load(Ordering::Acquire) == state::CLOSED
    }

    /// Number of outstanding calls (posted, in service, or unclaimed).
    ///
    /// Snapshot only; production paths must not branch on it.
    #[must_use]
    pub fn outstanding(&self) -> usize {
        self.inner.lock().outstanding()
    }

    /// `true` if a posted request is waiting to be received (the readiness a
    /// wait-set member of kind `Endpoint` observes).
    ///
    /// Non-consuming: it peeks the receive queue without dequeuing, so the
    /// subsequent [`recv_call`](Self::recv_call) is what actually takes the
    /// request. A closed endpoint is never ready (its queue was cleared by
    /// [`destroy`](Self::destroy) and stays empty).
    #[must_use]
    pub fn has_pending(&self) -> bool {
        !self.inner.lock().pending.is_empty()
    }

    /// Mark the endpoint closed and cancel every in-flight call.
    ///
    /// Idempotent and fail-closed: subsequent [`post`](Self::post)s return
    /// [`Errno::NotFound`], outstanding [`take_reply`](Self::take_reply)s
    /// observe [`ReplyOutcome::Cancelled`], and any buffered request/reply
    /// bytes are dropped. Records one [`AuditEvent::CallEndpointDestroyed`].
    pub fn destroy<S: Sink + ?Sized>(&self, audit: &S) {
        self.state.store(state::CLOSED, Ordering::Release);
        let cancelled = {
            let mut g = self.inner.lock();
            let n = g.outstanding();
            g.pending.clear();
            g.in_service.clear();
            g.completed.clear();
            n
        };
        let mut id_buf = [0u8; 16];
        let mut n_buf = [0u8; 12];
        record(
            audit,
            AuditEvent::CallEndpointDestroyed,
            &[
                Field {
                    key: "endpoint",
                    value: tairix_log::FieldValue::Str(format_hex_u64(self.id.0, &mut id_buf)),
                },
                Field {
                    key: "cancelled",
                    value: tairix_log::FieldValue::Str(format_usize(cancelled, &mut n_buf)),
                },
            ],
        );
    }

    /// Cancel every in-flight call posted by the task `sender`, which has
    /// exited: queued requests are dropped before service, in-service
    /// tickets are retired (a later [`reply`](Self::reply) for one is
    /// refused fail-closed, its bytes going nowhere), and unclaimed
    /// replies are discarded. Returns the number of calls cancelled.
    ///
    /// Without this, a dead caller's queued request would later be served
    /// on its ticket — the observed Pi 4 defect: an unloaded USB class
    /// driver's final request survived its death, was held by the
    /// host-controller driver after a fault recovery, and wedged the
    /// single-URB transport against the replacement driver. Records one
    /// [`AuditEvent::CallPosterVanished`] when anything was cancelled.
    pub fn cancel_posted_by<S: Sink + ?Sized>(&self, sender: u64, audit: &S) -> usize {
        let cancelled = {
            let mut g = self.inner.lock();
            let before = g.outstanding();
            g.pending.retain(|call| call.sender != sender);
            g.in_service.retain(|_, (owner, _, _, _)| *owner != sender);
            g.completed.retain(|_, (owner, _)| *owner != sender);
            before - g.outstanding()
        };
        if cancelled > 0 {
            let mut id_buf = [0u8; 16];
            let mut sender_buf = [0u8; 16];
            let mut n_buf = [0u8; 12];
            record(
                audit,
                AuditEvent::CallPosterVanished,
                &[
                    Field {
                        key: "endpoint",
                        value: tairix_log::FieldValue::Str(format_hex_u64(self.id.0, &mut id_buf)),
                    },
                    Field {
                        key: "sender",
                        value: tairix_log::FieldValue::Str(format_hex_u64(sender, &mut sender_buf)),
                    },
                    Field {
                        key: "cancelled",
                        value: tairix_log::FieldValue::Str(format_usize(cancelled, &mut n_buf)),
                    },
                ],
            );
        }
        cancelled
    }

    /// Post `request` and obtain the [`CallTicket`] correlating its reply.
    ///
    /// The kernel enforces every check, mirroring [`Port::send`](crate::port::Port::send):
    ///
    /// 1. **Lock-free fast path.** A destroyed endpoint returns
    ///    [`Errno::NotFound`] without taking the lock and records one
    ///    [`AuditEvent::CallPostToClosedEndpoint`].
    /// 2. **Capability check.** Every capability in `required_send_caps` must
    ///    be in `caller.effective()`, else [`Errno::PermissionDenied`] +
    ///    [`AuditEvent::CallPostDenied`].
    /// 3. **Size check.** The request must be `<= max_request` (bounded again
    ///    by [`IPC_MESSAGE_MAX_PAYLOAD_LEN`]), else [`Errno::MessageTooLarge`]
    ///    + [`AuditEvent::CallRequestTooLarge`].
    /// 4. **Capacity check.** If the endpoint already holds `capacity`
    ///    outstanding calls, [`Errno::LengthOutOfRange`] +
    ///    [`AuditEvent::CallQueueFull`].
    ///
    /// On success the request is copied into a kernel-owned buffer, a fresh
    /// ticket is minted, and one [`AuditEvent::CallPosted`] is emitted. The
    /// returned ticket is later surrendered to [`take_reply`](Self::take_reply)
    /// by the *same* caller task.
    ///
    /// `poster_sched` is the posting task's kernel-attested *scheduler* id
    /// (never a caller-supplied value): [`reply`](Self::reply) returns it so
    /// the syscall layer wakes exactly this caller rather than broadcasting
    /// to every parked one. An in-kernel poster with no scheduler identity
    /// passes `0`, and the reply path falls back to the broadcast wake.
    ///
    /// `deadline` is the absolute monotonic time (ns) after which an
    /// unanswered request is reaped as [`ReplyOutcome::TimedOut`] by
    /// [`take_reply`](Self::take_reply); [`u64::MAX`] means no deadline (the
    /// deadline-less `ipc_call` park). The endpoint holds no clock, so the
    /// deadline is an absolute value the caller computes and later compares
    /// against `now`.
    ///
    /// # Errors
    ///
    /// As enumerated above.
    pub fn post<S: Sink + ?Sized>(
        &self,
        caller: &TaskCapabilities,
        poster_sched: u64,
        request: &[u8],
        deadline: u64,
        audit: &S,
    ) -> Result<CallTicket, Errno> {
        let mut id_buf = [0u8; 16];
        let mut sender_buf = [0u8; 16];
        let mut len_buf = [0u8; 12];
        let id_field = Field {
            key: "endpoint",
            value: tairix_log::FieldValue::Str(format_hex_u64(self.id.0, &mut id_buf)),
        };
        let sender_field = Field {
            key: "sender",
            value: tairix_log::FieldValue::Str(format_hex_u64(caller.task().0, &mut sender_buf)),
        };
        let len_field = Field {
            key: "len",
            value: tairix_log::FieldValue::Str(format_usize(request.len(), &mut len_buf)),
        };

        // 1. Fast path: reject posts to a closed endpoint without locking.
        if self.state.load(Ordering::Acquire) == state::CLOSED {
            record(
                audit,
                AuditEvent::CallPostToClosedEndpoint,
                &[id_field, sender_field],
            );
            return Err(Errno::NotFound);
        }

        // 2. Capability check.
        if !self.required_send_caps.is_subset_of(caller.effective()) {
            record(audit, AuditEvent::CallPostDenied, &[id_field, sender_field]);
            return Err(Errno::PermissionDenied);
        }

        // 3. Size check (endpoint-local plus the global ABI cap), computed in
        //    `u64` so a 32-bit target (wasm32) rejects oversize correctly.
        let effective_max = u64::from(self.max_request).min(u64::from(IPC_MESSAGE_MAX_PAYLOAD_LEN));
        if request.len() as u64 > effective_max {
            record(
                audit,
                AuditEvent::CallRequestTooLarge,
                &[id_field, sender_field, len_field],
            );
            return Err(Errno::MessageTooLarge);
        }

        // 4. Enqueue under the lock; re-check destruction after acquiring,
        //    because `destroy()` may have raced between step 1 and here.
        let mut g = self.inner.lock();
        if self.state.load(Ordering::Acquire) == state::CLOSED {
            drop(g);
            record(
                audit,
                AuditEvent::CallPostToClosedEndpoint,
                &[id_field, sender_field],
            );
            return Err(Errno::NotFound);
        }
        if g.outstanding() >= self.capacity {
            drop(g);
            record(audit, AuditEvent::CallQueueFull, &[id_field, sender_field]);
            return Err(Errno::LengthOutOfRange);
        }
        let ticket = g.next_ticket;
        g.next_ticket += 1;
        let sender = caller.task().0;
        let origin = caller.attest_origin();
        g.pending.push_back(PendingCall {
            ticket,
            sender,
            poster_sched,
            origin,
            deadline,
            request: request.to_vec(),
        });
        drop(g);
        record(
            audit,
            AuditEvent::CallPosted,
            &[id_field, sender_field, len_field],
        );
        Ok(CallTicket(ticket))
    }

    /// Dequeue the oldest pending request for the server to service, if it
    /// fits a buffer of `max_copy` bytes.
    ///
    /// Returns [`RecvCall::Empty`] when no request is pending (the server
    /// should park and retry — this never blocks), [`RecvCall::TooLarge`]
    /// when the front request exceeds `max_copy` (left queued so no request
    /// is lost), or [`RecvCall::Received`] with the call
    /// moved into the in-service table keyed by its ticket so a later
    /// [`reply`](Self::reply) can match it. Performs no capability check: the
    /// server's authority is fixed at [`create`](Self::create) time, exactly like [`Port::recv`](crate::port::Port::recv);
    /// the syscall layer gates the caller against
    /// [`required_recv_caps`](Self::required_recv_caps).
    #[must_use]
    pub fn recv_call(&self, max_copy: usize) -> RecvCall {
        let mut g = self.inner.lock();
        let Some(front) = g.pending.front() else {
            return RecvCall::Empty;
        };
        // Refuse to dequeue a request the server's buffer cannot hold: leave
        // it queued and report its size so the server can resize (the kernel
        // maps this to `BufferTooSmall`). Without this the bounded copy would
        // have to drop the request after popping it.
        if front.request.len() > max_copy {
            return RecvCall::TooLarge {
                request_len: front.request.len(),
            };
        }
        let call = g.pending.pop_front().expect("front was present");
        g.in_service.insert(
            call.ticket,
            (call.sender, call.origin, call.poster_sched, call.deadline),
        );
        RecvCall::Received(ReceivedCall {
            ticket: CallTicket(call.ticket),
            sender: call.sender,
            request: call.request,
        })
    }

    /// The kernel-attested [`Origin`] of the caller that posted the
    /// in-service call identified by `ticket`.
    ///
    /// Returns [`None`] unless `ticket` names a call this endpoint has handed
    /// to the server via [`recv_call`](Self::recv_call) and not yet replied —
    /// so a server reads a caller's identity only while actively servicing
    /// that caller's request, never for a pending, completed, or unknown
    /// ticket (fail closed). The origin was snapshotted from the caller's own
    /// kernel state at [`post`](Self::post) time and is unforgeable by the
    /// caller; the syscall layer additionally confirms the reader owns this
    /// endpoint before exposing it.
    #[must_use]
    pub fn peer_origin(&self, ticket: CallTicket) -> Option<Origin> {
        self.inner
            .lock()
            .in_service
            .get(&ticket.0)
            .map(|(_, origin, _, _)| *origin)
    }

    /// Deliver `reply` for the in-service call identified by `ticket`.
    ///
    /// The reply must be `<= max_reply` (bounded again by
    /// [`IPC_MESSAGE_MAX_PAYLOAD_LEN`]) and `ticket` must name a call the
    /// server is currently servicing (received but not yet replied);
    /// otherwise the reply is refused fail-closed and one
    /// [`AuditEvent::CallReplyDenied`] is emitted. On success the reply is
    /// buffered for the caller, one [`AuditEvent::CallReplied`] is emitted,
    /// and the poster's *scheduler* id (as captured by [`post`](Self::post))
    /// is returned so the syscall layer wakes exactly the caller parked on
    /// this ticket — never a broadcast to every parked caller. A `0` means
    /// the poster carried no scheduler identity; the waker then falls back
    /// to the broadcast wake.
    ///
    /// No capability check: the single bound server's authority is fixed at
    /// create time.
    ///
    /// # Errors
    ///
    /// * [`Errno::MessageTooLarge`] if the reply exceeds `max_reply`.
    /// * [`Errno::NotFound`] if `ticket` is not currently in service (unknown,
    ///   already replied, or cancelled by [`destroy`](Self::destroy)).
    pub fn reply<S: Sink + ?Sized>(
        &self,
        ticket: CallTicket,
        reply: &[u8],
        audit: &S,
    ) -> Result<u64, Errno> {
        let mut id_buf = [0u8; 16];
        let mut ticket_buf = [0u8; 16];
        let mut len_buf = [0u8; 12];
        let id_field = Field {
            key: "endpoint",
            value: tairix_log::FieldValue::Str(format_hex_u64(self.id.0, &mut id_buf)),
        };
        let ticket_field = Field {
            key: "ticket",
            value: tairix_log::FieldValue::Str(format_hex_u64(ticket.0, &mut ticket_buf)),
        };
        let len_field = Field {
            key: "len",
            value: tairix_log::FieldValue::Str(format_usize(reply.len(), &mut len_buf)),
        };

        let effective_max = u64::from(self.max_reply).min(u64::from(IPC_MESSAGE_MAX_PAYLOAD_LEN));
        if reply.len() as u64 > effective_max {
            record(
                audit,
                AuditEvent::CallReplyDenied,
                &[id_field, ticket_field, len_field],
            );
            return Err(Errno::MessageTooLarge);
        }

        let mut g = self.inner.lock();
        let Some((sender, _origin, poster_sched, _deadline)) = g.in_service.remove(&ticket.0)
        else {
            drop(g);
            record(
                audit,
                AuditEvent::CallReplyDenied,
                &[id_field, ticket_field],
            );
            return Err(Errno::NotFound);
        };
        g.completed.insert(ticket.0, (sender, reply.to_vec()));
        drop(g);
        record(
            audit,
            AuditEvent::CallReplied,
            &[id_field, ticket_field, len_field],
        );
        Ok(poster_sched)
    }

    /// Claim the reply for `ticket` on behalf of `claimant` (the task that
    /// posted it), as of monotonic time `now` (ns).
    ///
    /// This is the caller's poll step; it never blocks (a
    /// parker loops it under the cooperative yield/park seam). The ticket is
    /// the unforgeable authority, and `claimant` must match the posting task:
    /// a mismatch reports [`ReplyOutcome::Unknown`], never revealing whether
    /// another task's ticket exists.
    ///
    /// * [`ReplyOutcome::Ready`] — the reply is available; its bytes are
    ///   returned and the ticket retired.
    /// * [`ReplyOutcome::TimedOut`] — the ticket's per-post `deadline` has
    ///   passed with no reply; the ticket is retired so a wedged callee fails
    ///   the caller closed rather than parking it forever. (`ipc_call` posts
    ///   with a [`u64::MAX`] deadline, so it never observes this.)
    /// * [`ReplyOutcome::Pending`] — posted/in service, not yet replied, and
    ///   still inside its deadline.
    /// * [`ReplyOutcome::Cancelled`] — the endpoint was destroyed; abandon.
    /// * [`ReplyOutcome::Unknown`] — no such ticket for `claimant`.
    #[must_use]
    pub fn take_reply(&self, claimant: u64, ticket: CallTicket, now: u64) -> ReplyOutcome {
        let mut g = self.inner.lock();
        match g.completed.remove(&ticket.0) {
            // A ready reply, but only its poster may claim it.
            Some((sender, bytes)) if sender == claimant => return ReplyOutcome::Ready(bytes),
            // Someone else's ticket: put the reply back untouched and deny
            // without revealing that it exists.
            Some(entry) => {
                g.completed.insert(ticket.0, entry);
                return ReplyOutcome::Unknown;
            }
            None => {}
        }
        if self.is_closed() {
            return ReplyOutcome::Cancelled;
        }
        // The ticket must belong to `claimant`, whether still pending (never
        // received) or in service (received, awaiting reply). Its deadline
        // decides between a timeout and a still-live wait.
        let deadline = g
            .in_service
            .get(&ticket.0)
            .filter(|(sender, _, _, _)| *sender == claimant)
            .map(|(_, _, _, deadline)| *deadline)
            .or_else(|| {
                g.pending
                    .iter()
                    .find(|c| c.ticket == ticket.0 && c.sender == claimant)
                    .map(|c| c.deadline)
            });
        match deadline {
            Some(deadline) if deadline <= now => {
                // Retire the timed-out ticket from wherever it sits so a late
                // reply for it is refused fail-closed and the slot is freed.
                g.in_service.remove(&ticket.0);
                g.pending.retain(|c| c.ticket != ticket.0);
                ReplyOutcome::TimedOut
            }
            Some(_) => ReplyOutcome::Pending,
            None => ReplyOutcome::Unknown,
        }
    }

    /// Whether a [`take_reply`](Self::take_reply) by `claimant` as of `now`
    /// would make progress — a reply is ready, or one of the claimant's
    /// tickets has timed out, or the endpoint is closed. The non-consuming
    /// peek a [`tairix_abi::WaitSourceKind::CallReply`] wait-set member scans:
    /// the woken owner drains with `take_reply`, never the wait.
    #[must_use]
    pub fn has_ready_reply_for(&self, claimant: u64, now: u64) -> bool {
        // A torn-down endpoint is "ready" so the parked reaper wakes and
        // observes `Cancelled`/`NotFound` rather than parking forever.
        if self.is_closed() {
            return true;
        }
        let g = self.inner.lock();
        if g.completed.values().any(|(sender, _)| *sender == claimant) {
            return true;
        }
        g.pending
            .iter()
            .any(|c| c.sender == claimant && c.deadline <= now)
            || g.in_service
                .values()
                .any(|(sender, _, _, deadline)| *sender == claimant && *deadline <= now)
    }

    /// The nearest (soonest) per-ticket deadline across every outstanding
    /// call `claimant` posted, or [`u64::MAX`] if it has none (or all are
    /// deadline-less). The wait-set layer folds this into its one-shot timer
    /// arming so a wedged callee's deadline wakes the parked reaper.
    #[must_use]
    pub fn next_deadline_for(&self, claimant: u64) -> u64 {
        let g = self.inner.lock();
        let mut nearest = u64::MAX;
        for c in &g.pending {
            if c.sender == claimant {
                nearest = nearest.min(c.deadline);
            }
        }
        for (sender, _, _, deadline) in g.in_service.values() {
            if *sender == claimant {
                nearest = nearest.min(*deadline);
            }
        }
        nearest
    }

    /// Withdraw the single call `ticket` that `claimant` posted, wherever it
    /// sits (pending, in service, or replied-but-unclaimed), returning
    /// whether anything was removed. A per-ticket form of
    /// [`cancel_posted_by`](Self::cancel_posted_by): a caller abandoning a
    /// wedged transfer frees the slot deterministically. Only the ticket's
    /// own poster may cancel it, so a foreign or unknown ticket removes
    /// nothing and returns `false` (no existence oracle).
    #[must_use]
    pub fn cancel_one(&self, claimant: u64, ticket: CallTicket) -> bool {
        let mut g = self.inner.lock();
        let before = g.outstanding();
        g.pending
            .retain(|c| !(c.ticket == ticket.0 && c.sender == claimant));
        if g.in_service
            .get(&ticket.0)
            .is_some_and(|(sender, _, _, _)| *sender == claimant)
        {
            g.in_service.remove(&ticket.0);
        }
        if g.completed
            .get(&ticket.0)
            .is_some_and(|(sender, _)| *sender == claimant)
        {
            g.completed.remove(&ticket.0);
        }
        before != g.outstanding()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::audit::RecordingSink;
    use tairix_abi::CapabilityId;
    use tairix_kernel_sec::captable::TaskId;
    use tairix_kernel_sec::identity::UserId;

    /// The scheduler id the tests post under — arbitrary but non-zero, so
    /// `reply` hands it back verbatim.
    const POSTER_SCHED: u64 = 0x51;

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

    /// An unrestricted endpoint any task may call, with generous bounds.
    fn open_endpoint(sink: &RecordingSink) -> CallEndpoint {
        let creator = task_with(1, &[]);
        CallEndpoint::create(
            EndpointId(0xC),
            &creator,
            CapabilitySet::empty(),
            CapabilitySet::empty(),
            CallEndpointLimits {
                max_request: 128,
                max_reply: 128,
                capacity: 8,
            },
            sink,
        )
        .expect("unrestricted endpoint")
    }

    /// Unbounded receive for the tests: the test buffers are never the
    /// constraint, so map the size-bounded [`CallEndpoint::recv_call`] onto
    /// the simple `Option` the round-trip assertions read.
    fn recv_one(ep: &CallEndpoint) -> Option<ReceivedCall> {
        match ep.recv_call(usize::MAX) {
            RecvCall::Received(call) => Some(call),
            RecvCall::Empty => None,
            RecvCall::TooLarge { .. } => unreachable!("usize::MAX bounds nothing"),
        }
    }

    #[test]
    fn create_rejects_oversize_request_cap() {
        let sink = RecordingSink::new();
        let creator = task_with(1, &[]);
        let err = CallEndpoint::create(
            EndpointId(1),
            &creator,
            CapabilitySet::empty(),
            CapabilitySet::empty(),
            CallEndpointLimits {
                max_request: IPC_MESSAGE_MAX_PAYLOAD_LEN + 1,
                max_reply: 8,
                capacity: 8,
            },
            &sink,
        )
        .err()
        .expect("oversize request cap is refused");
        assert_eq!(err, Errno::LengthOutOfRange);
        assert!(sink
            .ids()
            .contains(&AuditEvent::CallEndpointCreateDenied.id().0));
    }

    #[test]
    fn create_rejects_oversize_reply_cap() {
        let sink = RecordingSink::new();
        let creator = task_with(1, &[]);
        let err = CallEndpoint::create(
            EndpointId(1),
            &creator,
            CapabilitySet::empty(),
            CapabilitySet::empty(),
            CallEndpointLimits {
                max_request: 8,
                max_reply: IPC_MESSAGE_MAX_PAYLOAD_LEN + 1,
                capacity: 8,
            },
            &sink,
        )
        .err()
        .expect("oversize reply cap is refused");
        assert_eq!(err, Errno::LengthOutOfRange);
    }

    #[test]
    fn create_rejects_zero_capacity() {
        let sink = RecordingSink::new();
        let creator = task_with(1, &[]);
        let err = CallEndpoint::create(
            EndpointId(2),
            &creator,
            CapabilitySet::empty(),
            CapabilitySet::empty(),
            CallEndpointLimits {
                max_request: 64,
                max_reply: 64,
                capacity: 0,
            },
            &sink,
        )
        .err()
        .expect("zero capacity is refused");
        assert_eq!(err, Errno::LengthOutOfRange);
    }

    #[test]
    fn create_rejects_reserved_id_squat_without_bind_privilege() {
        let sink = RecordingSink::new();
        let squatter = task_with(1, &[]);
        for id in [
            tairix_abi::sysinfo::SYSINFO_ENDPOINT,
            tairix_abi::log_ingress::LOG_INGRESS_ENDPOINT,
            tairix_abi::elevate::ELEVATE_ENDPOINT_BASE,
            tairix_abi::elevate::ELEVATE_ENDPOINT_BASE
                + u64::from(tairix_abi::process::CONSOLE_INDEX_MAX),
        ] {
            let err = CallEndpoint::create(
                EndpointId(id),
                &squatter,
                CapabilitySet::empty(),
                CapabilitySet::empty(),
                CallEndpointLimits {
                    max_request: 64,
                    max_reply: 64,
                    capacity: 8,
                },
                &sink,
            )
            .err()
            .expect("reserved-id squat is refused");
            assert_eq!(err, Errno::PermissionDenied);
        }
        assert!(sink
            .ids()
            .contains(&AuditEvent::CallEndpointCreateDenied.id().0));
    }

    #[test]
    fn create_allows_reserved_id_with_bind_privilege() {
        let sink = RecordingSink::new();
        let service = task_with(1, &[CapabilityId::IPC_BIND_PRIVILEGED]);
        let ep = CallEndpoint::create(
            EndpointId(tairix_abi::sysinfo::SYSINFO_ENDPOINT),
            &service,
            CapabilitySet::empty(),
            CapabilitySet::empty(),
            CallEndpointLimits {
                max_request: 64,
                max_reply: 64,
                capacity: 8,
            },
            &sink,
        )
        .expect("privileged service binds its reserved rendezvous");
        assert_eq!(ep.id(), EndpointId(tairix_abi::sysinfo::SYSINFO_ENDPOINT));
        assert!(sink.ids().contains(&AuditEvent::CallEndpointCreated.id().0));
    }

    #[test]
    fn create_rejects_recv_caps_not_held_by_creator() {
        let sink = RecordingSink::new();
        let creator = task_with(1, &[]); // holds nothing
        let required_recv = caps_of(&[CapabilityId::AUDIT_READ]);
        let err = CallEndpoint::create(
            EndpointId(3),
            &creator,
            CapabilitySet::empty(),
            required_recv,
            CallEndpointLimits {
                max_request: 64,
                max_reply: 64,
                capacity: 8,
            },
            &sink,
        )
        .err()
        .expect("must not grant unheld authority");
        assert_eq!(err, Errno::PermissionDenied);
        assert!(sink
            .ids()
            .contains(&AuditEvent::CallEndpointCreateDenied.id().0));
    }

    #[test]
    fn create_requires_ipc_bind_privileged_for_restricted_sender() {
        let sink = RecordingSink::new();
        // Holds the send cap but lacks IPC_BIND_PRIVILEGED, so may not bind
        // an endpoint that restricts who may call it.
        let creator = task_with(1, &[CapabilityId::NET_RAW]);
        let err = CallEndpoint::create(
            EndpointId(4),
            &creator,
            caps_of(&[CapabilityId::NET_RAW]),
            CapabilitySet::empty(),
            CallEndpointLimits {
                max_request: 64,
                max_reply: 64,
                capacity: 8,
            },
            &sink,
        )
        .err()
        .expect("privileged bind requires IPC_BIND_PRIVILEGED");
        assert_eq!(err, Errno::PermissionDenied);
    }

    #[test]
    fn create_succeeds_and_exposes_its_parameters() {
        let sink = RecordingSink::new();
        let creator = task_with(
            1,
            &[CapabilityId::IPC_BIND_PRIVILEGED, CapabilityId::NET_RAW],
        );
        let ep = CallEndpoint::create(
            EndpointId(5),
            &creator,
            caps_of(&[CapabilityId::NET_RAW]),
            CapabilitySet::empty(),
            CallEndpointLimits {
                max_request: 100,
                max_reply: 200,
                capacity: 4,
            },
            &sink,
        )
        .expect("authorised");
        assert_eq!(ep.id(), EndpointId(5));
        assert_eq!(ep.max_request(), 100);
        assert_eq!(ep.max_reply(), 200);
        assert!(ep.required_send_caps().contains(CapabilityId::NET_RAW));
        assert!(ep.required_recv_caps().is_empty());
        assert!(!ep.is_closed());
        assert_eq!(ep.outstanding(), 0);
        assert!(sink.ids().contains(&AuditEvent::CallEndpointCreated.id().0));
    }

    #[test]
    fn post_without_required_cap_is_denied_and_audited() {
        let sink = RecordingSink::new();
        let creator = task_with(
            1,
            &[CapabilityId::IPC_BIND_PRIVILEGED, CapabilityId::NET_RAW],
        );
        let ep = CallEndpoint::create(
            EndpointId(0xA),
            &creator,
            caps_of(&[CapabilityId::NET_RAW]),
            CapabilitySet::empty(),
            CallEndpointLimits {
                max_request: 64,
                max_reply: 64,
                capacity: 4,
            },
            &sink,
        )
        .expect("authorised");
        let caller = task_with(7, &[]); // lacks NET_RAW
        let err = ep
            .post(&caller, POSTER_SCHED, b"hi", u64::MAX, &sink)
            .expect_err("denied");
        assert_eq!(err, Errno::PermissionDenied);
        assert!(sink.ids().contains(&AuditEvent::CallPostDenied.id().0));
        assert_eq!(ep.outstanding(), 0);
    }

    #[test]
    fn post_oversize_request_is_too_large() {
        let sink = RecordingSink::new();
        let creator = task_with(1, &[]);
        let ep = CallEndpoint::create(
            EndpointId(0xB),
            &creator,
            CapabilitySet::empty(),
            CapabilitySet::empty(),
            CallEndpointLimits {
                max_request: 4,
                max_reply: 64,
                capacity: 4,
            },
            &sink,
        )
        .expect("ok");
        let caller = task_with(7, &[]);
        let err = ep
            .post(&caller, POSTER_SCHED, b"too many bytes", u64::MAX, &sink)
            .expect_err("oversize");
        assert_eq!(err, Errno::MessageTooLarge);
        assert!(sink.ids().contains(&AuditEvent::CallRequestTooLarge.id().0));
    }

    #[test]
    fn post_to_closed_endpoint_is_not_found() {
        let sink = RecordingSink::new();
        let ep = open_endpoint(&sink);
        ep.destroy(&sink);
        let caller = task_with(7, &[]);
        let err = ep
            .post(&caller, POSTER_SCHED, b"x", u64::MAX, &sink)
            .expect_err("closed");
        assert_eq!(err, Errno::NotFound);
        assert!(sink
            .ids()
            .contains(&AuditEvent::CallPostToClosedEndpoint.id().0));
        assert!(ep.is_closed());
    }

    #[test]
    fn post_beyond_capacity_is_queue_full() {
        let sink = RecordingSink::new();
        let creator = task_with(1, &[]);
        let ep = CallEndpoint::create(
            EndpointId(0xD),
            &creator,
            CapabilitySet::empty(),
            CapabilitySet::empty(),
            CallEndpointLimits {
                max_request: 64,
                max_reply: 64,
                capacity: 2,
            },
            &sink,
        )
        .expect("ok");
        let caller = task_with(7, &[]);
        ep.post(&caller, POSTER_SCHED, b"a", u64::MAX, &sink)
            .expect("1");
        ep.post(&caller, POSTER_SCHED, b"b", u64::MAX, &sink)
            .expect("2");
        let err = ep
            .post(&caller, POSTER_SCHED, b"c", u64::MAX, &sink)
            .expect_err("full");
        assert_eq!(err, Errno::LengthOutOfRange);
        assert!(sink.ids().contains(&AuditEvent::CallQueueFull.id().0));
        assert_eq!(ep.outstanding(), 2);
    }

    #[test]
    fn full_round_trip_post_recv_reply_take() {
        let sink = RecordingSink::new();
        let ep = open_endpoint(&sink);
        let caller = task_with(7, &[]);

        let ticket = ep
            .post(&caller, POSTER_SCHED, b"ping", u64::MAX, &sink)
            .expect("posted");
        // Before the server receives it, the caller sees Pending.
        assert_eq!(ep.take_reply(7, ticket, 0), ReplyOutcome::Pending);

        let received = recv_one(&ep).expect("a pending call");
        assert_eq!(received.ticket, ticket);
        assert_eq!(received.sender, 7);
        assert_eq!(received.request, b"ping");
        // Received but unreplied is still Pending for the caller.
        assert_eq!(ep.take_reply(7, ticket, 0), ReplyOutcome::Pending);

        ep.reply(ticket, b"pong", &sink).expect("replied");
        assert!(sink.ids().contains(&AuditEvent::CallReplied.id().0));

        // The caller claims the reply exactly once.
        assert_eq!(
            ep.take_reply(7, ticket, 0),
            ReplyOutcome::Ready(b"pong".to_vec())
        );
        // A second claim finds nothing.
        assert_eq!(ep.take_reply(7, ticket, 0), ReplyOutcome::Unknown);
        assert_eq!(ep.outstanding(), 0);
    }

    #[test]
    fn peer_origin_reflects_the_in_service_caller_and_fails_closed() {
        use tairix_abi::{ProcId, TrustDomain};
        let sink = RecordingSink::new();
        let ep = open_endpoint(&sink);
        // A caller with a minted process instance and a real capability.
        let mut caller = task_with(7, &[CapabilityId::SYSINFO_GLOBAL]);
        let minted = ProcId::from_raw([0x9E; 16]);
        caller = caller.with_proc_id(minted);

        let ticket = ep
            .post(&caller, POSTER_SCHED, b"ping", u64::MAX, &sink)
            .expect("posted");
        // A pending (not-yet-received) call exposes no origin: the server
        // only learns a caller's identity while actively servicing it.
        assert_eq!(ep.peer_origin(ticket), None);
        // An unknown ticket never resolves.
        assert_eq!(ep.peer_origin(CallTicket(0xDEAD)), None);

        recv_one(&ep).expect("received");
        let origin = ep.peer_origin(ticket).expect("in-service origin");
        assert_eq!(origin.trust_domain(), TrustDomain::User);
        assert_eq!(origin.pid(), 7);
        assert_eq!(origin.proc_id(), minted);
        assert!(origin
            .capabilities()
            .holds_cap(CapabilityId::SYSINFO_GLOBAL));
        // It equals exactly what the caller's own record attests — proving
        // the server reads kernel-attested state, not anything on the wire.
        assert_eq!(origin, caller.attest_origin());

        // Once replied, the call leaves the in-service table and the origin
        // is no longer readable.
        ep.reply(ticket, b"pong", &sink).expect("replied");
        assert_eq!(ep.peer_origin(ticket), None);
    }

    #[test]
    fn recv_call_is_fifo_and_empty_yields_none() {
        let sink = RecordingSink::new();
        let ep = open_endpoint(&sink);
        let caller = task_with(7, &[]);
        assert!(recv_one(&ep).is_none());

        let t1 = ep
            .post(&caller, POSTER_SCHED, b"1", u64::MAX, &sink)
            .expect("1");
        let t2 = ep
            .post(&caller, POSTER_SCHED, b"2", u64::MAX, &sink)
            .expect("2");
        assert_ne!(t1, t2);
        assert_eq!(recv_one(&ep).expect("first").ticket, t1);
        assert_eq!(recv_one(&ep).expect("second").ticket, t2);
        assert!(recv_one(&ep).is_none());
    }

    #[test]
    fn recv_call_too_large_leaves_the_request_queued() {
        let sink = RecordingSink::new();
        let ep = open_endpoint(&sink);
        let caller = task_with(7, &[]);
        let ticket = ep
            .post(&caller, POSTER_SCHED, b"four", u64::MAX, &sink)
            .expect("posted");

        // A buffer too small for the front request reports its size and does
        // not dequeue it (no lost request).
        assert_eq!(ep.recv_call(3), RecvCall::TooLarge { request_len: 4 });
        assert_eq!(ep.outstanding(), 1);

        // A buffer that fits then receives the very same call.
        match ep.recv_call(4) {
            RecvCall::Received(call) => assert_eq!(call.ticket, ticket),
            other => panic!("expected the queued call, got {other:?}"),
        }
    }

    #[test]
    fn owner_reports_the_creating_task() {
        let sink = RecordingSink::new();
        let creator = task_with(0x4242, &[]);
        let ep = CallEndpoint::create(
            EndpointId(0xF),
            &creator,
            CapabilitySet::empty(),
            CapabilitySet::empty(),
            CallEndpointLimits {
                max_request: 16,
                max_reply: 16,
                capacity: 2,
            },
            &sink,
        )
        .expect("created");
        assert_eq!(ep.owner(), 0x4242);
    }

    #[test]
    fn take_reply_for_unknown_ticket_is_unknown() {
        let sink = RecordingSink::new();
        let ep = open_endpoint(&sink);
        assert_eq!(ep.take_reply(7, CallTicket(999), 0), ReplyOutcome::Unknown);
    }

    #[test]
    fn a_reply_is_claimable_only_by_its_poster() {
        let sink = RecordingSink::new();
        let ep = open_endpoint(&sink);
        let caller = task_with(7, &[]);
        let ticket = ep
            .post(&caller, POSTER_SCHED, b"q", u64::MAX, &sink)
            .expect("posted");
        recv_one(&ep).expect("received");
        ep.reply(ticket, b"r", &sink).expect("replied");

        // A different task may not claim it, and learns nothing.
        assert_eq!(ep.take_reply(8, ticket, 0), ReplyOutcome::Unknown);
        // The reply is preserved for its rightful owner.
        assert_eq!(
            ep.take_reply(7, ticket, 0),
            ReplyOutcome::Ready(b"r".to_vec())
        );
    }

    #[test]
    fn pending_and_in_service_are_invisible_to_non_posters() {
        let sink = RecordingSink::new();
        let ep = open_endpoint(&sink);
        let caller = task_with(7, &[]);
        let ticket = ep
            .post(&caller, POSTER_SCHED, b"q", u64::MAX, &sink)
            .expect("posted");
        // A non-poster polling the ticket while it is pending learns nothing.
        assert_eq!(ep.take_reply(8, ticket, 0), ReplyOutcome::Unknown);
        recv_one(&ep).expect("received");
        assert_eq!(ep.take_reply(8, ticket, 0), ReplyOutcome::Unknown);
    }

    #[test]
    fn reply_to_unknown_ticket_is_denied() {
        let sink = RecordingSink::new();
        let ep = open_endpoint(&sink);
        let err = ep
            .reply(CallTicket(42), b"r", &sink)
            .expect_err("unknown ticket");
        assert_eq!(err, Errno::NotFound);
        assert!(sink.ids().contains(&AuditEvent::CallReplyDenied.id().0));
    }

    #[test]
    fn reply_twice_is_denied_the_second_time() {
        let sink = RecordingSink::new();
        let ep = open_endpoint(&sink);
        let caller = task_with(7, &[]);
        let ticket = ep
            .post(&caller, POSTER_SCHED, b"q", u64::MAX, &sink)
            .expect("posted");
        recv_one(&ep).expect("received");
        ep.reply(ticket, b"r", &sink).expect("first reply");
        // The ticket left the in-service table on the first reply.
        let err = ep.reply(ticket, b"r2", &sink).expect_err("second reply");
        assert_eq!(err, Errno::NotFound);
    }

    #[test]
    fn oversize_reply_is_denied_and_leaves_the_call_in_service() {
        let sink = RecordingSink::new();
        let creator = task_with(1, &[]);
        let ep = CallEndpoint::create(
            EndpointId(0xE),
            &creator,
            CapabilitySet::empty(),
            CapabilitySet::empty(),
            CallEndpointLimits {
                max_request: 64,
                max_reply: 4,
                capacity: 4,
            },
            &sink,
        )
        .expect("ok");
        let caller = task_with(7, &[]);
        let ticket = ep
            .post(&caller, POSTER_SCHED, b"q", u64::MAX, &sink)
            .expect("posted");
        recv_one(&ep).expect("received");
        let err = ep
            .reply(ticket, b"too long", &sink)
            .expect_err("oversize reply");
        assert_eq!(err, Errno::MessageTooLarge);
        assert!(sink.ids().contains(&AuditEvent::CallReplyDenied.id().0));
        // The call is still in service: a correctly-sized reply still works.
        ep.reply(ticket, b"ok", &sink).expect("retry");
        assert_eq!(
            ep.take_reply(7, ticket, 0),
            ReplyOutcome::Ready(b"ok".to_vec())
        );
    }

    #[test]
    fn reply_returns_the_posters_scheduler_id() {
        let sink = RecordingSink::new();
        let ep = open_endpoint(&sink);
        let caller = task_with(7, &[]);
        let ticket = ep
            .post(&caller, 0xBEEF, b"q", u64::MAX, &sink)
            .expect("posted");
        recv_one(&ep).expect("received");
        // The reply hands back exactly the scheduler id the post captured,
        // so the syscall layer wakes that one caller and no other.
        assert_eq!(ep.reply(ticket, b"r", &sink), Ok(0xBEEF));
    }

    #[test]
    fn server_task_is_recorded_once_and_zero_is_rejected() {
        let sink = RecordingSink::new();
        let ep = open_endpoint(&sink);
        // Unrecorded until the server's first receive: posts fall back to
        // the broadcast wake.
        assert_eq!(ep.server_task(), None);
        // A zero id is the "unrecorded" sentinel and must not overwrite a
        // real recording (or be recordable at all).
        ep.record_server_task(0);
        assert_eq!(ep.server_task(), None);
        ep.record_server_task(42);
        assert_eq!(ep.server_task(), Some(42));
        // Recording is idempotent for the same server and a later record
        // (a restarted server task) supersedes the old id.
        ep.record_server_task(42);
        ep.record_server_task(43);
        assert_eq!(ep.server_task(), Some(43));
        // Zero still cannot forge the sentinel back in.
        ep.record_server_task(0);
        assert_eq!(ep.server_task(), Some(43));
    }

    #[test]
    fn destroy_cancels_in_flight_calls_and_audits_the_count() {
        let sink = RecordingSink::new();
        let ep = open_endpoint(&sink);
        let caller = task_with(7, &[]);
        // Post three, then drive them into three distinct states: the first
        // received-and-replied (completed), the second received (in service),
        // the third never received (pending).
        let t_done = ep
            .post(&caller, POSTER_SCHED, b"d", u64::MAX, &sink)
            .expect("done");
        let t_in_service = ep
            .post(&caller, POSTER_SCHED, b"s", u64::MAX, &sink)
            .expect("in service");
        let t_pending = ep
            .post(&caller, POSTER_SCHED, b"p", u64::MAX, &sink)
            .expect("pending");
        assert_eq!(recv_one(&ep).expect("first").ticket, t_done);
        assert_eq!(recv_one(&ep).expect("second").ticket, t_in_service);
        ep.reply(t_done, b"r", &sink).expect("reply the first");
        assert_eq!(ep.outstanding(), 3);

        ep.destroy(&sink);

        for t in [t_pending, t_in_service, t_done] {
            assert_eq!(ep.take_reply(7, t, 0), ReplyOutcome::Cancelled);
        }
        assert_eq!(ep.outstanding(), 0);
        assert!(sink
            .ids()
            .contains(&AuditEvent::CallEndpointDestroyed.id().0));
    }

    #[test]
    fn destroy_is_idempotent() {
        let sink = RecordingSink::new();
        let ep = open_endpoint(&sink);
        ep.destroy(&sink);
        ep.destroy(&sink);
        assert!(ep.is_closed());
    }

    #[test]
    fn cancel_posted_by_scrubs_every_state_of_the_dead_poster() {
        // The Pi 4 keyboard-wedge regression: a killed class driver's final
        // queued URB submit survived its death, was later received and held
        // by the HCD, and the replacement driver's every submit was refused
        // as AlreadyExists. Cancellation on the poster's exit must scrub the
        // dead task's calls in all three states.
        let sink = RecordingSink::new();
        let ep = open_endpoint(&sink);
        let caller = task_with(7, &[]);
        let t_done = ep
            .post(&caller, POSTER_SCHED, b"d", u64::MAX, &sink)
            .expect("done");
        let t_in_service = ep
            .post(&caller, POSTER_SCHED, b"s", u64::MAX, &sink)
            .expect("in service");
        let t_pending = ep
            .post(&caller, POSTER_SCHED, b"p", u64::MAX, &sink)
            .expect("pending");
        assert_eq!(recv_one(&ep).expect("first").ticket, t_done);
        assert_eq!(recv_one(&ep).expect("second").ticket, t_in_service);
        ep.reply(t_done, b"r", &sink).expect("reply the first");
        assert_eq!(ep.outstanding(), 3);

        assert_eq!(ep.cancel_posted_by(7, &sink), 3);

        // The pending call is never handed to the server...
        assert!(!ep.has_pending());
        assert!(recv_one(&ep).is_none());
        // ...a reply to the retired in-service ticket is refused fail-closed...
        assert_eq!(
            ep.reply(t_in_service, b"r", &sink).expect_err("cancelled"),
            Errno::NotFound
        );
        // ...and the unclaimed completed reply is discarded.
        assert_eq!(ep.take_reply(7, t_pending, 0), ReplyOutcome::Unknown);
        assert_eq!(ep.take_reply(7, t_done, 0), ReplyOutcome::Unknown);
        assert_eq!(ep.outstanding(), 0);
        assert!(sink.ids().contains(&AuditEvent::CallPosterVanished.id().0));
    }

    #[test]
    fn cancel_posted_by_leaves_other_posters_calls_untouched() {
        let sink = RecordingSink::new();
        let ep = open_endpoint(&sink);
        let dead = task_with(7, &[]);
        let live = task_with(8, &[]);
        ep.post(&dead, POSTER_SCHED, b"dead", u64::MAX, &sink)
            .expect("dead");
        let t_live = ep
            .post(&live, POSTER_SCHED, b"live", u64::MAX, &sink)
            .expect("live");

        assert_eq!(ep.cancel_posted_by(7, &sink), 1);

        // The live poster's call is still delivered and answerable.
        let got = recv_one(&ep).expect("live call survives");
        assert_eq!(got.ticket, t_live);
        assert_eq!(got.sender, 8);
        ep.reply(t_live, b"r", &sink).expect("replied");
        assert_eq!(
            ep.take_reply(8, t_live, 0),
            ReplyOutcome::Ready(b"r".to_vec())
        );
    }

    #[test]
    fn cancel_posted_by_with_nothing_in_flight_is_silent() {
        let sink = RecordingSink::new();
        let ep = open_endpoint(&sink);
        let before = sink.len();
        assert_eq!(ep.cancel_posted_by(7, &sink), 0);
        // No calls cancelled: no audit record is emitted.
        assert_eq!(sink.len(), before);
    }

    #[test]
    fn a_pending_call_times_out_only_once_its_deadline_passes() {
        let sink = RecordingSink::new();
        let ep = open_endpoint(&sink);
        let caller = task_with(7, &[]);
        // A request with an absolute deadline of 100ns.
        let ticket = ep
            .post(&caller, POSTER_SCHED, b"slow", 100, &sink)
            .expect("posted");
        // Before the deadline the caller is still pending, even received.
        assert_eq!(ep.take_reply(7, ticket, 50), ReplyOutcome::Pending);
        recv_one(&ep).expect("received");
        assert_eq!(ep.take_reply(7, ticket, 99), ReplyOutcome::Pending);
        // At/after the deadline with no reply, it fails closed and the ticket
        // is retired so the endpoint slot is freed.
        assert_eq!(ep.take_reply(7, ticket, 100), ReplyOutcome::TimedOut);
        assert_eq!(ep.outstanding(), 0);
        // A late reply for the retired ticket is refused fail-closed.
        assert_eq!(
            ep.reply(ticket, b"late", &sink).expect_err("retired"),
            Errno::NotFound
        );
    }

    #[test]
    fn a_ready_reply_beats_an_elapsed_deadline() {
        let sink = RecordingSink::new();
        let ep = open_endpoint(&sink);
        let caller = task_with(7, &[]);
        let ticket = ep
            .post(&caller, POSTER_SCHED, b"q", 100, &sink)
            .expect("posted");
        recv_one(&ep).expect("received");
        ep.reply(ticket, b"r", &sink).expect("replied");
        // Even well past the deadline, a delivered reply is returned, never a
        // spurious timeout that would discard a completed answer.
        assert_eq!(
            ep.take_reply(7, ticket, 1_000_000),
            ReplyOutcome::Ready(b"r".to_vec())
        );
    }

    #[test]
    fn has_ready_reply_for_is_a_faithful_peek() {
        let sink = RecordingSink::new();
        let ep = open_endpoint(&sink);
        let caller = task_with(7, &[]);
        let ticket = ep
            .post(&caller, POSTER_SCHED, b"q", 100, &sink)
            .expect("posted");
        // Nothing to reap while pending and inside the deadline.
        assert!(!ep.has_ready_reply_for(7, 50));
        // A non-poster never sees the reply as ready.
        assert!(!ep.has_ready_reply_for(8, 50));
        // The elapsed deadline is itself a reap-worthy event (a timeout).
        assert!(ep.has_ready_reply_for(7, 100));
        // A delivered reply is ready regardless of time.
        recv_one(&ep).expect("received");
        ep.reply(ticket, b"r", &sink).expect("replied");
        assert!(ep.has_ready_reply_for(7, 0));
        assert!(!ep.has_ready_reply_for(8, 0));
    }

    #[test]
    fn next_deadline_for_reports_the_soonest_outstanding_deadline() {
        let sink = RecordingSink::new();
        let ep = open_endpoint(&sink);
        let caller = task_with(7, &[]);
        assert_eq!(ep.next_deadline_for(7), u64::MAX);
        ep.post(&caller, POSTER_SCHED, b"a", 300, &sink).expect("a");
        ep.post(&caller, POSTER_SCHED, b"b", 150, &sink).expect("b");
        // Both are still pending; the nearest deadline is reported, and only
        // for the querying poster.
        assert_eq!(ep.next_deadline_for(7), 150);
        assert_eq!(ep.next_deadline_for(8), u64::MAX);
    }

    #[test]
    fn cancel_one_withdraws_only_the_posters_ticket() {
        let sink = RecordingSink::new();
        let ep = open_endpoint(&sink);
        let caller = task_with(7, &[]);
        let ticket = ep
            .post(&caller, POSTER_SCHED, b"q", u64::MAX, &sink)
            .expect("posted");
        // A foreign claimant cancels nothing (no existence oracle).
        assert!(!ep.cancel_one(8, ticket));
        assert_eq!(ep.outstanding(), 1);
        // The rightful poster withdraws it, freeing the slot.
        assert!(ep.cancel_one(7, ticket));
        assert_eq!(ep.outstanding(), 0);
        // A second cancel finds nothing.
        assert!(!ep.cancel_one(7, ticket));
    }
}
