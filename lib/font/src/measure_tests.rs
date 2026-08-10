//! Unit tests for the text-measurement memo and the three measuring
//! operations that read it.
//!
//! Every test drives a client of its own, never the process-global one, so a
//! lookup count is a fact about *this* measurement rather than about whatever
//! else the harness is running in parallel.

use alloc::string::String;

use tairix_log::DiscardSink;
use tairix_reclaim::{CacheBudget, CachedBytes, PressureBand, ReclaimOwner, ReportedPressure};

use super::tests::{cache_at, caching_client, client_with, glyph_lookups, INTER};
use super::*;

use crate::font::{BitmapFont, ELLIPSIS};
use crate::glyph_cache::glyph_cache_budget;
use crate::measure::{self, measure_cache_candidate};

/// A second proportional family, to show a face is part of the key.
const NOTO: FamilyKey = match FamilyKey::new("noto") {
    Ok(key) => key,
    Err(_) => FamilyKey::MONO,
};

const HEIGHT: u32 = 20;

/// A client with both caches on one gauge, so a pressure test moves both, and
/// with the memo built on `memo_budget` so a refusal can be exercised.
fn measuring_client(
    band: PressureBand,
    memo_budget: CacheBudget,
) -> (GlyphClient, &'static ReportedPressure) {
    static SINK: DiscardSink = DiscardSink;

    let (mut client, gauge) = caching_client(band, glyph_cache_budget(1 << 30));
    client.measure = Some(ReclaimCache::new(
        "test.font.measure",
        measure_cache_candidate(ReclaimOwner::UserlandProcess("test.font")),
        memo_budget,
        gauge,
        &SINK,
    ));
    (client, gauge)
}

/// A comfortable machine: room for both caches, band reported normal.
fn roomy_client() -> (GlyphClient, &'static ReportedPressure) {
    measuring_client(PressureBand::Normal, glyph_cache_budget(1 << 30))
}

/// Lookups the memo itself has answered or missed.
fn memo_lookups(client: &GlyphClient) -> u64 {
    let accounting = client
        .measure
        .as_ref()
        .expect("a memo is installed")
        .accounting();
    accounting.hits() + accounting.misses()
}

fn memo_entries(client: &GlyphClient) -> usize {
    client.measure.as_ref().expect("a memo is installed").len()
}

fn char_count(text: &str) -> u64 {
    u64::try_from(text.chars().count()).expect("a test string")
}

/// The width as it was computed before the memo: one advance lookup per
/// character, summed with saturating arithmetic.
///
/// An independent reference, deliberately not sharing code with the thing it
/// checks, so a differential test can catch the memo answering differently
/// from the walk it replaced.
fn reference_width(client: &mut GlyphClient, family: FamilyKey, text: &str) -> u32 {
    text.chars().fold(0u32, |width, scalar| {
        let advance = client
            .with_glyph(scalar, family, HEIGHT, FontWeight::Regular, |glyph| {
                glyph.advance
            })
            .unwrap_or(0);
        width.saturating_add(advance)
    })
}

/// The truncation as it was computed before the memo.
fn reference_truncate<'a>(
    client: &mut GlyphClient,
    family: FamilyKey,
    text: &'a str,
    width: u32,
) -> &'a str {
    let mut used = 0u32;
    let mut end = 0usize;
    for ch in text.chars() {
        let advance = client
            .with_glyph(ch, family, HEIGHT, FontWeight::Regular, |glyph| {
                glyph.advance
            })
            .unwrap_or(0);
        let next = used.saturating_add(advance);
        if next > width {
            break;
        }
        used = next;
        end += ch.len_utf8();
    }
    &text[..end]
}

/// The elision as it was computed before the memo.
fn reference_elide<'a>(
    client: &mut GlyphClient,
    family: FamilyKey,
    text: &'a str,
    width: u32,
) -> (&'a str, bool) {
    if reference_truncate(client, family, text, width).len() == text.len() {
        return (text, false);
    }
    let Some(room) = width.checked_sub(reference_width(client, family, ELLIPSIS)) else {
        return ("", false);
    };
    (reference_truncate(client, family, text, room), true)
}

/// Strings a label is made of in practice, plus the degenerate ones.
fn corpus() -> [String; 6] {
    [
        String::new(),
        String::from("a"),
        String::from("Switchboard"),
        String::from("é"),
        String::from("日本語のラベル"),
        "abcdefghij".repeat(50),
    ]
}

#[test]
fn measuring_the_same_string_twice_looks_up_its_advances_once() {
    let (mut client, _gauge) = roomy_client();
    let font = BitmapFont::new(NOTO, HEIGHT);
    let text = "Switchboard";

    let first = font.width_on(&mut client, text);
    assert_eq!(glyph_lookups(&client), char_count(text));

    let second = font.width_on(&mut client, text);
    assert_eq!(second, first);
    assert_eq!(
        glyph_lookups(&client),
        char_count(text),
        "the second measurement walked the string again"
    );
    assert_eq!(memo_entries(&client), 1);
}

