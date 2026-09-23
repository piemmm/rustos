//! Deterministic fuzz harness for the realm protocol's message decoders.
//!
//! Every frame a realm or a client decodes arrived over a network from a peer
//! that is assumed hostile, so `ClientMessage::decode` and
//! `ServerMessage::decode` are the crate's two most attacker-reachable
//! entry points. The invariants are:
//!
//! * decoding any byte string never panics — it yields a message or a typed
//!   refusal;
//! * a message that decodes re-encodes to the *same bytes* and re-decodes
//!   equal, so the encoding is canonical and there is exactly one spelling of
//!   any value;
//! * a message that decodes is within every fixed bound;
//! * every decoded run iterates exactly as many items as it reports, which is
//!   the invariant the sequence's cheap accessors rest on.
//!
//! TAIRiX pulls in no external fuzz runner: a per-run-seeded `Prng` mutates real
//! encoded messages and draws pseudo-random bytes. A plain `cargo test` runs
//! the [`SMOKE_ITERATIONS`] sweep once from a fresh, logged seed; `cargo xtask
//! fuzz` exports `TAIRIX_FUZZ_BUDGET_SECS` to extend the loop to a wall-clock
//! budget.

use tairix_fuzzseed::Prng;
use tairix_wintersun_net::bounds::{
    MAX_ACCOUNT_NAME_LEN, MAX_CHAT_BYTES, MAX_CONSOLE_COMMAND_BYTES, MAX_CONSOLE_REPLY_BYTES,
    MAX_ENTITIES_IN_INTEREST, MAX_GAME_EVENTS, MAX_PASSWORD_LEN, MAX_PLAINTEXT_LEN, MAX_TICK_HZ,
    MAX_WORLD_EDITS,
};
use tairix_wintersun_net::client::{ClientMessage, Credential};
use tairix_wintersun_net::server::ServerMessage;

mod corpus;

/// Fixed-iteration sweep run once by a plain `cargo test` (no budget set).
const SMOKE_ITERATIONS: u64 = 20_000;

/// Decode `bytes` as a client message (must not panic) and, when it decodes,
/// check the canonical-encoding and bound invariants.
pub fn exercise_client(bytes: &[u8]) {
    let Ok(message) = ClientMessage::decode(bytes) else {
        return;
    };

    match &message {
        ClientMessage::Authenticate {
            account,
            credential,
        } => {
            assert!(!account.is_empty() && account.len() <= MAX_ACCOUNT_NAME_LEN);
            assert!(!account.chars().any(char::is_control));
            if let Credential::Password(password) = credential {
                assert!(!password.is_empty() && password.len() <= MAX_PASSWORD_LEN);
            }
        }
        ClientMessage::Chat { target, body, .. } => {
            assert!(!body.is_empty() && body.len() <= MAX_CHAT_BYTES);
            assert!(!body.chars().any(char::is_control));
            if let Some(name) = target {
                assert!(!name.is_empty() && name.len() <= MAX_ACCOUNT_NAME_LEN);
                assert!(!name.chars().any(char::is_control));
            }
        }
        ClientMessage::ConsoleCommand { body } => {
            assert!(!body.is_empty() && body.len() <= MAX_CONSOLE_COMMAND_BYTES);
            assert!(!body.chars().any(char::is_control));
        }
        ClientMessage::SelectCharacter { .. }
        | ClientMessage::Intent(_)
        | ClientMessage::Ping { .. } => {}
    }

    // The encoding is canonical: re-encoding a decoded message reproduces the
    // exact bytes it came from, so no value has a second spelling.
    let mut out = [0u8; MAX_PLAINTEXT_LEN];
    let n = out_len(message.encode(&mut out));
    assert_eq!(&out[..n], bytes, "re-encoding must reproduce the input");
    assert_eq!(
        ClientMessage::decode(&out[..n]).expect("re-decodes"),
        message
    );
}

