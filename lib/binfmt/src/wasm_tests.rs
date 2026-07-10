//! Behavioural tests for the wasm module-structure view: a hand-assembled
//! fixture module, the success paths, and the malformed-LEB/framing
//! refusal matrix.

use super::{
    BodyRange, WasmError, WasmView, MAX_MODULE_SECTIONS, SECTION_CODE, SECTION_FUNCTION,
    SECTION_TYPE,
};
use alloc::vec::Vec;

/// Body 0: no locals, `nop`, `end`.
const BODY_0: &[u8] = &[0x00, 0x01, 0x0B];
/// Body 1: no locals, `nop`, `nop`, `end`.
const BODY_1: &[u8] = &[0x00, 0x01, 0x01, 0x0B];

fn push_section(out: &mut Vec<u8>, id: u8, payload: &[u8]) {
    out.push(id);
    let size = u8::try_from(payload.len()).expect("test payloads stay single-byte LEB");
    out.push(size);
    out.extend_from_slice(payload);
}

/// A module with a custom section, one function type, two functions, and
/// a two-body code section.
fn fixture() -> Vec<u8> {
    let mut out = Vec::new();
    out.extend_from_slice(b"\0asm");
    out.extend_from_slice(&1u32.to_le_bytes());
    push_section(&mut out, 0, b"\x04name");
    push_section(&mut out, SECTION_TYPE, &[0x01, 0x60, 0x00, 0x00]);
    push_section(&mut out, SECTION_FUNCTION, &[0x02, 0x00, 0x00]);
    let mut code = Vec::new();
    code.push(0x02); // body count
    code.push(u8::try_from(BODY_0.len()).expect("fits"));
    code.extend_from_slice(BODY_0);
    code.push(u8::try_from(BODY_1.len()).expect("fits"));
    code.extend_from_slice(BODY_1);
    push_section(&mut out, SECTION_CODE, &code);
    out
}

/// A header-only module followed by the given raw section-directory bytes.
fn module_with(directory: &[u8]) -> Vec<u8> {
    let mut out = Vec::new();
    out.extend_from_slice(b"\0asm");
    out.extend_from_slice(&1u32.to_le_bytes());
    out.extend_from_slice(directory);
    out
}

#[test]
fn parses_the_fixture_directory() {
    let bytes = fixture();
    let view = WasmView::parse(&bytes).expect("valid module");
    let ids: Vec<u8> = view.sections().iter().map(|s| s.id).collect();
    assert_eq!(ids, [0, SECTION_TYPE, SECTION_FUNCTION, SECTION_CODE]);
    let custom = &view.sections()[0];
    assert_eq!(view.section_bytes(custom), b"\x04name");
}

#[test]
fn reports_vector_section_counts() {
    let bytes = fixture();
    let view = WasmView::parse(&bytes).expect("valid module");
    assert_eq!(view.entry_count(SECTION_TYPE), Ok(Some(1)));
    assert_eq!(view.entry_count(SECTION_FUNCTION), Ok(Some(2)));
    // Absent section: an answer, not an error.
    assert_eq!(view.entry_count(5), Ok(None));
}

#[test]
fn walks_function_bodies_with_exact_framing() {
    let bytes = fixture();
    let view = WasmView::parse(&bytes).expect("valid module");
    let mut bodies = view.code_bodies().expect("count decodes").expect("present");
    assert_eq!(bodies.declared(), 2);

    let code = view.section(SECTION_CODE).expect("code section");
    let first = bodies.next().expect("body 0").expect("valid body");
    assert_eq!(
        first,
        BodyRange {
            index: 0,
            offset: code.offset + 2,
            size: BODY_0.len(),
        }
    );
    assert_eq!(&bytes[first.offset..first.offset + first.size], BODY_0);

    let second = bodies.next().expect("body 1").expect("valid body");
    assert_eq!(second.index, 1);
    assert_eq!(&bytes[second.offset..second.offset + second.size], BODY_1);

    assert!(bodies.next().is_none());
}

#[test]
fn a_module_without_code_has_no_bodies() {
    let bytes = module_with(&[]);
    let view = WasmView::parse(&bytes).expect("empty module is valid");
    assert!(view.code_bodies().expect("no decode needed").is_none());
    assert!(view.sections().is_empty());
}

#[test]
fn refuses_bad_magic_and_version() {
    assert_eq!(WasmView::parse(b"").err(), Some(WasmError::TooSmall));
    assert_eq!(
        WasmView::parse(b"\0asm\x01\x00\x00").err(),
        Some(WasmError::TooSmall)
    );
    assert_eq!(
        WasmView::parse(b"wasm\x01\x00\x00\x00").err(),
        Some(WasmError::BadMagic)
    );
    assert_eq!(
        WasmView::parse(b"\0asm\x02\x00\x00\x00").err(),
        Some(WasmError::UnsupportedVersion)
    );
}

