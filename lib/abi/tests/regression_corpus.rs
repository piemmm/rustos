//! Deterministic regression corpus for the CCOMPAT CC3/CC4 decoders.
//!
//! `lib/abi/tests/fuzz_decode.rs` proves the decoders never panic over a
//! continuing pseudo-random stream (`AGENTS.md` §19.6); this file is the
//! companion **regression corpus** the charter requires alongside it
//! (`AGENTS.md` §19.6 — "crashing inputs are added to the crate's regression
//! corpus alongside a unit test, so the same bytes are replayed"). No crash
//! has been found in these decoders to date, so the corpus is seeded instead
//! with the hand-crafted boundary cases that ring the accept/reject edge of
//! each decoder the CCOMPAT plan added:
//!
//! * the `rxe` needed-library table (`NeededLibrary::decode`) and the
//!   whole-image loader (`LoadImage::parse`) from stage CC4
//!   (`plans/CCOMPAT.md` §3 "Stage CC4");
//! * the program startup vector (`ProcessStartHeader::from_bytes`,
//!   `StringSlot::from_bytes`, `ProcessStart::parse`) from stage CC3.
//!
//! The file enforces two complementary contracts:
//!
//! 1. [`replay`] drives every CC3/CC4 decoder on each corpus entry and
//!    re-asserts the §19.6 "must not panic, and an accepted decode
//!    round-trips" contract on the very bytes a crash would be filed under.
//! 2. The per-decoder regression tests pin a *fixed* accept-or-reject verdict
//!    for the **validating** decoders (`NeededLibrary::decode`,
//!    `LoadImage::parse`, `ProcessStart::parse`), so a future change that
//!    silently loosens (accepts a malformed image) or tightens (rejects a
//!    valid one) one of them fails this test. (`ProcessStartHeader::from_bytes`
//!    and `StringSlot::from_bytes` are deliberately *not* validating decoders —
//!    they are fixed-width field readers whose only failure is
//!    [`Errno::BufferTooSmall`] — so verdicts are not pinned on them.)
//!
//! New crashing inputs, when found, are appended to [`corpus`] with a name and
//! a dedicated verdict test (`AGENTS.md` §19.6, §7; `tests/SECURITY.md`).

use rustos_abi::process::{ProcessStart, ProcessStartHeader, StringSlot};
use rustos_abi::{
    LoadHeader, LoadImage, NeededLibrary, RxeError, RxePermission, Segment, ABI_VERSION_CURRENT,
    LIBREF_MAX, LOAD_FLAG_PIE, LOAD_MAGIC, LOAD_MAX_NEEDED, RXE_PAGE_SIZE, SYSCALL_TABLE_HASH_LEN,
};

/// CFI tag the corpus images are stamped with and [`LoadImage::parse`] is
/// asked to expect. Any non-matching value would fail closed with
/// `InterfaceHashMismatch` before the structural checks run; using a single
/// value keeps every "valid image" entry actually parseable.
const TAG: [u8; SYSCALL_TABLE_HASH_LEN] = [0x5A; SYSCALL_TABLE_HASH_LEN];

/// The reference path of the curated *System runtime / C ABI* library a hosted
/// C bundle declares it needs (`AGENTS.md` §16.4; `plans/CCOMPAT.md` CC4).
const SYSTEM_RUNTIME_LIB: &str = "/System/Libraries/libros-sys.so";

/// Per-process stack-canary seed baked into the corpus startup vector.
const CANARY: u64 = 0xC0FF_EE00_D15E_A5ED;

// --- valid-image builders (public encoders only, `AGENTS.md` §2.2) -------

/// Encode an `rxe` load image with one R+X code page at vaddr 0 (holding the
/// entry point) and one R+W data page, plus `needed` shared-library records.
///
/// Built exclusively from the public `lib/abi` encoders so the corpus can
/// never disagree with the source of truth. `LoadImage::parse` validates the
/// header and the segment/needed tables (not the segment payloads), so the
/// records alone make a parseable image.
fn load_image(needed: &[&str]) -> Vec<u8> {
    let code = Segment {
        vaddr: 0,
        file_offset: 0,
        file_size: RXE_PAGE_SIZE,
        mem_size: RXE_PAGE_SIZE,
        permission: RxePermission::ReadExecute,
    };
    let data = Segment {
        vaddr: RXE_PAGE_SIZE,
        file_offset: RXE_PAGE_SIZE,
        file_size: RXE_PAGE_SIZE,
        mem_size: RXE_PAGE_SIZE,
        permission: RxePermission::ReadWrite,
    };
    let header = LoadHeader {
        magic: LOAD_MAGIC,
        abi_version: ABI_VERSION_CURRENT,
        flags: LOAD_FLAG_PIE,
        segment_count: 2,
        needed_count: u16::try_from(needed.len()).expect("needed count fits"),
        entry: 0,
        cfi_tag: TAG,
    };

    let mut out = Vec::new();
    out.extend_from_slice(&header.to_le_bytes());
    out.extend_from_slice(&code.to_le_bytes());
    out.extend_from_slice(&data.to_le_bytes());
    for reference in needed {
        let record = NeededLibrary::from_reference(reference).expect("valid reference");
        out.extend_from_slice(&record.to_le_bytes());
    }
    out
}

