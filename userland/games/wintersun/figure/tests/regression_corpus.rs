//! The committed regression corpus, replayed with pinned verdicts.
//!
//! `fuzz_identity.rs` proves the decoder never panics over a continuing
//! pseudo-random stream; this is the companion corpus the charter requires
//! alongside it, so an input once found is replayed on the bytes it was
//! filed under rather than only by a fuzzer that may not draw it again.
//!
//! Two contracts:
//!
//! 1. [`corpus`] holds records built from the crate's own encoder, so it
//!    cannot drift from the source of truth. Every one must decode,
//!    re-encode to the same bytes, and build.
//! 2. The literals below pin a *fixed* verdict on bytes that ring an accept
//!    or reject edge, so a change that silently loosens a bound (admits a
//!    swatch past its table, a tail on a human, a second spelling of a bald
//!    figure) or tightens one (refuses a legal record) fails here.
//!
//! No crash has been found in this decoder to date. A new one is appended
//! below as a named byte literal with its own verdict.

use tairix_wintersun_figure::humanoid;
use tairix_wintersun_figure::identity::{Field, Identity, IdentityError, RECORD_LEN};

mod corpus;

/// The reference human, as its bytes: version, species, five middle build
/// settings, an oval face, almond eyes, round ears, no horns, no tail, short
/// hair at middle volume, skin 4, brown hair, brown eyes, no markings, the
/// slate dye.
const HUMAN: [u8; RECORD_LEN] = [
    1, 0, 128, 128, 128, 128, 128, 0, 1, 0, 0, 0, 2, 128, 4, 7, 1, 0, 0,
];

/// `HUMAN` with byte `at` set to `value`.
fn human_with(at: usize, value: u8) -> [u8; RECORD_LEN] {
    let mut out = HUMAN;
    out[at] = value;
    out
}

#[test]
fn every_corpus_record_decodes_re_encodes_and_builds() {
    for record in corpus::records() {
        let identity = Identity::decode(&record).expect("a corpus record decodes");
        assert_eq!(identity.encode().as_slice(), record.as_slice());
        humanoid::rig(&identity).expect("and builds");
    }
}

#[test]
fn the_reference_human_is_the_record_its_bytes_spell() {
    let identity = Identity::decode(&HUMAN).expect("the reference human decodes");
    assert_eq!(identity.encode(), HUMAN);
}

#[test]
fn a_record_of_the_wrong_length_is_refused() {
    assert_eq!(
        Identity::decode(&HUMAN[..RECORD_LEN - 1]),
        Err(IdentityError::Truncated)
    );
    let mut long = HUMAN.to_vec();
    long.push(0);
    assert_eq!(Identity::decode(&long), Err(IdentityError::TrailingBytes));
    assert_eq!(Identity::decode(&[]), Err(IdentityError::Truncated));
}

#[test]
fn another_format_is_refused() {
    assert_eq!(
        Identity::decode(&human_with(0, 0)),
        Err(IdentityError::UnknownVersion)
    );
    assert_eq!(
        Identity::decode(&human_with(0, 2)),
        Err(IdentityError::UnknownVersion)
    );
}

#[test]
fn the_last_of_each_field_is_admitted_and_one_past_it_refused() {
    // The dragonkin is the last species.
    let dragonkin = Identity::new(tairix_wintersun_figure::reference::spec(
        tairix_wintersun_figure::species::Species::Dragonkin,
    ))
    .expect("real")
    .encode();
    assert!(Identity::decode(&dragonkin).is_ok());
    let mut past = dragonkin;
    past[1] = 5;
    assert_eq!(
        Identity::decode(&past),
        Err(IdentityError::Unknown(Field::Species))
    );

    // Skin 11 is a human's last; 12 is past the table.
    assert!(Identity::decode(&human_with(14, 11)).is_ok());
    assert_eq!(
        Identity::decode(&human_with(14, 12)),
        Err(IdentityError::Unknown(Field::SkinColour))
    );
    // Hair 15 and dye 15 are the last of shared tables of sixteen.
    assert!(Identity::decode(&human_with(15, 15)).is_ok());
    assert_eq!(
        Identity::decode(&human_with(15, 16)),
        Err(IdentityError::Unknown(Field::HairColour))
    );
    assert!(Identity::decode(&human_with(18, 15)).is_ok());
    assert_eq!(
        Identity::decode(&human_with(18, 16)),
        Err(IdentityError::Unknown(Field::AccentColour))
    );
    // The heart face is the last; the byte after it names nothing.
    assert!(Identity::decode(&human_with(7, 4)).is_ok());
    assert_eq!(
        Identity::decode(&human_with(7, 5)),
        Err(IdentityError::Unknown(Field::Face))
    );
}

#[test]
fn a_feature_a_human_does_not_carry_is_refused() {
    assert_eq!(
        Identity::decode(&human_with(11, 1)),
        Err(IdentityError::NotOfSpecies(Field::Tail))
    );
    assert_eq!(
        Identity::decode(&human_with(10, 1)),
        Err(IdentityError::NotOfSpecies(Field::Horns))
    );
    assert_eq!(
        Identity::decode(&human_with(9, 3)),
        Err(IdentityError::NotOfSpecies(Field::Ears))
    );
    // Red eyes are a dragon's.
    assert_eq!(
        Identity::decode(&human_with(16, 9)),
        Err(IdentityError::NotOfSpecies(Field::EyeColour))
    );
}

#[test]
fn a_second_spelling_of_a_figure_is_refused() {
    // Bald, with the zero volume and hair colour a bald figure has.
    let mut bald = human_with(12, 0);
    bald[13] = 0;
    bald[15] = 0;
    assert!(Identity::decode(&bald).is_ok());
    let mut full = bald;
    full[13] = 1;
    assert_eq!(
        Identity::decode(&full),
        Err(IdentityError::NonCanonical(Field::Volume))
    );
    let mut coloured = bald;
    coloured[15] = 1;
    assert_eq!(
        Identity::decode(&coloured),
        Err(IdentityError::NonCanonical(Field::HairColour))
    );
    // A human has no markings.
    assert_eq!(
        Identity::decode(&human_with(17, 1)),
        Err(IdentityError::NonCanonical(Field::MarkingsColour))
    );
}
