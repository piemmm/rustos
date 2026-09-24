//! The questions the front asks through its decoder: each shared by every
//! request asking it, and what each holds on every link.
//!
//! What a question holds is kept as keyed fingerprints of its answers, which
//! is all the front needs to hold the decoder to its word: an answer is added
//! once, renewed or retired only while held, and never past the most records
//! an honest engine could hold for one question on one link. The answers
//! themselves live in the requests' queues and in the decoder's cache, so a
//! request that joins a question already asked is brought up to date by a
//! replay from the decoder rather than from a second copy here.

use alloc::vec::Vec;
use core::hash::Hasher;

use tairix_abi::discovery_ipc::{Answer, Change, Entry};
use tairix_abi::net_ipc::{address_parts, IF_NAME_LEN};
use tairix_abi::Errno;
use tairix_hash::{HashSeed, SipHash13};
use tairix_net::dns::Name;
use tairix_net::mdns::{MAX_QUESTIONS, MAX_RECORDS};

use crate::wire::Form;

/// Where one request's view of a question stands.
#[derive(Copy, Clone, Debug, Eq, PartialEq)]
enum Replay {
    /// It is told every edge as it arrives.
    Live,
    /// It joined after the decoder was asked, and is owed what the question
    /// already holds; the replay has not been sent.
    Wanted(u32),
    /// The replay was sent. Until it ends, a live edge is already reflected
    /// in what it will be told, so it is told nothing else.
    Sent(u32),
}

/// One request asking a question.
#[derive(Copy, Clone, Debug)]
struct Subscriber {
    session: u32,
    request: u32,
    replay: Replay,
}

/// What a question holds on one link.
struct Held {
    interface: [u8; IF_NAME_LEN],
    /// Fingerprints, sorted.
    answers: Vec<u64>,
}

/// One question.
struct Question {
    id: u32,
    form: Form,
    name: Name,
    subscribers: Vec<Subscriber>,
    held: Vec<Held>,
    /// Whether the current decoder has been asked it.
    asked: bool,
}

/// What the front does with one answer from its decoder.
#[derive(Debug, Eq, PartialEq)]
pub enum Verdict {
    /// Tell these requests.
    Tell(Vec<(u32, u32)>),
    /// Nothing to tell: it crossed a stop, a flush, or a link edge in flight.
    Stale,
    /// No honest decoder sends it.
    Lie,
}

/// Every question the front asks.
pub struct Questions {
    table: Vec<Question>,
    /// Questions the current decoder is asking that no request wants.
    stopping: Vec<u32>,
    /// Every id below this one has been issued, once.
    next_id: u32,
    next_token: u32,
    key: HashSeed,
}

impl Questions {
    /// An empty table fingerprinting under `key`.
    #[must_use]
    pub fn new(key: HashSeed) -> Self {
        Self {
            table: Vec::new(),
            stopping: Vec::new(),
            next_id: 0,
            next_token: 0,
            key,
        }
    }

    /// Add request `(session, request)` to the question of `form` about
    /// `name`, asking it if no request yet does, and return its id.
    ///
    /// # Errors
    ///
    /// [`Errno::LimitExceeded`] when a new question would pass the most an
    /// engine asks at once, and [`Errno::OutOfMemory`] when a table cannot
    /// grow.
    pub fn subscribe(
        &mut self,
        form: Form,
        name: Name,
        session: u32,
        request: u32,
    ) -> Result<u32, Errno> {
        if let Some(question) = self
            .table
            .iter_mut()
            .find(|question| question.form == form && question.name == name)
        {
            question
                .subscribers
                .try_reserve(1)
                .map_err(|_| Errno::OutOfMemory)?;
            let replay = if question.asked {
                let token = self.next_token;
                self.next_token = self.next_token.wrapping_add(1);
                Replay::Wanted(token)
            } else {
                Replay::Live
            };
            question.subscribers.push(Subscriber {
                session,
                request,
                replay,
            });
            return Ok(question.id);
        }
        if self.table.len() >= MAX_QUESTIONS {
            return Err(Errno::LimitExceeded);
        }
        let mut subscribers = Vec::new();
        subscribers.try_reserve(1).map_err(|_| Errno::OutOfMemory)?;
        self.table.try_reserve(1).map_err(|_| Errno::OutOfMemory)?;
        let id = self.next_id;
        self.next_id = self.next_id.checked_add(1).ok_or(Errno::LimitExceeded)?;
        subscribers.push(Subscriber {
            session,
            request,
            replay: Replay::Live,
        });
        self.table.push(Question {
            id,
            form,
            name,
            subscribers,
            held: Vec::new(),
            asked: false,
        });
        Ok(id)
    }

