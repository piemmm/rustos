//! Client sessions: who opened each, the requests in it, and the answers
//! queued for it until it collects them.
//!
//! A session is rung once when answers wait and not again until it has
//! collected all of them, so a slow reader costs one doorbell however many
//! answers queue. What a session may hold is bounded: requests per session
//! and sessions per account are fixed containment bounds, and a queue that
//! fills stops taking a request's updates and tells it so, once, after
//! everything queued before the loss.

use alloc::collections::VecDeque;
use alloc::vec::Vec;

use tairix_abi::discovery_ipc::{
    CollectWriter, Entry, COLLECT_MIN_CAPACITY, ENTRY_MAX, REQUESTS_PER_SESSION,
    SESSIONS_PER_ACCOUNT, SESSION_QUEUE_BYTES,
};
use tairix_abi::net_ipc::IF_NAME_LEN;
use tairix_abi::{Errno, ProcId};
use tairix_inline::ArrayVec;

/// Sessions the service holds at once, for every account together — a fixed
/// containment bound on what the service commits to its clients: each holds
/// at most [`SESSION_QUEUE_BYTES`] of answers.
pub const MAX_SESSIONS: usize = 256;

/// Whether a request's updates have been dropped.
#[derive(Copy, Clone, Debug, Eq, PartialEq)]
enum Loss {
    /// None has.
    None,
    /// Some have, and it has not been told.
    Owed,
    /// It has been told; nothing more is queued for it.
    Told,
}

/// One request in a session: the questions it asks.
#[derive(Debug)]
pub struct Request {
    /// Its id within the session.
    pub id: u32,
    /// The questions it is subscribed to.
    pub questions: ArrayVec<u32, 2>,
    loss: Loss,
}

/// One client session.
#[derive(Debug)]
pub struct Session {
    id: u32,
    owner: ProcId,
    uid: u32,
    port: u64,
    requests: Vec<Request>,
    next_request: u32,
    queue: VecDeque<u8>,
    /// A doorbell is outstanding: the client has not yet collected
    /// everything it was rung for.
    rung: bool,
    /// Answers wait and the doorbell could not be posted for want of room in
    /// the client's port.
    ring_owed: bool,
}

impl Session {
    /// The port its doorbell rings on.
    #[must_use]
    pub const fn port(&self) -> u64 {
        self.port
    }
}

/// Every open session.
#[derive(Default)]
pub struct Sessions {
    sessions: Vec<Session>,
    next_id: u32,
}

impl Sessions {
    /// Open a session for `owner`, running as `uid`, ringing on `port`.
    ///
    /// # Errors
    ///
    /// [`Errno::LimitExceeded`] past [`SESSIONS_PER_ACCOUNT`] for the account
    /// or [`MAX_SESSIONS`] in all, [`Errno::OutOfMemory`] when the table
    /// cannot grow.
    pub fn open(&mut self, owner: ProcId, uid: u32, port: u64) -> Result<u32, Errno> {
        let held = self
            .sessions
            .iter()
            .filter(|session| session.uid == uid)
            .count();
        if held >= SESSIONS_PER_ACCOUNT || self.sessions.len() >= MAX_SESSIONS {
            return Err(Errno::LimitExceeded);
        }
        self.sessions
            .try_reserve(1)
            .map_err(|_| Errno::OutOfMemory)?;
        let id = self.next_id;
        self.next_id = self.next_id.wrapping_add(1);
        self.sessions.push(Session {
            id,
            owner,
            uid,
            port,
            requests: Vec::new(),
            next_request: 0,
            queue: VecDeque::new(),
            rung: false,
            ring_owed: false,
        });
        Ok(id)
    }

    /// Whether `owner` holds any session.
    #[must_use]
    pub fn holds_any(&self, owner: ProcId) -> bool {
        self.sessions.iter().any(|session| session.owner == owner)
    }

    /// The session `id`, if `owner` opened it. A session id is not a
    /// capability: another principal naming it is refused as if it did not
    /// exist.
    ///
    /// # Errors
    ///
    /// [`Errno::NotFound`] otherwise.
    pub fn owned(&mut self, id: u32, owner: ProcId) -> Result<&mut Session, Errno> {
        self.sessions
            .iter_mut()
            .find(|session| session.id == id && session.owner == owner)
            .ok_or(Errno::NotFound)
    }

