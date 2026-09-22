//! Deterministic fuzz harness for the RFC 1951 DEFLATE codec.
//!
//! `inflate` reads a *foreign* format: PNG `IDAT` bytes today, an SSH peer's
//! compressed traffic next. Both are attacker-reachable, so the decoder is
//! the untrusted-input parser this harness exists for. Its invariants:
//!
//! * neither `inflate_into` nor `Inflater` ever panics, for any input —
//!   arbitrary bytes, a corrupted real stream, or a stream chopped into
//!   arbitrary pieces;
//! * the codec round-trips: whatever the encoder emits decodes back to the
//!   bytes that went in, whether the stream is taken whole or fed to
//!   `Inflater` in arbitrary chunks against arbitrary output room; and
//! * the encoder's `bound` is a real bound — the output never exceeds it.
//!
//! TAIRiX pulls in no external fuzz runner: a per-run-seeded LCG draws the
//! inputs. A plain `cargo test` runs the [`SMOKE_ITERATIONS`] sweep once
//! from a fresh, logged seed; `cargo xtask fuzz --soak` exports
//! `TAIRIX_FUZZ_BUDGET_SECS` to extend the loop to a wall-clock budget.

use tairix_compress::deflate::{Deflate, Flush};
use tairix_compress::inflate::{inflate_into, Inflater};

/// Fixed-iteration sweep run once by a plain `cargo test` (no budget set).
const SMOKE_ITERATIONS: u64 = 600;

/// Largest plaintext drawn by the round-trip sweep. Above one window, so a
/// drawn input exercises the slide and the history the decoder keeps.
const MAX_INPUT: usize = 70_000;

/// Largest arbitrary byte string fed straight to the decoder.
const MAX_STREAM: usize = 4096;

/// Largest output a decode of arbitrary bytes is allowed to produce.
const MAX_OUTPUT: usize = 1 << 20;

/// Low byte of `x`, without a narrowing `as` cast.
fn low_byte(x: u64) -> u8 {
    x.to_le_bytes()[0]
}

/// `x` reduced into `0..=max` as a `usize`, without a narrowing `as` cast.
fn bounded(x: u64, max: usize) -> usize {
    let span = u64::try_from(max).unwrap_or(u64::MAX).saturating_add(1);
    usize::try_from(x % span).unwrap_or(0)
}

/// Decode arbitrary bytes into a bounded destination: must never panic.
fn decode_never_panics(stream: &[u8]) {
    let mut out = vec![0u8; MAX_OUTPUT];
    let _ = inflate_into(stream, &mut out);

    let mut decoder = Box::new(Inflater::new());
    let mut offset = 0usize;
    let mut room = out.len();
    while offset <= stream.len() && room > 0 {
        let Ok(progress) = decoder.inflate(&stream[offset..], &mut out[..room.min(4096)]) else {
            return;
        };
        if progress.consumed == 0 && progress.produced == 0 {
            return;
        }
        offset += progress.consumed;
        room -= progress.produced;
        if progress.finished {
            return;
        }
    }
}

/// Compress `input`, then decode it back both whole and in `chunk`-sized
/// pieces. Both must reproduce `input` exactly.
fn round_trips(input: &[u8], chunk: usize, room: usize) {
    let mut encoder = Box::new(Deflate::new());
    let bound = encoder.bound(input.len());
    let mut stream = vec![0u8; bound];
    let written = encoder
        .deflate(input, &mut stream, Flush::Finish)
        .expect("a bound-sized destination always fits");
    assert!(
        written <= bound,
        "{written} bytes exceeds the bound {bound}"
    );
    let stream = &stream[..written];

    let mut whole = vec![0u8; input.len()];
    let produced = inflate_into(stream, &mut whole).expect("our own stream decodes");
    assert_eq!(produced, input.len(), "one-shot length");
    assert_eq!(&whole[..], input, "one-shot round trip");

    let mut decoder = Box::new(Inflater::new());
    let mut got = Vec::with_capacity(input.len());
    let mut offset = 0usize;
    let mut out = vec![0u8; room.max(1)];
    while !decoder.is_finished() {
        let end = (offset + chunk.max(1)).min(stream.len());
        let progress = decoder
            .inflate(&stream[offset..end], &mut out)
            .expect("our own stream decodes in pieces");
        got.extend_from_slice(&out[..progress.produced]);
        offset += progress.consumed;
        assert!(
            progress.consumed > 0 || progress.produced > 0 || progress.finished,
            "the decoder must make progress"
        );
    }
    assert_eq!(got.len(), input.len(), "streamed length");
    assert_eq!(&got[..], input, "streamed round trip");
}

/// A plaintext mixing runs, a small alphabet, and noise, so a draw exercises
/// long matches, dynamic Huffman, and the stored fallback in turn.
fn draw_input(next: &mut impl FnMut() -> u64, len: usize) -> Vec<u8> {
    let mut input = Vec::with_capacity(len);
    while input.len() < len {
        let remaining = len - input.len();
        match next() % 4 {
            0 => {
                let run = bounded(next(), 300).min(remaining);
                input.extend(std::iter::repeat_n(low_byte(next()), run));
            }
            1 => {
                let run = bounded(next(), 200).min(remaining);
                for _ in 0..run {
                    input.push(b'a' + low_byte(next() >> 9) % 5);
                }
            }
            2 => {
                // Repeat something already written, which is what a
                // back-reference is for.
                let span = bounded(next(), 400).min(remaining).min(input.len());
                let from = input.len() - span;
                let echo = input[from..from + span].to_vec();
                input.extend_from_slice(&echo);
            }
            _ => {
                let run = bounded(next(), 100).min(remaining);
                for _ in 0..run {
                    input.push(low_byte(next() >> 17));
                }
            }
        }
    }
    input
}

#[test]
fn inflate_never_panics_and_the_codec_round_trips() {
    let deadline = tairix_fuzzseed::budget_deadline(tairix_fuzzseed::FUZZ_BUDGET_ENV);

    let mut state: u64 = tairix_fuzzseed::start(
        "inflate_never_panics_and_the_codec_round_trips",
        tairix_fuzzseed::FUZZ_SEED_ENV,
    );
    let mut next = || {
        state = state
            .wrapping_mul(6_364_136_223_846_793_005)
            .wrapping_add(1_442_695_040_888_963_407);
        state
    };

    let mut iteration: u64 = 0;
    loop {
        // 1. A structured plaintext, round-tripped whole and in pieces.
        let len = bounded(next(), MAX_INPUT);
        let input = draw_input(&mut next, len);
        let chunk = bounded(next(), 512).max(1);
        let room = bounded(next(), 8192).max(1);
        round_trips(&input, chunk, room);

        // 2. Corrupt a real stream and feed it to the decoder.
        let mut encoder = Box::new(Deflate::new());
        let mut stream = vec![0u8; encoder.bound(input.len())];
        if let Ok(written) = encoder.deflate(&input, &mut stream, Flush::Finish) {
            let mut damaged = stream[..written].to_vec();
            for _ in 0..bounded(next(), 8) {
                if damaged.is_empty() {
                    break;
                }
                let at = bounded(next(), damaged.len() - 1);
                damaged[at] ^= low_byte(next() >> 19);
            }
            decode_never_panics(&damaged);
        }

        // 3. Pure noise straight into the decoder.
        let noise: Vec<u8> = (0..bounded(next(), MAX_STREAM))
            .map(|_| low_byte(next() >> 23))
            .collect();
        decode_never_panics(&noise);

        iteration += 1;
        if !tairix_fuzzseed::within_budget(deadline) && iteration >= SMOKE_ITERATIONS {
            break;
        }
    }
}