    /// Take request `(session, request)` off question `id`, retiring the
    /// question when no request is left asking it.
    pub fn unsubscribe(&mut self, id: u32, session: u32, request: u32) {
        let Some(at) = self.table.iter().position(|question| question.id == id) else {
            return;
        };
        let question = &mut self.table[at];
        question
            .subscribers
            .retain(|subscriber| !(subscriber.session == session && subscriber.request == request));
        if !question.subscribers.is_empty() {
            return;
        }
        let question = self.table.swap_remove(at);
        // A stop that cannot be recorded leaves the decoder asking on behalf
        // of no one until it is replaced; nothing is told for it meanwhile.
        if question.asked && self.stopping.try_reserve(1).is_ok() {
            self.stopping.push(question.id);
        }
    }

    /// The next question the decoder should stop asking, if any.
    #[must_use]
    pub fn next_stop(&self) -> Option<u32> {
        self.stopping.first().copied()
    }

    /// The decoder was told to stop `id`.
    pub fn stopped(&mut self, id: u32) {
        self.stopping.retain(|stopping| *stopping != id);
    }

    /// The next question the decoder should be asked, if any.
    #[must_use]
    pub fn next_ask(&self) -> Option<(u32, Form, Name)> {
        self.table
            .iter()
            .find(|question| !question.asked)
            .map(|question| (question.id, question.form, question.name))
    }

    /// The decoder was asked `id`.
    pub fn asked(&mut self, id: u32) {
        if let Some(question) = self.table.iter_mut().find(|question| question.id == id) {
            question.asked = true;
        }
    }

    /// The next replay the decoder should be asked for, if any.
    #[must_use]
    pub fn next_replay(&self) -> Option<(u32, u32)> {
        self.table.iter().find_map(|question| {
            question
                .subscribers
                .iter()
                .find_map(|subscriber| match subscriber.replay {
                    Replay::Wanted(token) => Some((question.id, token)),
                    _ => None,
                })
        })
    }

    /// The replay `token` was sent.
    pub fn replay_sent(&mut self, token: u32) {
        if let Some(subscriber) = self.subscriber_mut(token) {
            subscriber.replay = Replay::Sent(token);
        }
    }

    /// The replay `token` has ended: from here its request is told every edge.
    pub fn replayed(&mut self, token: u32) {
        if let Some(subscriber) = self.subscriber_mut(token) {
            subscriber.replay = Replay::Live;
        }
    }

    fn subscriber_mut(&mut self, token: u32) -> Option<&mut Subscriber> {
        self.table
            .iter_mut()
            .flat_map(|question| question.subscribers.iter_mut())
            .find(|subscriber| {
                matches!(subscriber.replay, Replay::Wanted(t) | Replay::Sent(t) if t == token)
            })
    }

    /// Judge one answer the decoder sent. `linked` says whether the decoder
    /// has carried out the edge that brought `interface` up and not been told
    /// it went down since.
    pub fn on_answer(&mut self, entry: &Entry<'_>, linked: bool) -> Verdict {
        let Entry::Answer {
            request,
            interface,
            change,
            answer,
            ..
        } = entry
        else {
            return Verdict::Lie;
        };
        if *request >= self.next_id {
            return Verdict::Lie;
        }
        let key = self.key;
        let Some(question) = self
            .table
            .iter_mut()
            .find(|question| question.id == *request)
        else {
            return Verdict::Stale;
        };
        if !linked || !question.asked {
            return Verdict::Stale;
        }
        let print = fingerprint(key, answer);
        let at = if let Some(at) = question
            .held
            .iter()
            .position(|held| held.interface == *interface)
        {
            at
        } else {
            if *change != Change::Added || question.held.try_reserve(1).is_err() {
                return Verdict::Stale;
            }
            question.held.push(Held {
                interface: *interface,
                answers: Vec::new(),
            });
            question.held.len() - 1
        };
        let held = &mut question.held[at];
        match (change, held.answers.binary_search(&print)) {
            (Change::Added, Err(at)) => {
                if held.answers.len() >= MAX_RECORDS {
                    return Verdict::Lie;
                }
                if held.answers.try_reserve(1).is_err() {
                    return Verdict::Stale;
                }
                held.answers.insert(at, print);
            }
            (Change::Refreshed, Ok(_)) => {}
            (Change::Retired, Ok(at)) => {
                held.answers.remove(at);
            }
            // A duplicate addition, or a renewal or retirement of what is not
            // held: an edge that crossed a flush the front made.
            _ => return Verdict::Stale,
        }
        let tell = question
            .subscribers
            .iter()
            .filter(|subscriber| subscriber.replay == Replay::Live)
            .map(|subscriber| (subscriber.session, subscriber.request));
        collect(tell)
    }