#[test]
fn truncation_and_elision_read_the_same_one_measurement() {
    let (mut client, _gauge) = roomy_client();
    let font = BitmapFont::new(NOTO, HEIGHT);
    let text = "Switchboard";

    let width = font.width_on(&mut client, text);
    let walked = glyph_lookups(&client);
    assert_eq!(walked, char_count(text));

    font.fitting_bytes_on(&mut client, text, width / 2);
    assert_eq!(glyph_lookups(&client), walked, "truncation re-walked");

    // Elision measures the mark as well, hence exactly one further entry and
    // one further character walked.
    font.elision_on(&mut client, text, width / 2);
    assert_eq!(
        glyph_lookups(&client),
        walked + char_count(ELLIPSIS),
        "elision re-walked the label"
    );
    assert_eq!(memo_entries(&client), 2);
}

#[test]
fn a_monospace_family_pays_no_advance_lookup_and_no_memo_lookup() {
    let (mut client, _gauge) = roomy_client();
    let font = BitmapFont::monospace(HEIGHT);
    let text = "Switchboard";

    let cell = font
        .monospace_advance_on(&mut client)
        .expect("the test transport serves MONO as fixed-pitch");
    assert_eq!(
        font.width_on(&mut client, text),
        cell * u32::try_from(text.len()).expect("ASCII")
    );
    font.fitting_bytes_on(&mut client, text, 3 * cell);
    font.elision_on(&mut client, text, 3 * cell);

    assert_eq!(
        glyph_lookups(&client),
        0,
        "the monospace path asked for a glyph advance"
    );
    assert_eq!(
        memo_lookups(&client),
        0,
        "the monospace path consulted the memo"
    );
    assert_eq!(memo_entries(&client), 0);
}

#[test]
fn every_memoised_answer_equals_the_uncached_walk() {
    let (mut client, _gauge) = roomy_client();
    let font = BitmapFont::new(NOTO, HEIGHT);

    for text in corpus() {
        for width in [0, 1, 7, 40, 200, u32::MAX] {
            let measured = font.width_on(&mut client, &text);
            assert_eq!(
                measured,
                reference_width(&mut client, NOTO, &text),
                "width of {text:?}"
            );

            let end = font.fitting_bytes_on(&mut client, &text, width);
            assert_eq!(
                &text[..end],
                reference_truncate(&mut client, NOTO, &text, width),
                "truncation of {text:?} to {width}"
            );

            let (end, elided) = font.elision_on(&mut client, &text, width);
            assert_eq!(
                (&text[..end], elided),
                reference_elide(&mut client, NOTO, &text, width),
                "elision of {text:?} to {width}"
            );
        }
    }
}

#[test]
fn a_scale_or_face_change_is_a_separate_measurement() {
    let (mut client, _gauge) = roomy_client();
    let text = "Switchboard";
    let walked = char_count(text);

    let small = BitmapFont::new(NOTO, 16);
    let large = BitmapFont::new(NOTO, 32);
    let other_face = BitmapFont::new(INTER, 16);

    let at_small = small.width_on(&mut client, text);
    assert_eq!(glyph_lookups(&client), walked);

    let at_large = large.width_on(&mut client, text);
    assert_ne!(at_small, at_large, "a scale change measured the same");
    assert_eq!(
        glyph_lookups(&client),
        2 * walked,
        "the larger scale was served the smaller one's measurement"
    );

    let at_other_face = other_face.width_on(&mut client, text);
    assert_eq!(
        glyph_lookups(&client),
        3 * walked,
        "the second face was served the first face's measurement"
    );
    assert_eq!(at_other_face, at_small, "the test faces measure alike");
    assert_eq!(memo_entries(&client), 3);
}

#[test]
fn a_new_advance_source_invalidates_every_retained_measurement() {
    let (mut client, _gauge) = roomy_client();
    let font = BitmapFont::new(NOTO, HEIGHT);
    let text = "Switchboard";

    let before = font.width_on(&mut client, text);
    assert_eq!(memo_entries(&client), 1);

    client.install_transport(Box::new(SolidTestTransport));

    let after = font.width_on(&mut client, text);
    assert_eq!(
        after, before,
        "the fresh walk disagreed with the retained one"
    );
    assert_eq!(memo_entries(&client), 1, "a stale entry survived");
    assert_eq!(
        client
            .measure
            .as_ref()
            .expect("a memo is installed")
            .accounting()
            .invalidations(),
        1
    );
    assert_eq!(
        glyph_lookups(&client),
        2 * char_count(text),
        "the measurement was served from the old source's entry"
    );
}

