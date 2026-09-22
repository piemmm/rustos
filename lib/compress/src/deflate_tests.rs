//! Tests for the RFC 1951 encoder.

extern crate alloc;

use alloc::boxed::Box;
use alloc::vec;
use alloc::vec::Vec;

use super::{Deflate, Error, Flush};
use crate::inflate;

/// Compress `input` as one finished stream and give back the bytes.
fn finish(input: &[u8]) -> Vec<u8> {
    let mut encoder = Box::new(Deflate::new());
    let mut out = vec![0u8; encoder.bound(input.len())];
    let written = encoder
        .deflate(input, &mut out, Flush::Finish)
        .expect("a bound-sized destination always fits");
    out.truncate(written);
    out
}

/// Inflate `stream` back, expecting exactly `expected_len` bytes.
fn expand(stream: &[u8], expected_len: usize) -> Vec<u8> {
    let mut out = vec![0u8; expected_len];
    let produced = inflate::inflate_into(stream, &mut out).expect("our own stream decodes");
    out.truncate(produced);
    out
}

#[test]
fn empty_input_round_trips() {
    let stream = finish(&[]);
    assert_eq!(expand(&stream, 0), Vec::<u8>::new());
}

#[test]
fn single_byte_round_trips() {
    let stream = finish(b"x");
    assert_eq!(expand(&stream, 1), b"x");
}

#[test]
fn text_round_trips_and_shrinks() {
    let input = b"the quick brown fox jumps over the lazy dog. \
                  the quick brown fox jumps over the lazy dog. \
                  the quick brown fox jumps over the lazy dog."
        .to_vec();
    let stream = finish(&input);
    assert!(
        stream.len() < input.len(),
        "repeated text must compress: {} -> {}",
        input.len(),
        stream.len()
    );
    assert_eq!(expand(&stream, input.len()), input);
}

#[test]
fn long_run_round_trips() {
    let input = vec![0x5Au8; 200_000];
    let stream = finish(&input);
    assert!(stream.len() < input.len() / 100, "a run must collapse");
    assert_eq!(expand(&stream, input.len()), input);
}

#[test]
fn incompressible_input_never_exceeds_the_bound() {
    // A counter-based stream with no repeats worth coding, long enough to
    // cross several block boundaries.
    let input: Vec<u8> = (0..300_000u32)
        .map(|index| {
            let mixed = index.wrapping_mul(2_654_435_761).rotate_left(13);
            mixed.to_le_bytes()[0]
        })
        .collect();
    let mut encoder = Box::new(Deflate::new());
    let bound = encoder.bound(input.len());
    let mut out = vec![0u8; bound];
    let written = encoder
        .deflate(&input, &mut out, Flush::Finish)
        .expect("fits the bound");
    assert!(written <= bound, "{written} exceeds the bound {bound}");
    assert_eq!(expand(&out[..written], input.len()), input);
}

#[test]
fn a_destination_below_the_bound_is_refused_without_consuming() {
    let mut encoder = Box::new(Deflate::new());
    let mut tiny = [0u8; 4];
    assert_eq!(
        encoder.deflate(b"hello", &mut tiny, Flush::Finish),
        Err(Error::OutputOverflow)
    );
    // Unchanged: the same encoder still produces a whole stream.
    let mut out = vec![0u8; encoder.bound(5)];
    let written = encoder
        .deflate(b"hello", &mut out, Flush::Finish)
        .expect("fits");
    assert_eq!(expand(&out[..written], 5), b"hello");
}

#[test]
fn a_finished_stream_refuses_more_input() {
    let mut encoder = Box::new(Deflate::new());
    let mut out = vec![0u8; encoder.bound(1)];
    encoder
        .deflate(b"a", &mut out, Flush::Finish)
        .expect("fits");
    assert!(encoder.is_finished());
    assert_eq!(
        encoder.deflate(b"b", &mut out, Flush::Finish),
        Err(Error::Finished)
    );
    encoder.reset();
    assert!(!encoder.is_finished());
    let written = encoder
        .deflate(b"b", &mut out, Flush::Finish)
        .expect("fits");
    assert_eq!(expand(&out[..written], 1), b"b");
}

#[test]
fn sync_flush_makes_every_byte_so_far_readable() {
    let mut encoder = Box::new(Deflate::new());
    let mut decoder = Box::new(inflate::Inflater::new());
    let messages: [&[u8]; 4] = [
        b"first message, with some text in it",
        b"second message, with some text in it",
        b"third",
        b"first message, with some text in it",
    ];
    for message in messages {
        let mut out = vec![0u8; encoder.bound(message.len())];
        let written = encoder
            .deflate(message, &mut out, Flush::Sync)
            .expect("fits");
        let mut back = vec![0u8; message.len() + 64];
        let progress = decoder
            .inflate(&out[..written], &mut back)
            .expect("decodes");
        assert_eq!(
            progress.consumed, written,
            "a sync flush leaves no partial byte behind"
        );
        assert_eq!(&back[..progress.produced], message);
        assert!(!progress.finished);
    }
}

#[test]
fn a_later_flush_back_references_an_earlier_one() {
    let mut encoder = Box::new(Deflate::new());
    let body = b"a long distinctive phrase that should be matched later on";
    let mut first = vec![0u8; encoder.bound(body.len())];
    let first_len = encoder
        .deflate(body, &mut first, Flush::Sync)
        .expect("fits");
    let mut second = vec![0u8; encoder.bound(body.len())];
    let second_len = encoder
        .deflate(body, &mut second, Flush::Sync)
        .expect("fits");
    assert!(
        second_len < first_len,
        "the repeat must cost less than the original: {first_len} then {second_len}"
    );

    let mut decoder = Box::new(inflate::Inflater::new());
    let mut back = vec![0u8; body.len()];
    let progress = decoder.inflate(&first[..first_len], &mut back).expect("ok");
    assert_eq!(&back[..progress.produced], body);
    let progress = decoder
        .inflate(&second[..second_len], &mut back)
        .expect("ok");
    assert_eq!(&back[..progress.produced], body);
}

#[test]
fn buffered_input_is_flushed_by_a_later_call() {
    let mut encoder = Box::new(Deflate::new());
    let mut stream = Vec::new();
    let first = b"hold this back";
    let mut out = vec![0u8; encoder.bound(first.len())];
    let written = encoder.deflate(first, &mut out, Flush::None).expect("fits");
    stream.extend_from_slice(&out[..written]);
    let second = b" and then finish";
    let mut out = vec![0u8; encoder.bound(second.len())];
    let written = encoder
        .deflate(second, &mut out, Flush::Finish)
        .expect("fits");
    stream.extend_from_slice(&out[..written]);

    let whole = [first.as_slice(), second.as_slice()].concat();
    assert_eq!(expand(&stream, whole.len()), whole);
}

#[test]
fn input_spanning_several_window_slides_round_trips() {
    // Pseudo-random bytes drawn from a small alphabet: compressible enough
    // to produce matches, long enough to slide the window many times.
    let mut state = 0x1234_5678u32;
    let input: Vec<u8> = (0..400_000)
        .map(|_| {
            state = state.wrapping_mul(1_664_525).wrapping_add(1_013_904_223);
            u8::try_from((state >> 24) % 7).unwrap_or(0) + b'a'
        })
        .collect();
    let stream = finish(&input);
    assert_eq!(expand(&stream, input.len()), input);
}
