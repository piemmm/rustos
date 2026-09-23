//! The decoder: the sandboxed worker that runs the multicast DNS engines.
//!
//! It is the only process that ever parses a byte a peer sent, so it holds
//! nothing: the kernel's sandbox spawn mode brands it capability-empty and
//! confines it to its two pipes. Time and randomness therefore come from the
//! front — every frame that moves time carries `now`, and the engines' CSPRNG
//! is keyed by the front's draw — and the decoder reports the one instant its
//! engines next need time back.
//!
//! One engine per interface, created by that interface's first datagram, so a
//! record learned on one link can never answer for another.
//!
//! It transmits nothing: nothing publishes and nothing asks yet, so the
//! engines are given no room to build a datagram in.

use alloc::vec::Vec;

use tairix_abi::net_ipc::IF_NAME_LEN;
use tairix_abi::time::Duration64;
use tairix_hash::HashSeed;
use tairix_net::mdns::{MdnsEngine, Sender};
use tairix_rng::{FastRng, RandU64};
use tairix_sandbox::session::{FrameOut, SessionService, SessionStep};

use crate::wire::{FromDecoder, ToDecoder};

/// The service a decoder worker serves over its pipes.
#[derive(Default)]
pub struct Decoder {
    keys: Option<Keys>,
    engines: Vec<Interface>,
    /// The deadline the front was last told of. It starts as the front's own
    /// belief about a fresh decoder: nothing due.
    reported: Option<u64>,
    /// The latest instant seen, so time never runs backwards.
    now: u64,
}

/// What the front's configuration keyed.
struct Keys {
    cache: HashSeed,
    rng: FastRng,
}

/// One interface's engine.
struct Interface {
    name: [u8; IF_NAME_LEN],
    engine: MdnsEngine,
}

impl Decoder {
    /// A decoder awaiting its configuration.
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// The engine for `name`, created on first use; `None` when the
    /// allocator refuses one, which drops the datagram that needed it.
    fn engine_for(
        engines: &mut Vec<Interface>,
        name: [u8; IF_NAME_LEN],
        cache: HashSeed,
    ) -> Option<&mut MdnsEngine> {
        let index = if let Some(index) = engines.iter().position(|entry| entry.name == name) {
            index
        } else {
            engines.try_reserve(1).ok()?;
            engines.push(Interface {
                name,
                engine: MdnsEngine::new(cache),
            });
            engines.len() - 1
        };
        engines.get_mut(index).map(|entry| &mut entry.engine)
    }

    /// Report the engines' folded deadline when it changed, or always when
    /// `answering` a tick.
    fn report(&mut self, out: &mut dyn FrameOut, answering: bool) -> SessionStep {
        let deadline = self
            .engines
            .iter()
            .filter_map(|entry| entry.engine.next_deadline())
            .map(|at| at.saturating_total_nanos())
            .min();
        if !answering && self.reported == deadline {
            return SessionStep::Continue;
        }
        self.reported = deadline;
        match out.frame(&FromDecoder::Deadline(deadline).encode()) {
            Ok(()) => SessionStep::Continue,
            Err(_) => SessionStep::Finished,
        }
    }
}

/// One CSPRNG word for an engine's jitter.
fn draw(rng: &mut FastRng) -> u32 {
    let mut word = [0u8; 4];
    rng.fill_bytes(&mut word);
    u32::from_le_bytes(word)
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
            if self.keys.is_some() {
                return SessionStep::Finished;
            }
            self.keys = Some(Keys {
                cache: HashSeed::from_bytes(*cache_key),
                rng: FastRng::from_key(rng_key),
            });
            return SessionStep::Continue;
        }
        let Some(keys) = self.keys.as_mut() else {
            return SessionStep::Finished;
        };
        match frame {
            ToDecoder::Datagram {
                now,
                interface,
                source,
                port,
                payload,
            } => {
                self.now = self.now.max(now);
                if let Some(engine) = Self::engine_for(&mut self.engines, interface, keys.cache) {
                    // The front relays only what the stack found on-link.
                    let sender = Sender {
                        addr: source,
                        port,
                        on_link: true,
                    };
                    let _ = engine.on_message(
                        Duration64::from_nanos(self.now),
                        payload,
                        sender,
                        &mut || draw(&mut keys.rng),
                        &mut [],
                    );
                }
                self.report(out, false)
            }
            ToDecoder::Tick { now } => {
                self.now = self.now.max(now);
                let now = Duration64::from_nanos(self.now);
                for entry in &mut self.engines {
                    while entry
                        .engine
                        .poll(now, &mut || draw(&mut keys.rng), &mut [])
                        .is_some()
                    {}
                }
                self.report(out, true)
            }
            ToDecoder::Configure { .. } => SessionStep::Finished,
        }
    }
}

#[cfg(test)]
#[path = "decoder_tests.rs"]
mod tests;