    /// Close session `id` of `owner`, returning its requests.
    ///
    /// # Errors
    ///
    /// [`Errno::NotFound`] as [`Self::owned`].
    pub fn close(&mut self, id: u32, owner: ProcId) -> Result<Vec<Request>, Errno> {
        let at = self
            .sessions
            .iter()
            .position(|session| session.id == id && session.owner == owner)
            .ok_or(Errno::NotFound)?;
        Ok(self.sessions.swap_remove(at).requests)
    }

    /// Close every session `owner` holds, returning each one's id and
    /// requests.
    pub fn close_all(&mut self, owner: ProcId) -> Vec<(u32, Vec<Request>)> {
        let mut closed = Vec::new();
        let mut at = 0;
        while at < self.sessions.len() {
            if self.sessions[at].owner == owner {
                let session = self.sessions.swap_remove(at);
                // Each closure is carried out; an unrecordable one would leave
                // its questions asked for no one, so the list is reserved first.
                if closed.try_reserve(1).is_ok() {
                    closed.push((session.id, session.requests));
                }
            } else {
                at += 1;
            }
        }
        closed
    }

    /// Queue `entry` for request `request` of session `session`, rewritten
    /// to name that request. Returns the port to ring, when the session was
    /// quiet.
    pub fn queue(&mut self, session: u32, request: u32, entry: &Entry<'_>) -> Option<u64> {
        let session = self.sessions.iter_mut().find(|held| held.id == session)?;
        let slot = session
            .requests
            .iter_mut()
            .find(|held| held.id == request)?;
        if slot.loss != Loss::None {
            return None;
        }
        let entry = entry.for_request(request);
        let fits = session.queue.len() + entry.wire_len() <= SESSION_QUEUE_BYTES;
        if !(fits && push_entry(&mut session.queue, &entry)) {
            slot.loss = Loss::Owed;
        }
        session.ring()
    }

    /// Queue a flush of `interface` for request `request` of session
    /// `session`; as [`Self::queue`].
    pub fn flush(
        &mut self,
        session: u32,
        request: u32,
        interface: [u8; IF_NAME_LEN],
    ) -> Option<u64> {
        self.queue(session, request, &Entry::Flush { request, interface })
    }

    /// The sessions whose doorbell is owed, by port.
    pub fn owed_rings(&self) -> impl Iterator<Item = u64> + '_ {
        self.sessions
            .iter()
            .filter(|session| session.ring_owed)
            .map(Session::port)
    }

    /// Record how posting a doorbell to session `id` went.
    pub fn rang(&mut self, id: u32, result: Result<(), Errno>) {
        let Some(session) = self.sessions.iter_mut().find(|held| held.id == id) else {
            return;
        };
        if result == Err(Errno::WouldBlock) {
            session.rung = false;
            session.ring_owed = true;
        } else {
            // Posted, or to a port that is gone and takes nothing — its
            // owner's exit closes the session.
            session.ring_owed = false;
        }
    }

    /// The sessions owed a doorbell, as `(session, port)`.
    #[must_use]
    pub fn take_owed(&mut self) -> Vec<(u32, u64)> {
        let mut owed = Vec::new();
        for session in &mut self.sessions {
            if session.ring_owed && !session.rung && owed.try_reserve(1).is_ok() {
                session.rung = true;
                owed.push((session.id, session.port));
            }
        }
        owed
    }
}

impl Session {
    /// The session's id.
    #[must_use]
    pub const fn id(&self) -> u32 {
        self.id
    }

    /// Mark the session rung, returning its port when it was not already.
    fn ring(&mut self) -> Option<u64> {
        if self.rung || self.ring_owed {
            return None;
        }
        self.rung = true;
        Some(self.port)
    }