#[test]
fn a_pressured_or_unbudgeted_memo_retains_nothing_and_still_measures() {
    let font = BitmapFont::new(NOTO, HEIGHT);
    let text = "Switchboard";

    // A band that forbids growth, and a machine whose RAM reading gave a zero
    // budget: both refuse every entry, and both must still answer correctly.
    for (band, budget) in [
        (PressureBand::Mild, glyph_cache_budget(1 << 30)),
        (PressureBand::Normal, glyph_cache_budget(0)),
    ] {
        let (mut client, _gauge) = measuring_client(band, budget);
        let measured = font.width_on(&mut client, text);
        assert_eq!(measured, reference_width(&mut client, NOTO, text));
        assert!(measured > 0);
        assert_eq!(memo_entries(&client), 0);
        font.width_on(&mut client, text);
        assert_eq!(memo_entries(&client), 0);
    }
}

#[test]
fn a_band_that_tightens_hands_the_retained_measurements_back() {
    let (mut client, gauge) = roomy_client();
    let font = BitmapFont::new(NOTO, HEIGHT);
    let text = "Switchboard";

    let before = font.width_on(&mut client, text);
    assert_eq!(memo_entries(&client), 1);

    gauge.report(PressureBand::Mild);
    assert!(client.trim() > 0, "the trim released nothing");
    assert_eq!(memo_entries(&client), 0);
    assert_eq!(
        font.width_on(&mut client, text),
        before,
        "the re-walk disagreed with the released measurement"
    );
}

#[test]
fn a_service_that_answers_metrics_but_no_glyph_measures_zero_and_retains_nothing() {
    /// The font service dying between a metrics answer and the advances a
    /// measurement needs: the family reads as proportional, every glyph is
    /// refused.
    struct MetricsOnly;
    impl FontTransport for MetricsOnly {
        fn call(&mut self, request: &[u8], reply: &mut [u8]) -> Result<usize, Errno> {
            match FontRequest::from_bytes(request)? {
                FontRequest::Metrics { .. } => SolidTestTransport.call(request, reply),
                _ => Err(Errno::NotFound),
            }
        }
    }

    static SINK: DiscardSink = DiscardSink;

    let (_, gauge) = cache_at(PressureBand::Normal, glyph_cache_budget(1 << 30));
    let mut client = client_with(MetricsOnly);
    client.measure = Some(ReclaimCache::new(
        "test.font.measure",
        measure_cache_candidate(ReclaimOwner::UserlandProcess("test.font")),
        glyph_cache_budget(1 << 30),
        gauge,
        &SINK,
    ));
    let font = BitmapFont::new(NOTO, HEIGHT);
    let text = "Switchboard";

    assert_eq!(font.width_on(&mut client, text), 0);
    assert_eq!(font.fitting_bytes_on(&mut client, text, 100), text.len());
    assert_eq!(font.elision_on(&mut client, text, 100), (text.len(), false));
    assert_eq!(
        memo_entries(&client),
        0,
        "a walk the service could not complete was remembered"
    );
}

#[test]
fn a_measurement_answers_its_queries_charges_its_bytes_and_wipes_them() {
    let (mut measured, resolved) = measure::measure("abc", |_| Some(4));
    assert!(resolved);
    assert_eq!(measured.width(), 12);
    assert_eq!(measured.chars_within(0), 0);
    assert_eq!(measured.chars_within(7), 1);
    assert_eq!(measured.chars_within(u32::MAX), 3);
    assert_eq!(measured.payload_bytes(), 3 * size_of::<u32>() + 3);
    assert!(measured.is_of("abc"));
    assert!(!measured.is_of("abd"), "a clashing string would be served");

    measured.wipe();
    assert_eq!(measured.width(), 0);
    assert!(!measured.is_of("abc"), "released text stayed readable");
}

#[test]
fn an_empty_string_measures_to_nothing_and_an_unresolved_advance_is_reported() {
    let (empty, resolved) = measure::measure("", |_| Some(4));
    assert!(resolved);
    assert_eq!(empty.width(), 0);
    assert_eq!(empty.chars_within(0), 0);
    assert_eq!(empty.payload_bytes(), 0);

    let (partial, resolved) = measure::measure("ab", |ch| (ch == 'a').then_some(5));
    assert!(!resolved);
    assert_eq!(partial.width(), 5);
}

#[test]
fn a_measurement_key_separates_face_scale_weight_and_text() {
    let base = measure::measure_key(NOTO, HEIGHT, FontWeight::Regular, "hello");
    assert_eq!(
        base,
        measure::measure_key(NOTO, HEIGHT, FontWeight::Regular, "hello")
    );
    assert_ne!(
        base,
        measure::measure_key(INTER, HEIGHT, FontWeight::Regular, "hello")
    );
    assert_ne!(
        base,
        measure::measure_key(NOTO, HEIGHT + 1, FontWeight::Regular, "hello")
    );
    assert_ne!(
        base,
        measure::measure_key(NOTO, HEIGHT, FontWeight::Bold, "hello")
    );
    assert_ne!(
        base,
        measure::measure_key(NOTO, HEIGHT, FontWeight::Regular, "hellp")
    );
    assert_ne!(
        base,
        measure::measure_key(NOTO, HEIGHT, FontWeight::Regular, "hell")
    );
}
