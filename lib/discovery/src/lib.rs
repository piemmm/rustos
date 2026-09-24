//! TAIRiX link-local service discovery client (`plans/ZEROCONF.md` Z4).
//!
//! A program browses for, resolves, and looks up link-local services through
//! `discoveryd`, never by speaking multicast DNS itself: the network stack
//! reserves the port and the groups to that service, which asks the segment
//! on every client's behalf and admits each request against the caller's
//! kernel-attested identity.
//!
//! * [`Session`] — the pure client over an injected [`Transport`]: open a
//!   session, [`start`](Session::start) typed requests in it,
//!   [`collect`](Session::collect) what has arrived, and
//!   [`stop`](Session::stop) what is no longer wanted. Answers are
//!   incremental — each entry says an answer was added, renewed, or retired,
//!   or that everything held on one interface is void — so a browse that runs
//!   for an hour costs what changes, not what is held.
//! * [`Held`] — one request's current answers, kept from those entries: what
//!   a program that wants "the printers on this network now" reads.
//! * `RtDiscovery` (the `program` feature; documented on a freestanding
//!   target) — the production glue: the endpoint call, and a park on the
//!   session's doorbell that is woken by the service, never polled.
//!
//! Every answer names the interface it was learned on, and every name in one
//! was authored by an unauthenticated peer: structurally valid, never
//! display-safe. Resolution returns an address, never a capability.

#![no_std]
#![forbid(unsafe_code)]

extern crate alloc;

use alloc::vec::Vec;

use tairix_abi::discovery_ipc::{
    decode_id_reply, Answer, Change, CollectReply, DiscoveryRequest, Entry, Query,
    DISCOVERY_MAX_REPLY, DISCOVERY_MAX_REQUEST,
};
use tairix_abi::net_ipc::IF_NAME_LEN;
use tairix_abi::reply::decode_status_reply;
use tairix_abi::Errno;
use tairix_util::fallible;

#[cfg(all(feature = "program", target_os = "none"))]
mod rt;
#[cfg(all(feature = "program", target_os = "none"))]
pub use rt::{host_addresses, reverse_name, RtDiscovery, RtTransport};

/// How a wait for answers ended.
#[derive(Copy, Clone, Debug, Eq, PartialEq)]
pub enum Waited {
    /// The service rang and everything waiting was taken.
    Rung,
    /// The deadline passed first.
    TimedOut,
}

/// One call on the discovery endpoint.
pub trait Transport {
    /// Send `request` and write the reply into `reply`, returning its length.
    ///
    /// # Errors
    ///
    /// The call's refusal; [`Errno::NotFound`] means no service is bound.
    fn call(&mut self, request: &[u8], reply: &mut [u8]) -> Result<usize, Errno>;
}

/// Why a discovery call did not succeed.
#[derive(Copy, Clone, Debug, Eq, PartialEq)]
pub enum DiscoveryError {
    /// No discovery service is running: there is nothing on the link to
    /// learn about, which a caller treats as an empty answer rather than a
    /// failure.
    Unavailable,
    /// The service refused the request, for the reason carried:
    /// [`Errno::PermissionDenied`] when the caller may not ask it,
    /// [`Errno::LimitExceeded`] past a session's bounds, [`Errno::OutOfRange`]
    /// for a name or type multicast DNS does not answer for.
    Refused(Errno),
    /// The call itself failed.
    Transport(Errno),
    /// The reply was not one the service sends.
    Malformed,
}

impl core::fmt::Display for DiscoveryError {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        match self {
            Self::Unavailable => f.write_str("link-local discovery is not running"),
            Self::Refused(Errno::PermissionDenied) => {
                f.write_str("not permitted: this program may not ask that of the link")
            }
            Self::Refused(errno) => write!(f, "the discovery service refused: {errno}"),
            Self::Transport(errno) => {
                write!(f, "the discovery service could not be reached: {errno}")
            }
            Self::Malformed => f.write_str("the discovery service sent a reply it never sends"),
        }
    }
}

/// A session with the discovery service.
///
/// Dropping it closes it; the service also ends it when its owner exits.
pub struct Session<T: Transport> {
    transport: T,
    id: u32,
    request: Vec<u8>,
    reply: Vec<u8>,
    closed: bool,
}

impl<T: Transport> Session<T> {
    /// Open a session whose doorbell rings on `deliver_port`, a port the
    /// caller has bound privately and drains.
    ///
    /// # Errors
    ///
    /// [`DiscoveryError::Unavailable`] when no service is running, or the
    /// service's refusal.
    pub fn open(mut transport: T, deliver_port: u64) -> Result<Self, DiscoveryError> {
        let (Some(mut request), Some(mut reply)) = (
            fallible::filled(DISCOVERY_MAX_REQUEST, 0u8),
            fallible::filled(DISCOVERY_MAX_REPLY, 0u8),
        ) else {
            return Err(DiscoveryError::Transport(Errno::OutOfMemory));
        };
        let len = call(
            &mut transport,
            &DiscoveryRequest::Open { deliver_port },
            &mut request,
            &mut reply,
        )?;
        let id = decode_id_reply(&reply[..len]).map_err(refusal)?;
        Ok(Self {
            transport,
            id,
            request,
            reply,
            closed: false,
        })
    }

