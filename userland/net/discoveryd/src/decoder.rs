//! The decoder: the sandboxed worker that runs the multicast DNS engines.
//!
//! It is the only process that ever parses a byte a peer sent, so it holds
//! nothing: the kernel's sandbox spawn mode brands it capability-empty and
//! confines it to its two pipes. Time and randomness therefore come from the
//! front — every frame that moves time carries `now`, and the engines' CSPRNG
//! is keyed by the front's draw — and the decoder reports the one instant its
//! engines next need time back.
//!
//! One engine per interface the front says is up, so a record learned on one
//! link can never answer for another, and a link that goes down takes
//! everything learned on it. Every question the front asks is asked on every
//! such interface, and each answer crosses back already typed — a browse's
//! instance label, a resolve's target and port — so the front reads a field
//! rather than a record. A record that does not read as its question's form
//! (a browse `PTR` naming an instance of another type) is not an answer to it.

use alloc::vec::Vec;

use tairix_abi::discovery_ipc::{Answer, Change, Entry, ServiceTypeField};
use tairix_abi::net_ipc::IF_NAME_LEN;
use tairix_abi::time::Duration64;
use tairix_hash::HashSeed;
use tairix_net::dns::Name;
use tairix_net::dnssd::{ServiceInstance, ServiceType};
use tairix_net::mdns::{
    Answer as EngineAnswer, AnswerChange, MdnsEngine, QuestionId, RData, Record, Sender,
    MAX_MESSAGE_LEN,
};
use tairix_net::IpAddr;
use tairix_rng::{FastRng, RandU64};
use tairix_sandbox::session::{FrameOut, SessionService, SessionStep};
use tairix_util::fallible;

use crate::wire::{Form, FromDecoder, ToDecoder, MAX_FROM_DECODER};

/// The service a decoder worker serves over its pipes.
#[derive(Default)]
pub struct Decoder {
    configured: Option<Configured>,
    links: Vec<Link>,
    questions: Vec<Question>,
    /// The deadline the front was last told of. It starts as the front's own
    /// belief about a fresh decoder: nothing due.
    reported: Option<u64>,
    /// The latest instant seen, so time never runs backwards.
    now: u64,
}

/// What the front's configuration keyed, and the buffers it paid for.
struct Configured {
    cache: HashSeed,
    rng: FastRng,
    /// Where an engine builds a datagram.
    message: Vec<u8>,
    /// Where a frame to the front is built.
    frame: Vec<u8>,
}

/// One question the front asks.
struct Question {
    id: u32,
    form: Form,
    name: Name,
}

/// One interface's engine, and which of its questions is which of the
/// front's.
struct Link {
    name: [u8; IF_NAME_LEN],
    engine: MdnsEngine,
    asked: Vec<(u32, QuestionId)>,
}

impl Decoder {
    /// A decoder awaiting its configuration.
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// Report the engines' folded deadline when it changed, or always when
    /// `answering` a tick.
    fn report(&mut self, frame: &mut [u8], out: &mut dyn FrameOut, answering: bool) -> bool {
        let deadline = self
            .links
            .iter()
            .filter_map(|link| link.engine.next_deadline())
            .map(|at| at.saturating_total_nanos())
            .min();
        if !answering && self.reported == deadline {
            return true;
        }
        self.reported = deadline;
        send(frame, out, &FromDecoder::Deadline(deadline))
    }
}

/// One CSPRNG word for an engine's jitter.
fn draw(rng: &mut FastRng) -> u32 {
    let mut word = [0u8; 4];
    rng.fill_bytes(&mut word);
    u32::from_le_bytes(word)
}

/// Encode `message` into `frame` and write it, reporting whether the front
/// is still listening.
fn send(frame: &mut [u8], out: &mut dyn FrameOut, message: &FromDecoder<'_>) -> bool {
    message
        .encode(frame)
        .is_some_and(|len| out.frame(&frame[..len]).is_ok())
}