    /// Judge one replayed answer for the replay `token`: only a request still
    /// owed it is told, and only what the question holds.
    #[must_use]
    pub fn on_held(&self, token: u32, entry: &Entry<'_>) -> Verdict {
        let Entry::Answer {
            request,
            interface,
            answer,
            ..
        } = entry
        else {
            return Verdict::Lie;
        };
        let Some(question) = self.table.iter().find(|question| question.id == *request) else {
            return Verdict::Stale;
        };
        let print = fingerprint(self.key, answer);
        let holds = question
            .held
            .iter()
            .any(|held| held.interface == *interface && held.answers.binary_search(&print).is_ok());
        let owed = question
            .subscribers
            .iter()
            .find(|subscriber| subscriber.replay == Replay::Sent(token));
        match (holds, owed) {
            (true, Some(subscriber)) => {
                collect(core::iter::once((subscriber.session, subscriber.request)))
            }
            _ => Verdict::Stale,
        }
    }

    /// Forget everything held on `interface`, returning each request that
    /// held something there.
    pub fn flush_link(&mut self, interface: [u8; IF_NAME_LEN]) -> Vec<(u32, u32)> {
        let mut flushed = Vec::new();
        for question in &mut self.table {
            let Some(at) = question
                .held
                .iter()
                .position(|held| held.interface == interface)
            else {
                continue;
            };
            let held = question.held.swap_remove(at);
            if held.answers.is_empty() {
                continue;
            }
            for subscriber in &question.subscribers {
                note(&mut flushed, (subscriber.session, subscriber.request));
            }
        }
        flushed
    }

    /// A new decoder knows nothing: every question is to be asked again,
    /// every replay is void — the new decoder tells every request everything
    /// afresh — and everything held is forgotten. Returns each request and
    /// link that held something.
    pub fn forget_decoder(&mut self) -> Vec<(u32, u32, [u8; IF_NAME_LEN])> {
        self.stopping.clear();
        let mut flushed = Vec::new();
        for question in &mut self.table {
            question.asked = false;
            for subscriber in &mut question.subscribers {
                subscriber.replay = Replay::Live;
            }
            for held in question.held.drain(..) {
                if held.answers.is_empty() {
                    continue;
                }
                for subscriber in &question.subscribers {
                    let flush = (subscriber.session, subscriber.request, held.interface);
                    if !flushed.contains(&flush) && flushed.try_reserve(1).is_ok() {
                        flushed.push(flush);
                    }
                }
            }
        }
        flushed
    }

    /// Whether any question is asked at all.
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.table.is_empty()
    }
}

/// The requests to tell, each once.
fn collect(requests: impl Iterator<Item = (u32, u32)>) -> Verdict {
    let mut tell = Vec::new();
    for request in requests {
        note(&mut tell, request);
    }
    Verdict::Tell(tell)
}

fn note(list: &mut Vec<(u32, u32)>, request: (u32, u32)) {
    if !list.contains(&request) && list.try_reserve(1).is_ok() {
        list.push(request);
    }
}

/// The identity of one answer, keyed so a peer cannot choose two answers that
/// collide. Names compare without ASCII case, as the cache holds them.
fn fingerprint(key: HashSeed, answer: &Answer<'_>) -> u64 {
    let mut hash = SipHash13::new(key);
    let folded = |hash: &mut SipHash13, bytes: &[u8]| {
        for &byte in bytes {
            hash.write_u8(byte.to_ascii_lowercase());
        }
    };
    match *answer {
        Answer::Instance { label } => {
            hash.write_u8(1);
            folded(&mut hash, label);
        }
        Answer::Service {
            priority,
            weight,
            port,
            target,
        } => {
            hash.write_u8(2);
            hash.write_u16(priority);
            hash.write_u16(weight);
            hash.write_u16(port);
            folded(&mut hash, target);
        }
        Answer::Text { octets } => {
            hash.write_u8(3);
            hash.write(octets);
        }
        Answer::Address { address } => {
            let (family, octets) = address_parts(address);
            hash.write_u8(4);
            hash.write_u8(family.as_u8());
            hash.write(&octets);
        }
        Answer::Pointer { target } => {
            hash.write_u8(5);
            folded(&mut hash, target);
        }
        Answer::Type { service } => {
            hash.write_u8(6);
            hash.write_u8(service.transport.as_u8());
            folded(&mut hash, service.name);
        }
    }
    hash.finish()
}

#[cfg(test)]
#[path = "questions_tests.rs"]
mod tests;