    /// The session's id, which its doorbell names.
    #[must_use]
    pub const fn id(&self) -> u32 {
        self.id
    }

    /// Start `query`, returning the request's id; its answers arrive through
    /// [`Self::collect`] until it is stopped.
    ///
    /// # Errors
    ///
    /// The service's refusal, or the call's failure.
    pub fn start(&mut self, query: &Query<'_>) -> Result<u32, DiscoveryError> {
        let len = self.call(&DiscoveryRequest::Start {
            session: self.id,
            query: *query,
        })?;
        decode_id_reply(&self.reply[..len]).map_err(refusal)
    }

    /// Stop request `request`: nothing further is queued for it.
    ///
    /// # Errors
    ///
    /// The service's refusal, or the call's failure.
    pub fn stop(&mut self, request: u32) -> Result<(), DiscoveryError> {
        let len = self.call(&DiscoveryRequest::Stop {
            session: self.id,
            request,
        })?;
        decode_status_reply(&self.reply[..len]).map_err(refusal)
    }

    /// Take what is waiting, handing each entry to `each` in the order the
    /// service queued it. Returns whether more is waiting, in which case the
    /// caller collects again at once rather than waiting for the doorbell.
    /// Never waits.
    ///
    /// # Errors
    ///
    /// [`DiscoveryError::Malformed`] for a reply that does not parse — taken
    /// whole or not at all — or the service's refusal.
    pub fn collect(&mut self, each: &mut dyn FnMut(&Entry<'_>)) -> Result<bool, DiscoveryError> {
        // The whole reply buffer, so one call takes as much as the service
        // sends in one.
        let capacity = u32::try_from(self.reply.len()).map_err(|_| DiscoveryError::Malformed)?;
        let len = self.call(&DiscoveryRequest::Collect {
            session: self.id,
            capacity,
        })?;
        let collected = CollectReply::parse(&self.reply[..len]).map_err(refusal)?;
        for entry in collected.entries() {
            each(&entry);
        }
        Ok(collected.more)
    }

    /// Close the session and every request in it.
    ///
    /// # Errors
    ///
    /// The service's refusal, or the call's failure.
    pub fn close(mut self) -> Result<(), DiscoveryError> {
        self.closed = true;
        let len = self.call(&DiscoveryRequest::Close { session: self.id })?;
        decode_status_reply(&self.reply[..len]).map_err(refusal)
    }

    fn call(&mut self, request: &DiscoveryRequest<'_>) -> Result<usize, DiscoveryError> {
        call(
            &mut self.transport,
            request,
            &mut self.request,
            &mut self.reply,
        )
    }
}

impl<T: Transport> Drop for Session<T> {
    fn drop(&mut self) {
        if !self.closed {
            // Best effort: the service also ends a session with its owner.
            let _ = self.call(&DiscoveryRequest::Close { session: self.id });
        }
    }
}

fn call<T: Transport>(
    transport: &mut T,
    request: &DiscoveryRequest<'_>,
    bytes: &mut [u8],
    reply: &mut [u8],
) -> Result<usize, DiscoveryError> {
    let len = request.encode(bytes).map_err(DiscoveryError::Refused)?;
    match transport.call(&bytes[..len], reply) {
        Ok(len) => Ok(len),
        Err(Errno::NotFound) => Err(DiscoveryError::Unavailable),
        Err(errno) => Err(DiscoveryError::Transport(errno)),
    }
}

/// A reply's refusal, or a reply that is none.
fn refusal(errno: Errno) -> DiscoveryError {
    match errno {
        Errno::BufferTooSmall
        | Errno::BadMagic
        | Errno::LengthOutOfRange
        | Errno::AbiVersionUnsupported => DiscoveryError::Malformed,
        other => DiscoveryError::Refused(other),
    }
}

/// One answer a request holds.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct HeldAnswer {
    /// The interface it was learned on.
    pub interface: [u8; IF_NAME_LEN],
    /// Its lifetime as the peer last stated it, in seconds.
    pub ttl: u32,
    /// What it says.
    pub answer: OwnedAnswer,
}

/// An [`Answer`] with its bytes owned.
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum OwnedAnswer {
    /// A browse found an instance: its label.
    Instance(Vec<u8>),
    /// A resolve found where the instance is reached.
    Service {
        /// Lower is preferred.
        priority: u16,
        /// Share among equal priorities.
        weight: u16,
        /// The port.
        port: u16,
        /// The target host, uncompressed wire form.
        target: Vec<u8>,
    },
    /// A resolve found the instance's `TXT` rdata.
    Text(Vec<u8>),
    /// A host lookup found an address.
    Address(core::net::IpAddr),
    /// A reverse lookup found a name, uncompressed wire form.
    Pointer(Vec<u8>),
    /// A type enumeration found a service type: the name and transport.
    Type(Vec<u8>, tairix_abi::discovery_ipc::Transport),
}