    /// Add a request asking `questions`.
    ///
    /// # Errors
    ///
    /// [`Errno::LimitExceeded`] past [`REQUESTS_PER_SESSION`] and
    /// [`Errno::OutOfMemory`] when the table cannot grow.
    pub fn add_request(&mut self, questions: ArrayVec<u32, 2>) -> Result<u32, Errno> {
        if self.requests.len() >= REQUESTS_PER_SESSION {
            return Err(Errno::LimitExceeded);
        }
        self.requests
            .try_reserve(1)
            .map_err(|_| Errno::OutOfMemory)?;
        let id = self.next_request;
        self.next_request = self.next_request.wrapping_add(1);
        self.requests.push(Request {
            id,
            questions,
            loss: Loss::None,
        });
        Ok(id)
    }

    /// Record the questions request `id` asks.
    pub fn set_questions(&mut self, id: u32, questions: ArrayVec<u32, 2>) {
        if let Some(request) = self.requests.iter_mut().find(|request| request.id == id) {
            request.questions = questions;
        }
    }

    /// Whether the session has room for another request.
    #[must_use]
    pub fn has_room(&self) -> bool {
        self.requests.len() < REQUESTS_PER_SESSION
    }

    /// Remove request `id`, returning it.
    ///
    /// # Errors
    ///
    /// [`Errno::NotFound`] when the session holds no such request.
    pub fn remove_request(&mut self, id: u32) -> Result<Request, Errno> {
        let at = self
            .requests
            .iter()
            .position(|request| request.id == id)
            .ok_or(Errno::NotFound)?;
        Ok(self.requests.swap_remove(at))
    }

    /// Take as many queued entries as fit `capacity` into `out`, returning
    /// the reply's length. Never waits: an empty queue is an empty reply.
    ///
    /// # Errors
    ///
    /// [`Errno::OutOfRange`] for a capacity under [`COLLECT_MIN_CAPACITY`],
    /// and [`Errno::BufferTooSmall`] when `out` is shorter than it.
    pub fn collect(&mut self, capacity: usize, out: &mut [u8]) -> Result<usize, Errno> {
        if capacity < COLLECT_MIN_CAPACITY {
            return Err(Errno::OutOfRange);
        }
        let out = out.get_mut(..capacity).ok_or(Errno::BufferTooSmall)?;
        let mut writer = CollectWriter::new(out)?;
        let mut scratch = [0u8; ENTRY_MAX];
        while let Some(len) = peek_len(&self.queue) {
            // Only this service wrote the queue, so every entry is whole and
            // decodes; one that is not is dropped with everything after it.
            let Some(slot) = scratch.get_mut(..len).filter(|_| len <= self.queue.len()) else {
                self.queue.clear();
                break;
            };
            for (byte, queued) in slot.iter_mut().zip(&self.queue) {
                *byte = *queued;
            }
            let Ok(entry) = Entry::decode(slot) else {
                self.queue.clear();
                break;
            };
            if writer.push(&entry).is_err() {
                break;
            }
            self.queue.drain(..len);
        }
        // A loss is told after everything queued before it.
        if self.queue.is_empty() {
            for request in &mut self.requests {
                if request.loss == Loss::Owed
                    && writer
                        .push(&Entry::Lost {
                            request: request.id,
                        })
                        .is_ok()
                {
                    request.loss = Loss::Told;
                }
            }
        }
        let more = !self.queue.is_empty()
            || self
                .requests
                .iter()
                .any(|request| request.loss == Loss::Owed);
        // Rung again only once it has taken everything it was rung for.
        if !more {
            self.rung = false;
        }
        Ok(writer.finish(more))
    }
}

/// Append `entry`'s encoding, reporting whether it fitted.
fn push_entry(queue: &mut VecDeque<u8>, entry: &Entry<'_>) -> bool {
    let mut bytes = [0u8; ENTRY_MAX];
    let Ok(len) = entry.encode(&mut bytes) else {
        return false;
    };
    if queue.try_reserve(len).is_err() {
        return false;
    }
    queue.extend(&bytes[..len]);
    true
}

/// The length of the entry at the front of `queue`.
fn peek_len(queue: &VecDeque<u8>) -> Option<usize> {
    let low = *queue.front()?;
    let high = *queue.get(1)?;
    Some(usize::from(u16::from_le_bytes([low, high])))
}

#[cfg(test)]
#[path = "sessions_tests.rs"]
mod tests;