/// A valid program startup vector (`argv = ["prog", "42"]`, empty `envp`),
/// built through the production [`rustos_abi::process::write_into`] writer.
fn startup_vector() -> Vec<u8> {
    let args: &[&[u8]] = &[b"prog", b"42"];
    let env: &[&[u8]] = &[];
    let len = rustos_abi::process::encoded_len(args, env).expect("within abi-v1 limits");
    let mut buf = vec![0u8; len];
    let written = rustos_abi::process::write_into(&mut buf, args, env, CANARY).expect("fits");
    assert_eq!(written, len);
    buf
}

/// One needed-library record on the wire (256 bytes) for the curated runtime.
fn needed_record() -> Vec<u8> {
    NeededLibrary::from_reference(SYSTEM_RUNTIME_LIB)
        .expect("valid reference")
        .to_le_bytes()
        .to_vec()
}

// --- the corpus ----------------------------------------------------------

/// The seeded corpus: stable `(name, bytes)` pairs replayed by [`replay`].
///
/// The names are stable so a regression points at the exact entry, and the
/// bytes are the replayable artefact a future crash would be filed under.
fn corpus() -> Vec<(&'static str, Vec<u8>)> {
    let over_cap = u16::try_from(LOAD_MAX_NEEDED + 1).expect("fits u16");
    let overlong = u8::try_from(LIBREF_MAX).expect("fits");
    vec![
        // Degenerate lengths every decoder must handle without indexing out
        // of bounds.
        ("empty", Vec::new()),
        ("single_zero", vec![0u8]),
        ("zeros_16", vec![0u8; 16]),
        ("zeros_64", vec![0u8; 64]),
        ("zeros_512", vec![0u8; 512]),
        // CC4 — whole image: valid shapes.
        ("image_no_needed", load_image(&[])),
        ("image_one_needed", load_image(&[SYSTEM_RUNTIME_LIB])),
        (
            "image_max_needed",
            load_image(&[SYSTEM_RUNTIME_LIB; LOAD_MAX_NEEDED]),
        ),
        // CC4 — whole image: malformed shapes that must fail closed, not panic.
        ("image_needed_truncated", image_with_needed_count(1)),
        ("image_needed_over_cap", image_with_needed_count(over_cap)),
        ("image_bad_magic", flip_byte(load_image(&[]), 0)),
        ("image_not_pie", clear_pie_flag(load_image(&[]))),
        // CC4 — needed-library record (256 bytes), standalone shapes.
        ("needed_valid", needed_record()),
        ("needed_zero_len", set_byte(needed_record(), 0, 0)),
        ("needed_embedded_nul", set_byte(needed_record(), 2, 0)),
        ("needed_dirty_padding", dirty_needed_padding()),
        (
            "needed_overlong_len",
            set_byte(needed_record(), 0, overlong),
        ),
        // CC3 — startup vector shapes.
        ("startup_valid", startup_vector()),
        ("startup_bad_magic", flip_byte(startup_vector(), 0)),
        (
            "startup_header_truncated",
            startup_vector()[..ProcessStartHeader::WIRE_LEN - 1].to_vec(),
        ),
        (
            "string_slot",
            StringSlot {
                offset: 0x20,
                len: 4,
            }
            .to_le_bytes()
            .to_vec(),
        ),
    ]
}

/// A load image whose header claims `needed_count` records (without appending
/// any), so the loader must detect the missing table.
fn image_with_needed_count(needed_count: u16) -> Vec<u8> {
    let mut bytes = load_image(&[]);
    bytes[14..16].copy_from_slice(&needed_count.to_le_bytes());
    bytes
}

/// Clear the [`LOAD_FLAG_PIE`] bit in a load-image header (offset 8).
fn clear_pie_flag(mut bytes: Vec<u8>) -> Vec<u8> {
    bytes[8..12].copy_from_slice(&0u32.to_le_bytes());
    bytes
}

/// A needed-library record with a non-zero byte in the padding past the
/// reference length, which `decode` must reject.
fn dirty_needed_padding() -> Vec<u8> {
    let mut bytes = needed_record();
    let len = usize::from(bytes[0]);
    bytes[1 + len] = 0xFF;
    bytes
}

/// XOR `0xFF` into one byte of `bytes`.
fn flip_byte(mut bytes: Vec<u8>, at: usize) -> Vec<u8> {
    bytes[at] ^= 0xFF;
    bytes
}

/// Set one byte of `bytes` to `value`.
fn set_byte(mut bytes: Vec<u8>, at: usize, value: u8) -> Vec<u8> {
    bytes[at] = value;
    bytes
}

// --- the universal no-panic / round-trip contract ------------------------

