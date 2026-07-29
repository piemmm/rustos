//! Host unit tests for the rasterising [`FontService`] dispatcher.
//!
//! The four committed faces under `lib/font/assets/` are the same sources the
//! atlas generator uses, so the service is exercised against the real system
//! faces without any on-disk `/System/Fonts`. Each test drives a request
//! through [`FontService::handle`] and decodes the reply with the shared
//! `font_ipc` decoders, so the encode/decode contract is checked end to end.

use alloc::vec;
use alloc::vec::Vec;
use std::path::PathBuf;

use tairix_abi::font_ipc::{
    decode_glyph_reply, decode_metrics_reply, FontRequest, FontWeight, FONT_MAX_CELL_HEIGHT,
    FONT_MAX_GLYPH_REPLY, FONT_MIN_CELL_HEIGHT,
};
use tairix_abi::Errno;
use tairix_fontface::Repertoire;

use super::{FontService, FACE_REPERTOIRES};

/// The workspace root (the fontd crate is `userland/system/fontd`).
fn workspace_root() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .join("../../..")
        .canonicalize()
        .expect("workspace root")
}

/// The committed face bytes, in the family's resolution order.
fn face_bytes() -> Vec<Vec<u8>> {
    [
        "lib/font/assets/Inconsolata-EX.ttf",
        "lib/font/assets/MPLUS1Code-Regular.ttf",
        "lib/font/assets/D2Coding-Regular.ttf",
        "lib/font/assets/NotoSansHebrew-ExtraCondensed.ttf",
    ]
    .iter()
    .map(|rel| std::fs::read(workspace_root().join(rel)).expect("committed face"))
    .collect()
}

/// A ready service over the committed faces, plus the byte buffers it borrows
/// (returned so the caller keeps them alive for the service's lifetime).
fn service(bytes: &[Vec<u8>]) -> FontService<'_> {
    let sources: Vec<(&[u8], Repertoire)> = bytes
        .iter()
        .map(Vec::as_slice)
        .zip(FACE_REPERTOIRES)
        .collect();
    FontService::new(&sources).expect("service parses the committed faces")
}

#[test]
fn parses_the_committed_faces_and_derives_native_geometry() {
    let bytes = face_bytes();
    let svc = service(&bytes);
    // Native metrics match the atlas generator's derivation (Inconsolata EX
    // at a 25 px em: 15 × 28, baseline 23).
    let metrics = svc.metrics(28);
    assert_eq!(metrics.cell_width, 15);
    assert_eq!(metrics.cell_height, 28);
    assert_eq!(metrics.baseline, 23);
}

#[test]
fn a_glyph_request_round_trips_with_ink() {
    let bytes = face_bytes();
    let mut svc = service(&bytes);
    let mut reply = vec![0u8; FONT_MAX_GLYPH_REPLY];
    let n = svc.handle(
        &FontRequest::Glyph {
            scalar: 'A',
            cell_height: 28,
            weight: FontWeight::Regular,
        }
        .to_le_bytes(),
        &mut reply,
    );
    let glyph = decode_glyph_reply(&reply[..n]).expect("glyph reply decodes");
    // Bitmap is two cells wide; 'A' is single-cell so advance is one cell.
    assert_eq!(glyph.width, 30);
    assert_eq!(glyph.height, 28);
    assert_eq!(glyph.advance, 15);
    assert_eq!(glyph.coverage.len(), (glyph.width * glyph.height) as usize);
    assert!(
        glyph.coverage.contains(&255),
        "'A' has no fully covered pixel"
    );
    // Coverage is genuine 8-bit: 4-bit engine output scaled ×17, so every
    // sample is a multiple of 17 (0..=255).
    assert!(glyph.coverage.iter().all(|&c| c % 17 == 0));
}

#[test]
fn a_wide_scalar_advances_two_cells_and_inks_the_continuation() {
    let bytes = face_bytes();
    let mut svc = service(&bytes);
    let mut reply = vec![0u8; FONT_MAX_GLYPH_REPLY];
    // U+6F22 (漢) is a wide Japanese glyph the M PLUS companion covers.
    let n = svc.handle(
        &FontRequest::Glyph {
            scalar: '漢',
            cell_height: 28,
            weight: FontWeight::Regular,
        }
        .to_le_bytes(),
        &mut reply,
    );
    let glyph = decode_glyph_reply(&reply[..n]).expect("wide glyph decodes");
    assert_eq!(glyph.advance, 30, "a wide glyph advances two cells");
    // Ink reaches into the continuation (right) cell.
    let cell_w = (glyph.width / 2) as usize;
    let reaches_continuation = glyph
        .coverage
        .chunks(glyph.width as usize)
        .any(|row| row[cell_w..].iter().any(|&c| c > 0));
    assert!(
        reaches_continuation,
        "wide glyph never reaches its second cell"
    );
}

#[test]
fn an_uncovered_scalar_falls_back_to_the_replacement_glyph() {
    let bytes = face_bytes();
    let mut svc = service(&bytes);
    let mut reply = vec![0u8; FONT_MAX_GLYPH_REPLY];
    // A private-use scalar no face maps resolves to U+FFFD, which has ink.
    let n = svc.handle(
        &FontRequest::Glyph {
            scalar: '\u{10FFFF}',
            cell_height: 28,
            weight: FontWeight::Regular,
        }
        .to_le_bytes(),
        &mut reply,
    );
    let glyph = decode_glyph_reply(&reply[..n]).expect("fallback decodes");
    assert!(
        glyph.coverage.iter().any(|&c| c > 0),
        "the U+FFFD fallback has no ink"
    );
}