impl SessionService for Decoder {
    fn handle(&mut self, request: &[u8], out: &mut dyn FrameOut) -> SessionStep {
        // The front never sends a frame it did not mean, so one that does not
        // decode, or arrives out of order, ends the session rather than being
        // guessed at.
        let Ok(frame) = ToDecoder::decode(request) else {
            return SessionStep::Finished;
        };
        if let ToDecoder::Configure { cache_key, rng_key } = frame {
            if self.configured.is_some() {
                return SessionStep::Finished;
            }
            let (Some(message), Some(frame)) = (
                fallible::filled(MAX_MESSAGE_LEN, 0u8),
                fallible::filled(MAX_FROM_DECODER, 0u8),
            ) else {
                return SessionStep::Finished;
            };
            self.configured = Some(Configured {
                cache: HashSeed::from_bytes(*cache_key),
                rng: FastRng::from_key(rng_key),
                message,
                frame,
            });
            return SessionStep::Continue;
        }
        let Some(mut configured) = self.configured.take() else {
            return SessionStep::Finished;
        };
        let answering = matches!(frame, ToDecoder::Tick { .. });
        let alive = self.apply(&mut configured, frame, out)
            && self.transmit(&mut configured, out)
            && self.report(&mut configured.frame, out, answering);
        self.configured = Some(configured);
        if alive {
            SessionStep::Continue
        } else {
            SessionStep::Finished
        }
    }
}

impl Decoder {
    /// Carry out one frame, reporting whether the session goes on.
    fn apply(
        &mut self,
        configured: &mut Configured,
        frame: ToDecoder<'_>,
        out: &mut dyn FrameOut,
    ) -> bool {
        match frame {
            ToDecoder::Configure { .. } => false,
            ToDecoder::Datagram {
                now,
                interface,
                source,
                port,
                payload,
            } => {
                self.now = self.now.max(now);
                let sender = Sender {
                    addr: source,
                    port,
                    // The front relays only what the stack found on-link.
                    on_link: true,
                };
                self.on_datagram(configured, interface, sender, payload, out)
            }
            // Time moves; the engines' due work is done below.
            ToDecoder::Tick { now } => {
                self.now = self.now.max(now);
                true
            }
            ToDecoder::Link { now, interface, up } => {
                self.now = self.now.max(now);
                let at = self.links.iter().position(|link| link.name == interface);
                let carried = match (up, at) {
                    (true, None) => self.link_up(configured, interface, out),
                    (false, Some(at)) => {
                        self.links.swap_remove(at);
                        true
                    }
                    // The front tells each link's edge once.
                    _ => false,
                };
                carried
                    && send(
                        &mut configured.frame,
                        out,
                        &FromDecoder::Linked { interface, up },
                    )
            }
            ToDecoder::Ask {
                now,
                question,
                form,
                name,
            } => {
                self.now = self.now.max(now);
                self.on_ask(configured, question, form, name, out)
            }
            ToDecoder::Stop { question } => self.on_stop(question),
            ToDecoder::Replay { question, token } => {
                self.replay(&mut configured.frame, question, token, out)
            }
        }
    }

    /// Hand one relayed datagram to its interface's engine, relaying what it
    /// learns and sending what it answers.
    fn on_datagram(
        &mut self,
        configured: &mut Configured,
        interface: [u8; IF_NAME_LEN],
        sender: Sender,
        payload: &[u8],
        out: &mut dyn FrameOut,
    ) -> bool {
        let Some(link) = self.links.iter_mut().find(|link| link.name == interface) else {
            return false;
        };
        let Configured {
            rng,
            message,
            frame,
            ..
        } = configured;
        let mut alive = true;
        let Link {
            name,
            engine,
            asked,
        } = link;
        let emitted = engine.on_message(
            Duration64::from_nanos(self.now),
            payload,
            sender,
            &mut || draw(rng),
            message,
            &mut |answer| {
                alive &= relay(frame, out, *name, asked, &self.questions, answer, None);
            },
        );
        if let Some(emit) = emitted {
            alive &= send(
                frame,
                out,
                &FromDecoder::Transmit {
                    interface,
                    to: emit.to,
                    payload: &message[..emit.len],
                },
            );
        }
        alive
    }