/// Decode `bytes` as a server message (must not panic) and, when it decodes,
/// check the canonical-encoding and bound invariants.
pub fn exercise_server(bytes: &[u8]) {
    let Ok(message) = ServerMessage::decode(bytes) else {
        return;
    };

    match &message {
        ServerMessage::Welcome(welcome) => {
            assert!(welcome.parameters.tick_hz > 0 && welcome.parameters.tick_hz <= MAX_TICK_HZ);
            assert!(welcome.parameters.day_length_seconds > 0);
        }
        ServerMessage::Snapshot(snapshot) => {
            assert!(snapshot.entities.len() <= MAX_ENTITIES_IN_INTEREST);
            assert_eq!(snapshot.entities.iter().count(), snapshot.entities.len());
        }
        ServerMessage::Delta(delta) => {
            // Entered and updated are both inside the interest set, so the cap
            // bounds their sum rather than each alone.
            assert!(delta.entered.len() + delta.updated.len() <= MAX_ENTITIES_IN_INTEREST);
            assert!(delta.departed.len() <= MAX_ENTITIES_IN_INTEREST);
            assert_eq!(delta.entered.iter().count(), delta.entered.len());
            assert_eq!(delta.updated.iter().count(), delta.updated.len());
            assert_eq!(delta.departed.iter().count(), delta.departed.len());
        }
        ServerMessage::WorldDelta(world) => {
            assert!(world.edits.len() <= MAX_WORLD_EDITS);
            assert_eq!(world.edits.iter().count(), world.edits.len());
        }
        ServerMessage::Events(events) => {
            assert!(events.len() <= MAX_GAME_EVENTS);
            assert_eq!(events.iter().count(), events.len());
        }
        ServerMessage::ChatMessage { sender, body, .. } => {
            assert!(!sender.is_empty() && sender.len() <= MAX_ACCOUNT_NAME_LEN);
            assert!(!body.is_empty() && body.len() <= MAX_CHAT_BYTES);
            assert!(!sender.chars().any(char::is_control));
            assert!(!body.chars().any(char::is_control));
        }
        ServerMessage::ConsoleReply { body, .. } => {
            assert!(body.len() <= MAX_CONSOLE_REPLY_BYTES);
            // A console reply wraps and tabulates; nothing else gets through.
            assert!(!body
                .chars()
                .any(|c| c.is_control() && c != '\n' && c != '\t'));
        }
        ServerMessage::AuthResult(_)
        | ServerMessage::Pong { .. }
        | ServerMessage::Disconnect(_) => {}
    }

    let mut out = [0u8; MAX_PLAINTEXT_LEN];
    let n = out_len(message.encode(&mut out));
    assert_eq!(&out[..n], bytes, "re-encoding must reproduce the input");
    assert_eq!(
        ServerMessage::decode(&out[..n]).expect("re-decodes"),
        message
    );
}

/// A decoded message must always re-encode; a failure here is the defect the
/// harness exists to find, not an input to skip.
fn out_len(encoded: Result<usize, tairix_wintersun_net::WireError>) -> usize {
    encoded.expect("a decoded message must re-encode")
}

/// One round of mutation against one template set.
fn mutate_round(rng: &mut Prng, templates: &[Vec<u8>], exercise: fn(&[u8])) {
    let template = rng.pick(templates);

    // A real frame with a handful of bytes flipped: hammers the kind tag,
    // the discriminants, the counts, and the length prefixes.
    let mut mutated = template.clone();
    let flips = rng.at_most(8);
    for _ in 0..flips {
        if mutated.is_empty() {
            break;
        }
        let pos = rng.below(mutated.len());
        mutated[pos] ^= rng.next_u8();
    }
    exercise(&mutated);

    // A truncation at an arbitrary point.
    let cut = rng.at_most(template.len());
    exercise(&template[..cut]);

    // A real frame with trailing bytes appended, which no encoder produces.
    let mut extended = template.clone();
    let extra = rng.at_most(16);
    extended.extend(corpus::blob(rng, extra));
    exercise(&extended);
}

/// A frame whose kind tag is plausible but whose body is noise: the shape
/// that drives the count and length paths hardest.
fn forged_round(rng: &mut Prng) {
    let kind = u16::from(rng.next_u8() % 12);
    let body_len = rng.at_most(600);
    let body = corpus::blob(rng, body_len);
    let mut forged = Vec::with_capacity(2 + body.len());
    forged.extend_from_slice(&kind.to_le_bytes());
    forged.extend_from_slice(&body);
    exercise_client(&forged);
    exercise_server(&forged);

    let noise_len = rng.at_most(128);
    let noise = corpus::blob(rng, noise_len);
    exercise_client(&noise);
    exercise_server(&noise);
}

#[test]
fn decoding_any_bytes_never_panics_and_round_trips_canonically() {
    let deadline = tairix_fuzzseed::budget_deadline(tairix_fuzzseed::FUZZ_BUDGET_ENV);
    // The seed is drawn and logged by `tairix_fuzzseed`: fresh per run,
    // reproducible from the logged value via `TAIRIX_FUZZ_SEED`.
    let mut rng = corpus::seeded("decoding_any_bytes_never_panics_and_round_trips_canonically");

    let client_templates = corpus::client_frames();
    let server_templates = corpus::server_frames();

    // Every committed corpus entry is replayed first, so a crash once found
    // is re-checked on the bytes it was filed under before any new input.
    for frame in &client_templates {
        exercise_client(frame);
    }
    for frame in &server_templates {
        exercise_server(frame);
    }

    let mut iteration: u64 = 0;
    loop {
        mutate_round(&mut rng, &client_templates, exercise_client);
        mutate_round(&mut rng, &server_templates, exercise_server);
        forged_round(&mut rng);

        iteration += 1;
        if !tairix_fuzzseed::within_budget(deadline) && iteration >= SMOKE_ITERATIONS {
            break;
        }
    }
}
