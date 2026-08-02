//! Host unit tests for the rasterising [`FontService`] dispatcher.
//!
//! The four committed faces under `lib/font/assets/` are the same sources the
//! atlas generator uses, so the service is exercised against the real system
//! faces without any on-disk `/System/Fonts`. Each test drives a request
//! through [`FontService::handle`] and decodes the reply with the shared
//! `font_ipc` decoders, so the encode/decode contract is checked end to end.

use alloc::boxed::Box;
use alloc::vec;
use alloc::vec::Vec;
use std::path::PathBuf;

use tairix_abi::font_ipc::{
    decode_glyph_reply, decode_metrics_reply, FontRequest, FontWeight, FONT_MAX_CELL_HEIGHT,
    FONT_MAX_GLYPH_REPLY, FONT_MIN_CELL_HEIGHT,
};
use tairix_abi::Errno;
use tairix_font::glyph_cache_budget;
use tairix_fontface::Repertoire;
use tairix_log::DiscardSink;
use tairix_reclaim::{PressureBand, ReportedPressure};

use super::{glyph_cache, FontService, GlyphCache, FACE_REPERTOIRES};

/// A machine with plenty of RAM, so a test that is not about the bound gets a
/// cache that comfortably holds what it asks for.
const ROOMY_MACHINE_BYTES: u64 = 64 << 30;

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

/// A cache built exactly as the `Run` binary builds one — the shared
/// classification and the shared RAM-derived budget — but from a gauge the
/// test drives and a sink a host test has nowhere to send records to.
fn cache_for(total_ram_bytes: u64, band: PressureBand) -> (GlyphCache, &'static ReportedPressure) {
    static SINK: DiscardSink = DiscardSink;
    let gauge: &'static ReportedPressure = Box::leak(Box::new(ReportedPressure::unknown()));
    gauge.report(band);
    (glyph_cache(total_ram_bytes, gauge, &SINK), gauge)
}

/// A ready service over the committed faces, plus the byte buffers it borrows
/// (returned so the caller keeps them alive for the service's lifetime).
fn service(bytes: &[Vec<u8>]) -> FontService<'_> {
    service_with(
        bytes,
        cache_for(ROOMY_MACHINE_BYTES, PressureBand::Normal).0,
    )
}

/// A ready service over the committed faces holding exactly `cache`.
fn service_with(bytes: &[Vec<u8>], cache: GlyphCache) -> FontService<'_> {
    let sources: Vec<(&[u8], Repertoire)> = bytes
        .iter()
        .map(Vec::as_slice)
        .zip(FACE_REPERTOIRES)
        .collect();
    FontService::new(&sources, cache).expect("service parses the committed faces")
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

/// Ask for one glyph and return the decoded reply's geometry and coverage.
fn render(
    svc: &mut FontService<'_>,
    scalar: char,
    cell_height: u32,
    weight: FontWeight,
) -> Option<(u32, u32, Vec<u8>)> {
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
    let glyph = decode_glyph_reply(&reply[..n]).ok()?;
    Some((glyph.width, glyph.height, glyph.coverage.to_vec()))
}

/// The heights a hostile client would walk: the top of the permitted range,
/// where each raster is at its largest.
fn hostile_heights() -> impl Iterator<Item = u32> {
    (FONT_MAX_CELL_HEIGHT - 31)..=FONT_MAX_CELL_HEIGHT
}

#[test]
fn a_caller_walking_the_size_range_cannot_grow_the_service_past_its_budget() {
    // A machine whose budget holds a handful of the largest permitted
    // rasters, so the walk genuinely forces eviction rather than being
    // refused outright for exceeding the whole budget.
    let bytes = face_bytes();
    let (cache, _gauge) = cache_for(8 << 30, PressureBand::Normal);
    let ceiling = glyph_cache_budget(8 << 30).hard();
    let mut svc = service_with(&bytes, cache);

    for height in hostile_heights() {
        assert!(
            render(&mut svc, 'A', height, FontWeight::Regular).is_some(),
            "every permitted size is still served"
        );
        assert!(
            svc.cache.charged_bytes() <= ceiling,
            "a size walk pushed the service to {} bytes, past its {ceiling}-byte ceiling",
            svc.cache.charged_bytes()
        );
    }
    assert!(
        svc.cache.accounting().evictions() > 0,
        "the walk must have forced the bound to bite"
    );
    assert!(
        !svc.cache.poisoned(),
        "bounding a hostile caller is ordinary operation, not a defect"
    );
}