    /// Take on one question and ask it on every link.
    fn on_ask(
        &mut self,
        configured: &mut Configured,
        question: u32,
        form: Form,
        name: &[u8],
        out: &mut dyn FrameOut,
    ) -> bool {
        let Some(name) = Name::from_wire(name) else {
            return false;
        };
        if self.questions.iter().any(|asked| asked.id == question)
            || self.questions.try_reserve(1).is_err()
        {
            return false;
        }
        self.questions.push(Question {
            id: question,
            form,
            name,
        });
        let asked = self.questions.len() - 1;
        (0..self.links.len()).all(|link| self.ask_on(configured, link, asked, out))
    }

    /// Stop asking one question on every link.
    fn on_stop(&mut self, question: u32) -> bool {
        let Some(at) = self.questions.iter().position(|asked| asked.id == question) else {
            return false;
        };
        self.questions.swap_remove(at);
        for link in &mut self.links {
            if let Some(index) = link.asked.iter().position(|(id, _)| *id == question) {
                let (_, engine_id) = link.asked.swap_remove(index);
                link.engine.stop_asking(engine_id);
            }
        }
        true
    }

    /// Start an engine for `interface` and ask it every question.
    fn link_up(
        &mut self,
        configured: &mut Configured,
        interface: [u8; IF_NAME_LEN],
        out: &mut dyn FrameOut,
    ) -> bool {
        if self.links.try_reserve(1).is_err() {
            return false;
        }
        self.links.push(Link {
            name: interface,
            engine: MdnsEngine::new(configured.cache),
            asked: Vec::new(),
        });
        let link = self.links.len() - 1;
        (0..self.questions.len()).all(|asked| self.ask_on(configured, link, asked, out))
    }

    /// Ask question `asked` on link `link`, relaying what its cache already
    /// holds. An engine that refuses a question the front bounded is not one
    /// the session can go on with.
    fn ask_on(
        &mut self,
        configured: &mut Configured,
        link: usize,
        asked: usize,
        out: &mut dyn FrameOut,
    ) -> bool {
        let Self {
            links,
            questions,
            now,
            ..
        } = self;
        let (Some(link), Some(question)) = (links.get_mut(link), questions.get(asked)) else {
            return false;
        };
        if link.asked.try_reserve(1).is_err() {
            return false;
        }
        let Configured { rng, frame, .. } = configured;
        let mut alive = true;
        let name = link.name;
        let asked_on = link.engine.ask(
            Duration64::from_nanos(*now),
            question.name,
            question.form.record_type(),
            &mut || draw(rng),
            &mut |answer| {
                alive &= relay(frame, out, name, &[], questions, answer, Some(question.id));
            },
        );
        let Ok(engine_id) = asked_on else {
            return false;
        };
        link.asked.push((question.id, engine_id));
        alive
    }

    /// Send what `question` holds on every link under `token`, then the
    /// marker that ends the replay.
    fn replay(
        &mut self,
        frame: &mut [u8],
        question: u32,
        token: u32,
        out: &mut dyn FrameOut,
    ) -> bool {
        if let Some(asked) = self.questions.iter().find(|asked| asked.id == question) {
            for link in &self.links {
                for cached in link
                    .engine
                    .cache()
                    .lookup(&asked.name, asked.form.record_type())
                {
                    let Some(answer) = typed(asked, &cached.record) else {
                        continue;
                    };
                    let entry = Entry::Answer {
                        request: question,
                        interface: link.name,
                        change: Change::Added,
                        ttl: cached.record.ttl,
                        answer,
                    };
                    if !send(frame, out, &FromDecoder::Held { token, entry }) {
                        return false;
                    }
                }
            }
        }
        send(frame, out, &FromDecoder::Replayed { token })
    }