#[test]
fn a_second_request_is_served_from_cache_identically() {
    let bytes = face_bytes();
    let mut svc = service(&bytes);
    let request = FontRequest::Glyph {
        scalar: 'g',
        cell_height: 24,
        weight: FontWeight::Regular,
    }
    .to_le_bytes();
    let mut first = vec![0u8; FONT_MAX_GLYPH_REPLY];
    let mut second = vec![0u8; FONT_MAX_GLYPH_REPLY];
    let n1 = svc.handle(&request, &mut first);
    let n2 = svc.handle(&request, &mut second);
    assert_eq!(n1, n2);
    assert_eq!(first[..n1], second[..n2], "cache hit differs from miss");
}

/// The total ink (summed 8-bit coverage) and the geometry of one glyph reply.
///
/// Summing rather than holding the bitmap lets a weight test compare two
/// rasters without keeping the borrowed reply buffer alive.
fn render_ink(
    svc: &mut FontService<'_>,
    scalar: char,
    cell_height: u32,
    weight: FontWeight,
) -> (u32, u32, u32, u32) {
    let mut reply = vec![0u8; FONT_MAX_GLYPH_REPLY];
    let n = svc.handle(
        &FontRequest::Glyph {
            scalar,
            cell_height,
            weight,
        }
        .to_le_bytes(),
        &mut reply,
    );
    let glyph = decode_glyph_reply(&reply[..n]).expect("glyph reply decodes");
    let ink = glyph.coverage.iter().map(|&c| u32::from(c)).sum();
    (ink, glyph.width, glyph.height, glyph.advance)
}

#[test]
fn a_heavier_weight_inks_more_of_the_same_cell() {
    let bytes = face_bytes();
    let mut svc = service(&bytes);
    let regular = render_ink(&mut svc, 'H', 28, FontWeight::Regular);
    let medium = render_ink(&mut svc, 'H', 28, FontWeight::Medium);
    let bold = render_ink(&mut svc, 'H', 28, FontWeight::Bold);

    assert!(
        regular.0 < medium.0 && medium.0 < bold.0,
        "weights do not read as a rising progression: {regular:?} {medium:?} {bold:?}"
    );
    // The geometry a client laid out with must not move with the weight.
    assert_eq!(regular.1, bold.1);
    assert_eq!(regular.2, bold.2);
    assert_eq!(regular.3, bold.3);
}

#[test]
fn weight_keys_the_cache_so_a_regular_run_is_never_served_bold() {
    let bytes = face_bytes();
    let mut mixed = service(&bytes);
    let _ = render_ink(&mut mixed, 'g', 24, FontWeight::Bold);
    let after_bold = render_ink(&mut mixed, 'g', 24, FontWeight::Regular);

    let mut fresh = service(&bytes);
    let alone = render_ink(&mut fresh, 'g', 24, FontWeight::Regular);

    assert_eq!(
        after_bold, alone,
        "a cached bold raster leaked into Regular"
    );
}

#[test]
fn metrics_scale_with_cell_height() {
    let bytes = face_bytes();
    let mut svc = service(&bytes);
    let small = svc.metrics(FONT_MIN_CELL_HEIGHT);
    let large = svc.metrics(FONT_MAX_CELL_HEIGHT);
    assert_eq!(small.cell_height, FONT_MIN_CELL_HEIGHT);
    assert_eq!(large.cell_height, FONT_MAX_CELL_HEIGHT);
    assert!(small.cell_width >= 1);
    assert!(large.cell_width > small.cell_width);
    assert!(large.baseline > small.baseline);
    // A metrics request round-trips the same geometry the accessor reports.
    let expected = svc.metrics(24);
    let mut reply = vec![0u8; FONT_MAX_GLYPH_REPLY];
    let n = svc.handle(
        &FontRequest::Metrics { cell_height: 24 }.to_le_bytes(),
        &mut reply,
    );
    let decoded = decode_metrics_reply(&reply[..n]).expect("metrics decode");
    assert_eq!(decoded, expected);
}

#[test]
fn a_malformed_request_fails_closed_with_an_error_frame() {
    let bytes = face_bytes();
    let mut svc = service(&bytes);
    let mut reply = vec![0u8; FONT_MAX_GLYPH_REPLY];
    // Corrupt magic: the decoder rejects it and the service replies an error
    // frame both a glyph- and a metrics-expecting client decode as the errno.
    let mut request = FontRequest::Glyph {
        scalar: 'A',
        cell_height: 28,
        weight: FontWeight::Regular,
    }
    .to_le_bytes();
    request[0] ^= 0xFF;
    let n = svc.handle(&request, &mut reply);
    assert_eq!(decode_glyph_reply(&reply[..n]), Err(Errno::BadMagic));
}

#[test]
fn a_truncated_face_fails_service_construction() {
    let bytes = face_bytes();
    let mut truncated = bytes.clone();
    truncated[0].truncate(64);
    let sources: Vec<(&[u8], Repertoire)> = truncated
        .iter()
        .map(Vec::as_slice)
        .zip(FACE_REPERTOIRES)
        .collect();
    assert!(FontService::new(&sources).is_err());
}
