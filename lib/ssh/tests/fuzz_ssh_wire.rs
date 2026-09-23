//! Deterministic fuzz harness for the RFC 4251 wire codec.
//!
//! Every SSH message a peer sends is read through this codec before anything
//! else looks at it. The invariants:
//!
//! * no sequence of reads panics, whatever the bytes;
//! * a read never reports more bytes than the input held;
//! * every `string`, `mpint`, and `name-list` that decodes re-encodes to
//!   exactly the bytes it came from — so no attacker-chosen spelling of a
//!   value survives a parse; and
//! * every value the writer produces decodes back to itself.
//!
//! Inputs are drawn from a per-run-seeded `Prng`; the smoke sweep runs on a
//! plain `cargo test` and `cargo xtask fuzz --soak` extends it to a budget.

use tairix_fuzzseed::Prng;
use tairix_ssh::wire::{Mpint, NameList, Reader, WireError, Writer};

/// Fixed-iteration sweep run once by a plain `cargo test`.
const SMOKE_ITERATIONS: u64 = 2000;

/// Largest arbitrary input read.
const MAX_INPUT: usize = 512;

/// The name alphabet the structured draws spell names from, including the
/// bytes a name may not hold.
const NAME_BYTES: &[u8] = b"abcz09-_.@,  \t\x7f\x00~!";

/// Read `bytes` as a random sequence of values, checking each canonical one
/// re-encodes to what it consumed.
fn read_everything(rng: &mut Prng, bytes: &[u8]) {
    let mut reader = Reader::new(bytes);
    loop {
        let before = reader.remaining();
        let consumed = |after: usize| &bytes[bytes.len() - before..bytes.len() - after];
        let outcome = match rng.below(9) {
            0 => reader.byte().map(|_| ()),
            1 => reader.boolean().map(|_| ()),
            2 => reader.uint32().map(|_| ()),
            3 => reader.uint64().map(|_| ()),
            4 => reader.fixed::<16>().map(|_| ()),
            5 => reader.string().map(|value| {
                let mut out = Vec::new();
                let mut writer = Writer::new(&mut out);
                writer.string(value);
                writer.finish().expect("a read string fits a write");
                assert_eq!(out, consumed(reader.remaining()));
            }),
            6 => reader.utf8().map(|_| ()),
            7 => reader.mpint().map(|value| {
                let mut out = Vec::new();
                let mut writer = Writer::new(&mut out);
                writer.mpint(value);
                writer.finish().expect("fits");
                assert_eq!(
                    out,
                    consumed(reader.remaining()),
                    "a decoded mpint is canonical"
                );
                if let Some(magnitude) = value.magnitude() {
                    assert!(
                        magnitude.first() != Some(&0),
                        "a magnitude has no leading zero"
                    );
                }
            }),
            _ => reader.name_list().map(|list| {
                let mut out = Vec::new();
                let mut writer = Writer::new(&mut out);
                writer.name_list(list);
                writer.finish().expect("fits");
                assert_eq!(out, consumed(reader.remaining()));
                for name in list.iter() {
                    assert!(!name.is_empty() && name.len() <= 64);
                    assert!(name.bytes().all(|b| b > b' ' && b <= b'~' && b != b','));
                }
                assert_eq!(NameList::new(list.as_str()), Ok(list));
            }),
        };
        assert!(reader.remaining() <= before, "a read gave bytes back");
        match outcome {
            Ok(()) if reader.remaining() == 0 => {
                assert_eq!(reader.clone().finish(), Ok(()));
                return;
            }
            Ok(()) => {}
            Err(err) => {
                assert!(matches!(
                    err,
                    WireError::Truncated
                        | WireError::NonCanonicalMpint
                        | WireError::InvalidName
                        | WireError::InvalidUtf8
                ));
                return;
            }
        }
    }
}

/// A name-list drawn to sit near the validity boundary.
fn draw_names(rng: &mut Prng) -> Vec<u8> {
    let mut text = Vec::new();
    for index in 0..rng.at_most(6) {
        if index > 0 {
            text.push(b',');
        }
        for _ in 0..rng.at_most(70) {
            text.push(*rng.pick(NAME_BYTES));
        }
    }
    text
}

/// A magnitude, often with leading zeros and a set top bit.
fn draw_magnitude(rng: &mut Prng) -> Vec<u8> {
    let mut magnitude = vec![0u8; rng.at_most(80)];
    rng.fill(&mut magnitude);
    for byte in magnitude.iter_mut().take(rng.at_most(4)) {
        *byte = 0;
    }
    magnitude
}

#[test]
fn the_wire_codec_is_total_and_canonical() {
    let deadline = tairix_fuzzseed::budget_deadline(tairix_fuzzseed::FUZZ_BUDGET_ENV);
    let mut rng = Prng::new(tairix_fuzzseed::start(
        "the_wire_codec_is_total_and_canonical",
        tairix_fuzzseed::FUZZ_SEED_ENV,
    ));
    let mut iteration: u64 = 0;
    loop {
        // 1. Arbitrary bytes under an arbitrary read sequence.
        let mut noise = vec![0u8; rng.at_most(MAX_INPUT)];
        rng.fill(&mut noise);
        read_everything(&mut rng, &noise);

        // 2. A name-list near the boundary: accepted exactly when every name
        //    is one RFC 4250 allows, and then it round-trips.
        let names = draw_names(&mut rng);
        match NameList::parse(&names) {
            Ok(list) => {
                assert_eq!(list.as_str().as_bytes(), &names[..]);
                if !names.is_empty() {
                    assert_eq!(list.iter().count(), names.split(|&b| b == b',').count());
                }
            }
            Err(err) => assert_eq!(err, WireError::InvalidName),
        }

        // 3. A magnitude written canonically reads back as that magnitude.
        let magnitude = draw_magnitude(&mut rng);
        let mut out = Vec::new();
        let mut writer = Writer::new(&mut out);
        writer.mpint_unsigned(&magnitude);
        writer.finish().expect("small");
        let mut reader = Reader::new(&out);
        let value = reader.mpint().expect("written canonically");
        reader.finish().expect("exactly one value");
        let digits: Vec<u8> = magnitude.iter().copied().skip_while(|&b| b == 0).collect();
        assert_eq!(value.magnitude(), Some(&digits[..]));

        // 4. Arbitrary two's-complement data: accepted exactly when canonical.
        let data = draw_magnitude(&mut rng);
        if let Ok(value) = Mpint::from_twos_complement(&data) {
            let redundant = match data.as_slice() {
                [0x00] => true,
                [0x00, next, ..] => next & 0x80 == 0,
                [0xFF, next, ..] => next & 0x80 != 0,
                _ => false,
            };
            assert!(
                !redundant,
                "a redundant leading byte was accepted: {data:02x?}"
            );
            assert_eq!(value.as_twos_complement(), &data[..]);
        }

        iteration += 1;
        if !tairix_fuzzseed::within_budget(deadline) && iteration >= SMOKE_ITERATIONS {
            break;
        }
    }
}