    /// Run every engine's due work at the current instant: expire, refresh,
    /// and send each question that is due.
    fn transmit(&mut self, configured: &mut Configured, out: &mut dyn FrameOut) -> bool {
        let now = Duration64::from_nanos(self.now);
        let Configured {
            rng,
            message,
            frame,
            ..
        } = configured;
        for link in &mut self.links {
            let Link {
                name,
                engine,
                asked,
            } = link;
            let mut alive = true;
            while let Some(emit) = engine.poll(now, &mut || draw(rng), message, &mut |answer| {
                alive &= relay(frame, out, *name, asked, &self.questions, answer, None);
            }) {
                alive &= send(
                    frame,
                    out,
                    &FromDecoder::Transmit {
                        interface: *name,
                        to: emit.to,
                        payload: &message[..emit.len],
                    },
                );
                if !alive {
                    return false;
                }
            }
            if !alive {
                return false;
            }
        }
        true
    }
}

/// Relay one engine answer as the front's question it answers: `during_ask`
/// names the question an engine is being asked, whose engine id is not yet
/// recorded, and otherwise `asked` maps the engine's ids to the front's.
fn relay(
    frame: &mut [u8],
    out: &mut dyn FrameOut,
    interface: [u8; IF_NAME_LEN],
    asked: &[(u32, QuestionId)],
    questions: &[Question],
    answer: &EngineAnswer<'_>,
    during_ask: Option<u32>,
) -> bool {
    let front = during_ask.or_else(|| {
        asked
            .iter()
            .find(|(_, engine_id)| *engine_id == answer.question)
            .map(|(id, _)| *id)
    });
    let Some(question) = front.and_then(|id| questions.iter().find(|asked| asked.id == id)) else {
        return true;
    };
    let Some(typed) = typed(question, &answer.record.record) else {
        return true;
    };
    let change = match answer.change {
        AnswerChange::Added => Change::Added,
        AnswerChange::Refreshed => Change::Refreshed,
        AnswerChange::Retired => Change::Retired,
    };
    send(
        frame,
        out,
        &FromDecoder::Answer(Entry::Answer {
            request: question.id,
            interface,
            change,
            ttl: answer.record.record.ttl,
            answer: typed,
        }),
    )
}

/// What `record` says as an answer to `question`, or `None` when it does not
/// read as the question's form.
fn typed<'r>(question: &Question, record: &'r Record) -> Option<Answer<'r>> {
    Some(match (question.form, &record.data) {
        (Form::Instance, RData::Ptr(target)) => {
            let instance = ServiceInstance::from_name(target).ok()?;
            let parent = instance.service().to_name(instance.domain()).ok()?;
            if parent != question.name {
                return None;
            }
            Answer::Instance {
                label: target.labels().next()?,
            }
        }
        (Form::Service, RData::Srv(service)) => Answer::Service {
            priority: service.priority,
            weight: service.weight,
            port: service.port,
            target: service.target.as_wire(),
        },
        (Form::Text, RData::Txt(text)) => Answer::Text {
            octets: text.as_octets(),
        },
        (Form::AddressV4, RData::A(address)) => Answer::Address {
            address: IpAddr::V4(*address),
        },
        (Form::AddressV6, RData::Aaaa(address)) => Answer::Address {
            address: IpAddr::V6(*address),
        },
        (Form::Pointer, RData::Ptr(target)) => Answer::Pointer {
            target: target.as_wire(),
        },
        (Form::Type, RData::Ptr(target)) => {
            let (service, domain) = ServiceType::from_name(target).ok()?;
            if domain != Name::encode("local").ok()? {
                return None;
            }
            let label = target.labels().next()?;
            Answer::Type {
                service: ServiceTypeField {
                    name: label.get(1..)?,
                    transport: service.transport(),
                },
            }
        }
        _ => return None,
    })
}

#[cfg(test)]
#[path = "decoder_tests.rs"]
mod tests;