#[test]
fn a_size_outside_the_permitted_range_is_still_refused_and_rasterises_nothing() {
    let bytes = face_bytes();
    let mut svc = service(&bytes);
    for height in [
        FONT_MIN_CELL_HEIGHT - 1,
        FONT_MAX_CELL_HEIGHT + 1,
        0,
        u32::MAX,
    ] {
        let mut reply = vec![0u8; FONT_MAX_GLYPH_REPLY];
        let n = svc.handle(
            &FontRequest::Glyph {
                scalar: 'A',
                cell_height: height,
                weight: FontWeight::Regular,
            }
            .to_le_bytes(),
            &mut reply,
        );
        assert_eq!(
            decode_glyph_reply(&reply[..n]),
            Err(Errno::LengthOutOfRange),
            "cell height {height} must stay refused"
        );
    }
    assert_eq!(
        svc.cache.len(),
        0,
        "a refused request must never reach the rasteriser or the cache"
    );
}

#[test]
fn an_unknown_ram_size_caches_nothing_yet_still_serves() {
    let bytes = face_bytes();
    let (cache, _gauge) = cache_for(0, PressureBand::Normal);
    let mut svc = service_with(&bytes, cache);
    let served = render(&mut svc, 'A', 28, FontWeight::Regular).expect("still served");
    assert_eq!(served.2.len(), (served.0 * served.1) as usize);
    assert!(served.2.contains(&255), "the raster is real, not blank");
    assert_eq!(svc.cache.len(), 0, "a zero budget retains nothing");
    assert_eq!(svc.cache.charged_bytes(), 0);
}

#[test]
fn mild_pressure_empties_the_cache_and_refuses_further_growth() {
    let bytes = face_bytes();
    let (cache, gauge) = cache_for(ROOMY_MACHINE_BYTES, PressureBand::Normal);
    let mut svc = service_with(&bytes, cache);
    assert!(render(&mut svc, 'A', 28, FontWeight::Regular).is_some());
    assert_eq!(svc.cache.len(), 1);

    gauge.report(PressureBand::Mild);
    assert!(svc.trim_cache() > 0, "mild pressure must release");
    assert_eq!(svc.cache.len(), 0);
    assert_eq!(svc.cache.charged_bytes(), 0);

    assert!(
        render(&mut svc, 'B', 28, FontWeight::Regular).is_some(),
        "a shrunk service still rasterises"
    );
    assert_eq!(svc.cache.len(), 0, "no growth while the band forbids it");
}

#[test]
fn a_glyph_rasterises_identically_cached_uncached_and_after_a_shrink() {
    let bytes = face_bytes();
    let (uncached, _uncached_gauge) = cache_for(0, PressureBand::Normal);
    let mut without = service_with(&bytes, uncached);
    let expected = render(&mut without, 'g', 24, FontWeight::Bold).expect("served with no cache");

    let (cache, gauge) = cache_for(ROOMY_MACHINE_BYTES, PressureBand::Normal);
    let mut with = service_with(&bytes, cache);
    assert_eq!(
        render(&mut with, 'g', 24, FontWeight::Bold),
        Some(expected.clone())
    );
    assert_eq!(
        render(&mut with, 'g', 24, FontWeight::Bold),
        Some(expected.clone()),
        "a cache hit serves the same raster the miss did"
    );

    gauge.report(PressureBand::Mild);
    let _ = with.trim_cache();
    assert_eq!(
        render(&mut with, 'g', 24, FontWeight::Bold),
        Some(expected),
        "the cache is an accelerator; losing it changes nothing"
    );
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
    let (cache, _gauge) = cache_for(ROOMY_MACHINE_BYTES, PressureBand::Normal);
    assert!(FontService::new(&sources, cache).is_err());
}
