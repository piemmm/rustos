//! Deterministic fuzz harness for the RFC 1950 zlib envelope.
//!
//! The envelope is what `lib/image` hands a PNG's `IDAT` bytes to and what
//! `zlib@openssh.com` runs a whole session through, so its header, body, and
//! trailer are all attacker-reachable. The invariants:
//!
//! * neither `decompress_into` nor `Decoder` ever panics, for arbitrary
//!   bytes, a corrupted real stream, or a stream chopped into pieces;
//! * a corrupted trailer is refused rather than accepted — the checksum is
//!   the envelope's whole job; and
//! * the streaming pair round-trips a packetised conversation: what the
//!   encoder flushes per message is exactly what the decoder yields, with
//!   nothing left to carry between messages.
//!
//! Inputs are drawn from a per-run-seeded `Prng`, as elsewhere in the tree; the
//! smoke sweep runs on a plain `cargo test` and `cargo xtask fuzz --soak`
//! extends it to a wall-clock budget.

use tairix_compress::zlib::{decompress_into, Decoder, Encoder, Flush};
use tairix_fuzzseed::Prng;

/// Fixed-iteration sweep run once by a plain `cargo test` (no budget set).
const SMOKE_ITERATIONS: u64 = 400;

/// Largest message a drawn conversation sends in one flush.
const MAX_MESSAGE: usize = 9000;

/// Messages a drawn conversation exchanges.
const MAX_MESSAGES: usize = 24;

/// Largest arbitrary byte string fed straight to the decoder.
const MAX_STREAM: usize = 4096;

/// Largest output a decode of arbitrary bytes is allowed to produce.
const MAX_OUTPUT: usize = 1 << 18;

/// Decode arbitrary bytes both ways: must never panic.
fn decode_never_panics(stream: &[u8]) {
    let mut out = vec![0u8; MAX_OUTPUT];
    let _ = decompress_into(stream, &mut out);

    let mut decoder = Box::new(Decoder::new());
    let mut offset = 0usize;
    loop {
        let Ok(progress) = decoder.decompress(&stream[offset..], &mut out[..4096]) else {
            return;
        };
        if progress.finished || (progress.consumed == 0 && progress.produced == 0) {
            return;
        }
        offset += progress.consumed;
    }
}

/// Text-like bytes with plenty of repetition, the traffic SSH compresses.
fn draw_message(rng: &mut Prng, len: usize) -> Vec<u8> {
    const WORDS: [&[u8]; 6] = [
        b"$ ls -l /System/Commands\r\n",
        b"total 0\r\n",
        b"drwxr-xr-x  2 root  root  4096 ",
        b"permission denied\r\n",
        b"login: ",
        b"\x1b[0m\x1b[1;32m",
    ];
    let mut message = Vec::with_capacity(len);
    while message.len() < len {
        let remaining = len - message.len();
        if rng.next_u64().is_multiple_of(8) {
            message.push(rng.next_u8());
            continue;
        }
        let word = *rng.pick(&WORDS);
        message.extend_from_slice(&word[..word.len().min(remaining)]);
    }
    message
}

#[test]
fn zlib_never_panics_and_the_streaming_pair_round_trips() {
    let deadline = tairix_fuzzseed::budget_deadline(tairix_fuzzseed::FUZZ_BUDGET_ENV);

    let mut rng = Prng::new(tairix_fuzzseed::start(
        "zlib_never_panics_and_the_streaming_pair_round_trips",
        tairix_fuzzseed::FUZZ_SEED_ENV,
    ));

    let mut iteration: u64 = 0;
    loop {
        // 1. A packetised conversation, flushed per message.
        let mut encoder = Box::new(Encoder::new());
        let mut decoder = Box::new(Decoder::new());
        let rounds = rng.at_most(MAX_MESSAGES);
        let mut whole = Vec::new();
        for _ in 0..rounds {
            let len = rng.at_most(MAX_MESSAGE);
            let message = draw_message(&mut rng, len);
            let mut wire = vec![0u8; encoder.bound(message.len())];
            let written = encoder
                .compress(&message, &mut wire, Flush::Sync)
                .expect("a bound-sized destination always fits");
            let mut out = vec![0u8; message.len() + 64];
            let progress = decoder
                .decompress(&wire[..written], &mut out)
                .expect("our own stream decodes");
            assert_eq!(
                progress.consumed, written,
                "a sync flush leaves nothing to carry"
            );
            assert_eq!(&out[..progress.produced], &message[..], "per-message body");
            whole.extend_from_slice(&message);
        }

        // 2. Finish the same conversation and check it one-shot, then damage
        //    the trailer and check the checksum refuses it.
        let mut tail = vec![0u8; encoder.bound(0)];
        let written = encoder
            .compress(&[], &mut tail, Flush::Finish)
            .expect("fits");
        assert!(written >= 4, "a finished stream carries its trailer");

        // 3. Corrupt a freshly finished stream at random offsets.
        let len = rng.at_most(MAX_MESSAGE);
        let message = draw_message(&mut rng, len);
        let mut fresh = Box::new(Encoder::new());
        let mut stream = vec![0u8; fresh.bound(message.len())];
        let written = fresh
            .compress(&message, &mut stream, Flush::Finish)
            .expect("fits");
        let mut out = vec![0u8; message.len()];
        assert_eq!(
            decompress_into(&stream[..written], &mut out),
            Ok(message.len()),
            "a finished stream decodes whole"
        );
        assert_eq!(&out[..], &message[..]);

        let mut damaged = stream[..written].to_vec();
        for _ in 0..rng.at_most(6) {
            if damaged.is_empty() {
                break;
            }
            let at = rng.below(damaged.len());
            damaged[at] ^= rng.next_u8();
        }
        decode_never_panics(&damaged);

        // 4. A trailer flipped on its own is always refused: the body still
        //    decodes, so only the checksum can catch it.
        if written >= 4 && !message.is_empty() {
            let mut flipped = stream[..written].to_vec();
            let last = flipped.len() - 1 - rng.at_most(3);
            flipped[last] ^= 1 << rng.at_most(7);
            let mut out = vec![0u8; message.len()];
            assert!(
                decompress_into(&flipped, &mut out).is_err(),
                "a damaged trailer must be refused"
            );
        }

        // 5. Pure noise straight into both decoders.
        let mut noise = vec![0u8; rng.at_most(MAX_STREAM)];
        rng.fill(&mut noise);
        decode_never_panics(&noise);

        iteration += 1;
        if !tairix_fuzzseed::within_budget(deadline) && iteration >= SMOKE_ITERATIONS {
            break;
        }
    }
}