/// Drive every CC3/CC4 decoder on `bytes`.
///
/// Mirrors the contract `fuzz_decode::exercise` enforces, scoped to the
/// decoders this corpus guards: no decoder may panic, and an accepted decode
/// must round-trip (or re-parse deterministically) through its own encoder.
fn replay(bytes: &[u8]) {
    if let Ok(lib) = NeededLibrary::decode(bytes) {
        let redecoded = NeededLibrary::decode(&lib.to_le_bytes())
            .expect("round-trip of an accepted needed-library record must succeed");
        assert_eq!(lib, redecoded);
    }
    if let Ok(image) = LoadImage::parse(bytes, &TAG) {
        let reparsed =
            LoadImage::parse(bytes, &TAG).expect("re-parse of an accepted load image must succeed");
        assert_eq!(image, reparsed);
        for name in image.needed_libraries() {
            assert!(!name.is_empty());
        }
    }
    if let Ok(header) = ProcessStartHeader::from_bytes(bytes) {
        let redecoded = ProcessStartHeader::from_bytes(&header.to_le_bytes())
            .expect("round-trip of an accepted start header must succeed");
        assert_eq!(header, redecoded);
    }
    if let Ok(slot) = StringSlot::from_bytes(bytes) {
        let redecoded = StringSlot::from_bytes(&slot.to_le_bytes())
            .expect("round-trip of an accepted string slot must succeed");
        assert_eq!(slot, redecoded);
    }
    if let Ok(view) = ProcessStart::parse(bytes) {
        let reparsed = ProcessStart::parse(bytes)
            .expect("re-parse of an accepted startup vector must succeed");
        assert_eq!(view, reparsed);
        for i in 0..view.arg_count() {
            assert!(view.arg(i).is_some());
        }
        for i in 0..view.env_count() {
            assert!(view.env(i).is_some());
        }
    }
}

#[test]
fn every_corpus_entry_replays_cleanly() {
    for (name, bytes) in corpus() {
        // A panic in `replay` already fails the test; the explicit assert keeps
        // the failing entry's name in the output.
        replay(&bytes);
        assert!(!name.is_empty());
    }
}

// --- per-validating-decoder verdict locks --------------------------------

#[test]
fn loadimage_accepts_a_valid_image_with_needed_libraries() {
    let image =
        LoadImage::parse(&load_image(&[SYSTEM_RUNTIME_LIB]), &TAG).expect("valid image parses");
    let needed: Vec<&str> = image.needed_libraries().collect();
    assert_eq!(needed, vec![SYSTEM_RUNTIME_LIB]);
}

#[test]
fn loadimage_accepts_the_maximum_needed_table() {
    let image = LoadImage::parse(&load_image(&[SYSTEM_RUNTIME_LIB; LOAD_MAX_NEEDED]), &TAG)
        .expect("max needed table parses");
    assert_eq!(image.needed_libraries().count(), LOAD_MAX_NEEDED);
}

#[test]
fn loadimage_rejects_a_truncated_needed_table() {
    assert_eq!(
        LoadImage::parse(&image_with_needed_count(1), &TAG),
        Err(RxeError::BufferTooSmall)
    );
}

#[test]
fn loadimage_rejects_more_needed_than_the_cap() {
    let over = u16::try_from(LOAD_MAX_NEEDED + 1).expect("fits u16");
    assert_eq!(
        LoadImage::parse(&image_with_needed_count(over), &TAG),
        Err(RxeError::TooManyNeeded)
    );
}

#[test]
fn loadimage_rejects_a_non_pie_image() {
    assert_eq!(
        LoadImage::parse(&clear_pie_flag(load_image(&[])), &TAG),
        Err(RxeError::NotPositionIndependent)
    );
}

#[test]
fn loadimage_rejects_a_bad_magic() {
    assert_eq!(
        LoadImage::parse(&flip_byte(load_image(&[]), 0), &TAG),
        Err(RxeError::BadMagic)
    );
}

#[test]
fn needed_library_accepts_the_curated_runtime_reference() {
    let lib = NeededLibrary::decode(&needed_record()).expect("valid record");
    assert_eq!(lib.reference(), SYSTEM_RUNTIME_LIB);
}

#[test]
fn needed_library_rejects_zero_length() {
    assert_eq!(
        NeededLibrary::decode(&set_byte(needed_record(), 0, 0)),
        Err(RxeError::BadNeededLibrary)
    );
}

#[test]
fn needed_library_rejects_an_embedded_nul() {
    assert_eq!(
        NeededLibrary::decode(&set_byte(needed_record(), 2, 0)),
        Err(RxeError::BadNeededLibrary)
    );
}

#[test]
fn needed_library_rejects_dirty_padding() {
    assert_eq!(
        NeededLibrary::decode(&dirty_needed_padding()),
        Err(RxeError::BadNeededLibrary)
    );
}

#[test]
fn startup_vector_round_trips_through_parse() {
    let bytes = startup_vector();
    let view = ProcessStart::parse(&bytes).expect("valid startup vector parses");
    assert_eq!(view.arg_count(), 2);
    assert_eq!(view.arg(0), Some(&b"prog"[..]));
    assert_eq!(view.arg(1), Some(&b"42"[..]));
    assert_eq!(view.env_count(), 0);
    assert_eq!(view.canary(), CANARY);
}

#[test]
fn startup_vector_rejects_a_corrupted_magic() {
    assert!(ProcessStart::parse(&flip_byte(startup_vector(), 0)).is_err());
}