impl OwnedAnswer {
    /// `answer` with its bytes copied, or `None` when memory refuses them.
    fn from_answer(answer: &Answer<'_>) -> Option<Self> {
        let owned = |bytes: &[u8]| -> Option<Vec<u8>> {
            let mut out = Vec::new();
            out.try_reserve_exact(bytes.len()).ok()?;
            out.extend_from_slice(bytes);
            Some(out)
        };
        Some(match *answer {
            Answer::Instance { label } => Self::Instance(owned(label)?),
            Answer::Service {
                priority,
                weight,
                port,
                target,
            } => Self::Service {
                priority,
                weight,
                port,
                target: owned(target)?,
            },
            Answer::Text { octets } => Self::Text(owned(octets)?),
            Answer::Address { address } => Self::Address(address),
            Answer::Pointer { target } => Self::Pointer(owned(target)?),
            Answer::Type { service } => Self::Type(owned(service.name)?, service.transport),
        })
    }

    /// Whether `other` is the same answer: names compare without ASCII
    /// case, as the service holds them.
    fn same(&self, other: &Self) -> bool {
        let folded = |a: &[u8], b: &[u8]| a.eq_ignore_ascii_case(b);
        match (self, other) {
            (Self::Instance(a), Self::Instance(b)) | (Self::Pointer(a), Self::Pointer(b)) => {
                folded(a, b)
            }
            (
                Self::Service {
                    priority,
                    weight,
                    port,
                    target,
                },
                Self::Service {
                    priority: p,
                    weight: w,
                    port: o,
                    target: t,
                },
            ) => priority == p && weight == w && port == o && folded(target, t),
            (Self::Type(a, x), Self::Type(b, y)) => x == y && folded(a, b),
            (a, b) => a == b,
        }
    }
}

/// What applying one entry did to a [`Held`] set.
#[derive(Copy, Clone, Debug, Eq, PartialEq)]
pub enum Applied {
    /// The set changed.
    Changed,
    /// Nothing the set holds moved.
    Unchanged,
    /// The service dropped updates for this request: the set is incomplete
    /// from here, and stopping and starting the request again is the way to
    /// learn what is held now.
    Lost,
}

/// One request's current answers, kept from its entries.
#[derive(Clone, Debug, Default)]
pub struct Held {
    answers: Vec<HeldAnswer>,
    lost: bool,
}

impl Held {
    /// Apply one entry for this request.
    pub fn apply(&mut self, entry: &Entry<'_>) -> Applied {
        match entry {
            Entry::Answer {
                interface,
                change,
                ttl,
                answer,
                ..
            } => {
                let Some(answer) = OwnedAnswer::from_answer(answer) else {
                    self.lost = true;
                    return Applied::Lost;
                };
                let at = self
                    .answers
                    .iter()
                    .position(|held| held.interface == *interface && held.answer.same(&answer));
                match (change, at) {
                    (Change::Added | Change::Refreshed, Some(at)) => {
                        self.answers[at].ttl = *ttl;
                        Applied::Unchanged
                    }
                    (Change::Added | Change::Refreshed, None) => {
                        if self.answers.try_reserve(1).is_err() {
                            self.lost = true;
                            return Applied::Lost;
                        }
                        self.answers.push(HeldAnswer {
                            interface: *interface,
                            ttl: *ttl,
                            answer,
                        });
                        Applied::Changed
                    }
                    (Change::Retired, Some(at)) => {
                        self.answers.remove(at);
                        Applied::Changed
                    }
                    (Change::Retired, None) => Applied::Unchanged,
                }
            }
            Entry::Flush { interface, .. } => {
                let before = self.answers.len();
                self.answers.retain(|held| held.interface != *interface);
                if self.answers.len() == before {
                    Applied::Unchanged
                } else {
                    Applied::Changed
                }
            }
            Entry::Lost { .. } => {
                self.lost = true;
                Applied::Lost
            }
        }
    }

    /// The answers held, in the order they arrived.
    #[must_use]
    pub fn answers(&self) -> &[HeldAnswer] {
        &self.answers
    }

    /// Whether updates were dropped, so the set may be incomplete.
    #[must_use]
    pub const fn is_lost(&self) -> bool {
        self.lost
    }
}

/// The request an entry is for.
#[must_use]
pub const fn request_of(entry: &Entry<'_>) -> u32 {
    match entry {
        Entry::Answer { request, .. } | Entry::Flush { request, .. } | Entry::Lost { request } => {
            *request
        }
    }
}

#[cfg(test)]
mod tests;
