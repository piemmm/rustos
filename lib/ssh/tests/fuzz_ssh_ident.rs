//! Deterministic fuzz harness for the identification exchange.
//!
//! The identification line is the first thing a peer sends and is read before
//! anything is authenticated or even framed. The invariants:
//!
//! * nothing panics, however the bytes are chosen or chunked;
//! * a line is never held past its bound: whatever is buffered without a
//!   terminator is refused once it reaches [`MAX_BANNER_LINE`] (or
//!   [`MAX_IDENT_LINE`] for a line that began `SSH-`);
//! * where a line ends does not depend on how the stream was divided, and
//!   resuming a scan where the last one stopped gives what a fresh scan does;
//! * a server may precede its identification with lines and a client may not;
//!   and
//! * the identification is kept byte for byte, because the exchange hash
//!   covers it.

use tairix_fuzzseed::Prng;
use tairix_ssh::ident::{next_line, Ident, IdentError, Line, MAX_BANNER_LINE, MAX_IDENT_LINE};
use tairix_ssh::Role;

/// Fixed-iteration sweep run once by a plain `cargo test`.
const SMOKE_ITERATIONS: u64 = 1500;

/// Bytes the structured streams are drawn from: the ones the rules turn on,
/// weighted toward them.
const ALPHABET: &[u8] = b"SSH-2.0-1.99 abc_\r\n\r\n\0\x7f\xff";

/// Split `stream` into lines as a transport would, feeding it `chunks` at a
/// time, and return what it saw: each line's text and kind, and how it ended.
fn split(
    stream: &[u8],
    from: Role,
    chunks: &[usize],
) -> (Vec<(bool, Vec<u8>)>, Option<IdentError>) {
    let mut buffered = Vec::new();
    let mut offset = 0;
    let mut resume = 0;
    let mut lines = Vec::new();
    let mut sizes = chunks.iter().copied().cycle();
    while offset < stream.len() {
        let take = sizes.next().unwrap_or(1).max(1).min(stream.len() - offset);
        buffered.extend_from_slice(&stream[offset..offset + take]);
        offset += take;
        loop {
            let outcome = next_line(&buffered, from, resume);
            assert_eq!(
                outcome,
                next_line(&buffered, from, 0),
                "resuming changed the answer"
            );
            match outcome {
                Ok(Line::Incomplete { scanned }) => {
                    assert!(scanned >= resume && scanned <= buffered.len());
                    resume = scanned;
                    let limit = if b"SSH-".starts_with(&buffered[..buffered.len().min(4)]) {
                        MAX_IDENT_LINE
                    } else {
                        MAX_BANNER_LINE
                    };
                    assert!(
                        buffered.len() < limit,
                        "an unterminated line was held at its bound"
                    );
                    break;
                }
                Ok(Line::Banner { text, consumed }) => {
                    assert_eq!(from, Role::Server, "only a server sends banner lines");
                    assert!(consumed <= MAX_BANNER_LINE && text.len() < consumed);
                    lines.push((false, text.to_vec()));
                    buffered.drain(..consumed);
                    resume = 0;
                }
                Ok(Line::Ident { text, consumed }) => {
                    assert!(consumed <= MAX_IDENT_LINE);
                    assert!(text.starts_with(b"SSH-"));
                    if let Ok(ident) = Ident::parse(text) {
                        assert_eq!(ident.as_bytes(), text, "kept byte for byte");
                        assert!(matches!(ident.protocol(), b"2.0" | b"1.99"));
                        assert!(!ident.software().is_empty());
                    }
                    lines.push((true, text.to_vec()));
                    return (lines, None);
                }
                Err(err) => return (lines, Some(err)),
            }
        }
    }
    (lines, None)
}

/// A stream shaped like a peer's opening: some lines, maybe an
/// identification, maybe junk.
fn draw_stream(rng: &mut Prng) -> Vec<u8> {
    let mut stream = Vec::new();
    for _ in 0..rng.at_most(4) {
        let len = if rng.below(16) == 0 {
            MAX_BANNER_LINE - 4 + rng.at_most(8)
        } else {
            rng.at_most(40)
        };
        for _ in 0..len {
            stream.push(*rng.pick(ALPHABET));
        }
        stream.extend_from_slice(if rng.below(4) == 0 { b"\n" } else { b"\r\n" });
    }
    if rng.below(3) != 0 {
        stream.extend_from_slice(rng.pick(&[&b"SSH-2.0-"[..], b"SSH-1.99-", b"SSH-1.5-", b"SSH-"]));
        let len = if rng.below(8) == 0 {
            MAX_IDENT_LINE - 10 + rng.at_most(20)
        } else {
            rng.at_most(30)
        };
        for _ in 0..len {
            stream.push(*rng.pick(ALPHABET));
        }
        stream.extend_from_slice(b"\r\n");
    }
    stream
}

#[test]
fn the_identification_exchange_is_bounded_and_chunking_blind() {
    let deadline = tairix_fuzzseed::budget_deadline(tairix_fuzzseed::FUZZ_BUDGET_ENV);
    let mut rng = Prng::new(tairix_fuzzseed::start(
        "the_identification_exchange_is_bounded_and_chunking_blind",
        tairix_fuzzseed::FUZZ_SEED_ENV,
    ));
    let mut iteration: u64 = 0;
    loop {
        let stream = if rng.below(4) == 0 {
            let mut noise = vec![0u8; rng.at_most(600)];
            rng.fill(&mut noise);
            noise
        } else {
            draw_stream(&mut rng)
        };
        let from = if rng.below(2) == 0 {
            Role::Server
        } else {
            Role::Client
        };
        let whole = split(&stream, from, &[stream.len()]);
        let chunks: Vec<usize> = (0..=rng.at_most(5)).map(|_| 1 + rng.at_most(64)).collect();
        let pieces = split(&stream, from, &chunks);
        let bytewise = split(&stream, from, &[1]);
        assert_eq!(whole.0, pieces.0, "lines depend on the chunking");
        assert_eq!(whole.0, bytewise.0, "lines depend on the chunking");
        // A stream refused whole may be refused sooner in pieces, never later
        // and never for a different reason once both saw the same bytes.
        if let (Some(a), Some(b)) = (whole.1, bytewise.1) {
            assert_eq!(a, b, "the refusal depends on the chunking");
        }
        if from == Role::Client {
            assert!(
                whole.0.iter().all(|(ident, _)| *ident),
                "a client sent a banner line"
            );
        }
        // The line parser alone is total too.
        let mut line = vec![0u8; rng.at_most(300)];
        rng.fill(&mut line);
        if rng.below(2) == 0 && line.len() >= 8 {
            line[..8].copy_from_slice(b"SSH-2.0-");
        }
        let _ = Ident::parse(&line);

        iteration += 1;
        if !tairix_fuzzseed::within_budget(deadline) && iteration >= SMOKE_ITERATIONS {
            break;
        }
    }
}