#[test]
fn refuses_malformed_section_lengths() {
    // Overlong LEB: padding bits set in the fifth byte.
    let overlong = module_with(&[1, 0x80, 0x80, 0x80, 0x80, 0x70]);
    assert_eq!(WasmView::parse(&overlong).err(), Some(WasmError::BadLeb));

    // Six-byte continuation: the fifth byte still has its high bit set.
    let too_long = module_with(&[1, 0x80, 0x80, 0x80, 0x80, 0x80, 0x00]);
    assert_eq!(WasmView::parse(&too_long).err(), Some(WasmError::BadLeb));

    // Unterminated LEB at end of input.
    let unterminated = module_with(&[1, 0x80]);
    assert_eq!(
        WasmView::parse(&unterminated).err(),
        Some(WasmError::TooSmall)
    );

    // Declared size runs past the file.
    let oversize = module_with(&[1, 0x7F]);
    assert_eq!(
        WasmView::parse(&oversize).err(),
        Some(WasmError::OutOfBounds)
    );
}

#[test]
fn refuses_unknown_and_duplicate_sections() {
    let unknown = module_with(&[13, 0x00]);
    assert_eq!(
        WasmView::parse(&unknown).err(),
        Some(WasmError::BadSectionId)
    );

    let duplicate = module_with(&[1, 0x01, 0x00, 1, 0x01, 0x00]);
    assert_eq!(
        WasmView::parse(&duplicate).err(),
        Some(WasmError::DuplicateSection)
    );

    // Two custom sections are legitimate.
    let customs = module_with(&[0, 0x01, 0x00, 0, 0x01, 0x00]);
    let view = WasmView::parse(&customs).expect("customs may repeat");
    assert_eq!(view.sections().len(), 2);
}

#[test]
fn enforces_the_section_directory_cap() {
    let mut directory = Vec::new();
    for _ in 0..=MAX_MODULE_SECTIONS {
        directory.extend_from_slice(&[0, 0x00]); // empty custom section
    }
    let bytes = module_with(&directory);
    assert_eq!(
        WasmView::parse(&bytes).err(),
        Some(WasmError::TooManySections)
    );
}

#[test]
fn a_body_extent_past_the_section_fails_closed() {
    // Code section: one body claiming 10 bytes with only 2 present.
    let bytes = module_with(&[SECTION_CODE, 0x04, 0x01, 0x0A, 0x00, 0x0B]);
    let view = WasmView::parse(&bytes).expect("directory is well-formed");
    let mut bodies = view.code_bodies().expect("count decodes").expect("present");
    assert_eq!(bodies.next(), Some(Err(WasmError::OutOfBounds)));
    assert!(bodies.next().is_none());
}

#[test]
fn leftover_code_payload_fails_closed() {
    // One declared body of 1 byte, then a stray byte the framing never
    // accounts for.
    let bytes = module_with(&[SECTION_CODE, 0x04, 0x01, 0x01, 0x0B, 0xAA]);
    let view = WasmView::parse(&bytes).expect("directory is well-formed");
    let mut bodies = view.code_bodies().expect("count decodes").expect("present");
    assert!(bodies.next().expect("body 0").is_ok());
    assert_eq!(bodies.next(), Some(Err(WasmError::TrailingBytes)));
    assert!(bodies.next().is_none());
}

#[test]
fn a_malformed_body_count_fails_closed() {
    let bytes = module_with(&[SECTION_CODE, 0x01, 0x80]);
    let view = WasmView::parse(&bytes).expect("directory is well-formed");
    assert_eq!(view.code_bodies().err(), Some(WasmError::TooSmall));
}

/// Walk everything a view exposes, ignoring per-item errors.
fn exercise(view: &WasmView<'_>) {
    for entry in view.sections() {
        let _ = view.section_bytes(entry);
        let _ = view.entry_count(entry.id);
    }
    if let Ok(Some(bodies)) = view.code_bodies() {
        for body in bodies {
            let _ = body;
        }
    }
}

#[test]
fn truncations_and_flips_never_panic() {
    let good = fixture();
    // A truncation at a section boundary is a legitimately shorter
    // module, so the sweep asserts safety, not refusal — except the
    // header, which must always be present.
    for len in 0..good.len() {
        if let Ok(view) = WasmView::parse(&good[..len]) {
            assert!(len >= 8, "a module shorter than its header must fail");
            exercise(&view);
        }
    }
    for i in 0..good.len() {
        let mut mutated = good.clone();
        mutated[i] ^= 0x40;
        if let Ok(view) = WasmView::parse(&mutated) {
            exercise(&view);
        }
    }
}
