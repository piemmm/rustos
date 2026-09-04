//! Host unit tests for the [`FontService`] dispatcher.
//!
//! Fixtures are built from the small committed assets
//! (`mono/Inconsolata-EX.ttf`, static; `inter/Inter-Variable.ttf`, variable;
//! `mono/NotoSansHebrew-ExtraCondensed.ttf`, static and tiny) through the
//! in-memory [`MemoryStore`] and the real [`discover`] scan, so the whole
//! discovery-to-serve pipeline is exercised end to end without any
//! `/System/Fonts` and without the multi-megabyte CJK companion faces.

use std::path::PathBuf;

use alloc::boxed::Box;
use alloc::vec;
use alloc::vec::Vec;

use tairix_abi::font_ipc::{
    decode_glyphs_reply, decode_metrics_reply, FamilyKey, FontRequest, FontWeight, GlyphRun,
    FONT_MAX_GLYPH_REPLY, FONT_MAX_GLYPH_RUN, FONT_MAX_PIXEL_HEIGHT, FONT_MIN_PIXEL_HEIGHT,
};
use tairix_abi::Errno;
use tairix_fontface::{AxisSetting, Face};
use tairix_log::DiscardSink;
use tairix_reclaim::{PressureBand, ReportedPressure};

use crate::discovery::discover;
use crate::discovery::fixtures::{MemoryFamily, MemoryStore};
use crate::service::{FontService, GlyphCache};

/// A machine with plenty of RAM, so a test that is not about the bound gets
/// a cache that comfortably holds what it asks for.
const ROOMY_MACHINE_BYTES: u64 = 64 << 30;

/// The workspace root (the fontd crate is `userland/system/fontd`).
fn workspace_root() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .join("../../..")
        .canonicalize()
        .expect("workspace root")
}

/// Read a committed asset's bytes by its path under `lib/font/assets/`.
fn asset(rel: &str) -> Vec<u8> {
    std::fs::read(workspace_root().join("lib/font/assets").join(rel)).expect("committed asset")
}

/// A cache built exactly as the `Run` binary builds one — the shared
/// classification and the shared RAM-derived budget — but from a gauge the
/// test drives and a sink a host test has nowhere to send records to.
fn cache_for(total_ram_bytes: u64, band: PressureBand) -> (GlyphCache, &'static ReportedPressure) {
    static SINK: DiscardSink = DiscardSink;
    let gauge: &'static ReportedPressure = Box::leak(Box::new(ReportedPressure::unknown()));
    gauge.report(band);
    (
        crate::service::glyph_cache(total_ram_bytes, gauge, &SINK),
        gauge,
    )
}

/// A roomy-cache service discovered from `store`.
fn discover_roomy<'a>(store: &mut MemoryStore<'a>) -> FontService<'a> {
    let (cache, _gauge) = cache_for(ROOMY_MACHINE_BYTES, PressureBand::Normal);
    discover(store, cache, &DiscardSink).expect("discovers")
}

/// A single-family `mono` store: the static, monospace committed face.
fn mono_only_store(mono: &[u8]) -> MemoryStore<'_> {
    MemoryStore {
        dirs: vec![(
            "mono",
            MemoryFamily {
                manifest: "label = Mono\nkind = monospace\nface = Inconsolata-EX.ttf\n",
                faces: vec![("Inconsolata-EX.ttf", mono)],
            },
        )],
    }
}

/// One glyph record: the geometry and the coverage bytes.
type Rendered = (u32, u32, u32, i32, Vec<u8>);

/// Ask for a run and return every record the batch answered.
fn request_glyphs(
    svc: &mut FontService<'_>,
    family: FamilyKey,
    scalars: &[char],
    pixel_height: u32,
    weight: FontWeight,
) -> Result<Vec<Rendered>, Errno> {
    let mut reply = vec![0u8; FONT_MAX_GLYPH_REPLY];
    let n = svc.handle(
        &FontRequest::Glyphs {
            family,
            scalars: GlyphRun::new(scalars).expect("a well-formed run"),
            pixel_height,
            weight,
        }
        .to_le_bytes(),
        &mut reply,
    );
    decode_glyphs_reply(&reply[..n]).map(|batch| {
        batch
            .glyphs()
            .iter()
            .map(|g| (g.width, g.height, g.advance, g.left, g.coverage.to_vec()))
            .collect()
    })
}

/// Ask for one scalar, as a run of one.
fn request_glyph(
    svc: &mut FontService<'_>,
    family: FamilyKey,
    scalar: char,
    pixel_height: u32,
    weight: FontWeight,
) -> Result<Rendered, Errno> {
    let batch = request_glyphs(svc, family, &[scalar], pixel_height, weight)?;
    batch.into_iter().next().ok_or(Errno::NotFound)
}

#[test]
fn the_glyph_cache_reports_under_a_renderable_label() {
    use tairix_abi::sysinfo::CACHE_LABEL_MAX;

    let (cache, _gauge) = cache_for(ROOMY_MACHINE_BYTES, PressureBand::Normal);
    let ledger = cache
        .ledger()
        .expect("the glyph cache is a classified reclaim cache");
    let record = ledger.to_record().expect("the label fits the wire record");
    assert_eq!(record.label(), "fontd.glyphs");
    assert!(record.label().len() <= CACHE_LABEL_MAX);
}

#[test]
fn an_unknown_family_key_fails_closed() {
    let mono = asset("mono/Inconsolata-EX.ttf");
    let mut store = mono_only_store(&mono);
    let mut svc = discover_roomy(&mut store);
    let unknown = FamilyKey::new("nope").expect("a well-formed key");
    assert_eq!(
        request_glyph(&mut svc, unknown, 'A', 28, FontWeight::Regular),
        Err(Errno::NotFound)
    );

    let mut reply = vec![0u8; FONT_MAX_GLYPH_REPLY];
    let n = svc.handle(
        &FontRequest::Metrics {
            family: unknown,
            pixel_height: 28,
            weight: FontWeight::Regular,
        }
        .to_le_bytes(),
        &mut reply,
    );
    assert_eq!(decode_metrics_reply(&reply[..n]), Err(Errno::NotFound));
}

#[test]
fn proportional_and_monospace_families_report_distinct_metrics() {
    let mono = asset("mono/Inconsolata-EX.ttf");
    let inter = asset("inter/Inter-Variable.ttf");
    let mut store = MemoryStore {
        dirs: vec![
            (
                "mono",
                MemoryFamily {
                    manifest: "label = Mono\nkind = monospace\nface = Inconsolata-EX.ttf\n",
                    faces: vec![("Inconsolata-EX.ttf", &mono)],
                },
            ),
            (
                "inter",
                MemoryFamily {
                    manifest: "label = Inter\nkind = proportional\nface = Inter-Variable.ttf\n",
                    faces: vec![("Inter-Variable.ttf", &inter)],
                },
            ),
        ],
    };
    let mut svc = discover_roomy(&mut store);

    let mut reply = vec![0u8; FONT_MAX_GLYPH_REPLY];
    let n = svc.handle(
        &FontRequest::Metrics {
            family: FamilyKey::new("mono").expect("key"),
            pixel_height: 28,
            weight: FontWeight::Regular,
        }
        .to_le_bytes(),
        &mut reply,
    );
    let mono_metrics = decode_metrics_reply(&reply[..n]).expect("mono metrics decode");
    assert!(
        mono_metrics.monospace_advance > 0,
        "a monospace family reports its uniform advance"
    );

    let n = svc.handle(
        &FontRequest::Metrics {
            family: FamilyKey::new("inter").expect("key"),
            pixel_height: 28,
            weight: FontWeight::Regular,
        }
        .to_le_bytes(),
        &mut reply,
    );
    let inter_metrics = decode_metrics_reply(&reply[..n]).expect("inter metrics decode");
    assert_eq!(
        inter_metrics.monospace_advance, 0,
        "a proportional family never reports a monospace advance"
    );
    assert_eq!(mono_metrics.pixel_height, 28);
    assert_eq!(inter_metrics.pixel_height, 28);
}

/// The printable-ASCII scalar whose advance the `wght` axis moves furthest
/// in the face `bytes` describe.
///
/// Probed rather than named outright: a capital's advance barely moves with
/// weight (its side bearings absorb the thicker stem), so a hard-coded letter
/// can hide a genuine variation behind pixel rounding, while the
/// widest-moving glyph shows it at any usable size.
fn most_weight_sensitive_scalar(bytes: &[u8]) -> char {
    let instance = |weight: FontWeight| {
        Face::parse_instance(
            bytes,
            &[AxisSetting {
                tag: *b"wght",
                value: f32::from(weight.axis_value()),
            }],
        )
        .expect("a variable face instances at a standard weight")
    };
    let light = instance(FontWeight::Regular);
    let heavy = instance(FontWeight::Bold);
    (0x21..0x7F)
        .filter_map(|code| {
            let light_advance = light.advance(light.glyph_for(code)?).ok()?;
            let heavy_advance = heavy.advance(heavy.glyph_for(code)?).ok()?;
            Some((heavy_advance - light_advance, code))
        })
        .max()
        .and_then(|(_, code)| char::from_u32(code))
        .expect("the face maps printable ASCII")
}

#[test]
fn a_variable_faces_bold_advance_differs_from_its_regular_advance() {
    let inter = asset("inter/Inter-Variable.ttf");
    let mut store = MemoryStore {
        dirs: vec![(
            "inter",
            MemoryFamily {
                manifest: "label = Inter\nkind = proportional\nface = Inter-Variable.ttf\n",
                faces: vec![("Inter-Variable.ttf", &inter)],
            },
        )],
    };
    let mut svc = discover_roomy(&mut store);
    let key = FamilyKey::new("inter").expect("key");
    let scalar = most_weight_sensitive_scalar(&inter);

    let regular = request_glyph(&mut svc, key, scalar, 32, FontWeight::Regular).expect("regular");
    let bold = request_glyph(&mut svc, key, scalar, 32, FontWeight::Bold).expect("bold");

    assert_ne!(
        regular.2, bold.2,
        "a real wght axis must change the reported advance"
    );
    let regular_ink: u32 = regular.4.iter().map(|&c| u32::from(c)).sum();
    let bold_ink: u32 = bold.4.iter().map(|&c| u32::from(c)).sum();
    assert!(
        bold_ink > regular_ink,
        "a heavier real instance must ink more of the glyph"
    );
}

#[test]
fn a_static_faces_synthetic_bold_leaves_the_advance_unchanged() {
    let mono = asset("mono/Inconsolata-EX.ttf");
    let mut store = mono_only_store(&mono);
    let mut svc = discover_roomy(&mut store);
    let key = FamilyKey::new("mono").expect("key");

    let regular = request_glyph(&mut svc, key, 'H', 28, FontWeight::Regular).expect("regular");
    let bold = request_glyph(&mut svc, key, 'H', 28, FontWeight::Bold).expect("bold");

    assert_eq!(
        regular.2, bold.2,
        "synthetic emboldening must never move the advance a client laid out with"
    );
    assert_eq!(regular.0, bold.0, "geometry width stays put");
    assert_eq!(regular.1, bold.1, "geometry height stays put");
    let regular_ink: u32 = regular.4.iter().map(|&c| u32::from(c)).sum();
    let bold_ink: u32 = bold.4.iter().map(|&c| u32::from(c)).sum();
    assert!(
        bold_ink > regular_ink,
        "the synthetic stroke must still ink more of the glyph"
    );
}

#[test]
fn a_glyph_request_is_served_from_cache_on_a_second_call() {
    let mono = asset("mono/Inconsolata-EX.ttf");
    let mut store = mono_only_store(&mono);
    let mut svc = discover_roomy(&mut store);
    let key = FamilyKey::new("mono").expect("key");

    let first = request_glyph(&mut svc, key, 'g', 24, FontWeight::Regular).expect("first");
    let second = request_glyph(&mut svc, key, 'g', 24, FontWeight::Regular).expect("second");
    assert_eq!(
        first, second,
        "a cache hit must serve the same bytes a miss did"
    );
}

/// Set difference of two faces' mapped codepoints: every scalar `a` maps
/// that `b` does not.
fn mapped_only_in<'x>(a: &'x Face<'_>, b: &'x Face<'_>) -> impl Iterator<Item = u32> + 'x {
    a.mapped()
        .iter()
        .map(|&(code, _)| code)
        .filter(|code| b.glyph_for(*code).is_none())
}

#[test]
fn resolution_walks_primary_then_companion_then_fallback_then_replacement() {
    let hebrew_bytes = asset("mono/NotoSansHebrew-ExtraCondensed.ttf");
    let mono_bytes = asset("mono/Inconsolata-EX.ttf");
    let inter_bytes = asset("inter/Inter-Variable.ttf");

    // Probe real coverage with the same engine the service uses, so the test
    // picks scalars that genuinely exercise each resolution step rather than
    // guessing at font content.
    let hebrew_face = Face::parse(&hebrew_bytes).expect("hebrew face parses");
    let mono_face = Face::parse(&mono_bytes).expect("mono face parses");
    let inter_face = Face::parse(&inter_bytes).expect("inter face parses");

    let own_primary_scalar = char::from_u32(
        hebrew_face
            .mapped()
            .first()
            .map(|&(code, _)| code)
            .expect("the Hebrew face maps at least one codepoint"),
    )
    .expect("a valid scalar");
    assert!(
        mono_face.glyph_for(u32::from(own_primary_scalar)).is_none(),
        "the probe scalar must not also be covered by the companion face"
    );

    // A scalar the companion (second, Latin) face covers but the Hebrew
    // primary does not — resolved from the second face in the family's own
    // list.
    let own_companion_scalar = 'A';
    assert!(hebrew_face
        .glyph_for(u32::from(own_companion_scalar))
        .is_none());
    assert!(mono_face
        .glyph_for(u32::from(own_companion_scalar))
        .is_some());

    // A scalar only the fallback family's face covers.
    let fallback_only = mapped_only_in(&inter_face, &hebrew_face)
        .find(|&code| mono_face.glyph_for(code).is_none())
        .and_then(char::from_u32)
        .expect("Inter maps something neither of the family's own faces do");

    let mut store = MemoryStore {
        dirs: vec![
            (
                "test-order",
                MemoryFamily {
                    manifest: "label = Order\nkind = proportional\n\
                               face = NotoSansHebrew-ExtraCondensed.ttf\n\
                               face = Inconsolata-EX.ttf\nfallback = test-fallback\n",
                    faces: vec![
                        ("NotoSansHebrew-ExtraCondensed.ttf", &hebrew_bytes),
                        ("Inconsolata-EX.ttf", &mono_bytes),
                    ],
                },
            ),
            (
                "test-fallback",
                MemoryFamily {
                    manifest: "label = Fallback\nkind = fallback\nface = Inter-Variable.ttf\n",
                    faces: vec![("Inter-Variable.ttf", &inter_bytes)],
                },
            ),
        ],
    };
    let mut svc = discover_roomy(&mut store);
    let key = FamilyKey::new("test-order").expect("key");

    assert!(
        request_glyph(&mut svc, key, own_primary_scalar, 28, FontWeight::Regular).is_ok(),
        "a scalar the primary face maps must resolve from it"
    );
    assert!(
        request_glyph(&mut svc, key, own_companion_scalar, 28, FontWeight::Regular).is_ok(),
        "a scalar only the companion face maps must still resolve within the family"
    );
    assert!(
        request_glyph(&mut svc, key, fallback_only, 28, FontWeight::Regular).is_ok(),
        "a scalar only the fallback family maps must resolve through it"
    );

    // A scalar no face anywhere maps still renders — U+FFFD from the
    // primary — never a refusal for lack of coverage.
    let never_mapped = '\u{10FFFF}';
    match request_glyph(&mut svc, key, never_mapped, 28, FontWeight::Regular) {
        Ok((_, _, _, _, coverage)) => {
            assert!(
                coverage.iter().any(|&c| c > 0),
                "the U+FFFD fallback must have visible ink"
            );
        }
        Err(err) => {
            // Only acceptable when the primary face itself maps no
            // replacement glyph at all — the documented, structurally
            // defensive edge case.
            assert_eq!(err, Errno::NotFound);
            assert!(hebrew_face.glyph_for(0xFFFD).is_none());
        }
    }
}

#[test]
fn two_families_sharing_a_fallback_face_key_the_cache_separately() {
    let mono_bytes = asset("mono/Inconsolata-EX.ttf");
    let inter_bytes = asset("inter/Inter-Variable.ttf");
    let hebrew_bytes = asset("mono/NotoSansHebrew-ExtraCondensed.ttf");

    let hebrew_face = Face::parse(&hebrew_bytes).expect("hebrew face parses");
    let mono_face = Face::parse(&mono_bytes).expect("mono face parses");
    let inter_face = Face::parse(&inter_bytes).expect("inter face parses");

    // A scalar neither `family-a`'s own face (mono) nor `family-b`'s own
    // face (inter) maps, but the shared fallback does.
    let shared_scalar = mapped_only_in(&hebrew_face, &mono_face)
        .find(|&code| inter_face.glyph_for(code).is_none())
        .and_then(char::from_u32)
        .expect("the Hebrew face maps something neither Latin face does");

    let mut store = MemoryStore {
        dirs: vec![
            (
                "family-a",
                MemoryFamily {
                    manifest: "label = A\nkind = proportional\nface = Inconsolata-EX.ttf\n\
                               fallback = shared-fallback\n",
                    faces: vec![("Inconsolata-EX.ttf", &mono_bytes)],
                },
            ),
            (
                "family-b",
                MemoryFamily {
                    manifest: "label = B\nkind = proportional\nface = Inter-Variable.ttf\n\
                               fallback = shared-fallback\n",
                    faces: vec![("Inter-Variable.ttf", &inter_bytes)],
                },
            ),
            (
                "shared-fallback",
                MemoryFamily {
                    manifest: "label = Fallback\nkind = fallback\n\
                               face = NotoSansHebrew-ExtraCondensed.ttf\n",
                    faces: vec![("NotoSansHebrew-ExtraCondensed.ttf", &hebrew_bytes)],
                },
            ),
        ],
    };
    let mut svc = discover_roomy(&mut store);
    let family_a = FamilyKey::new("family-a").expect("key");
    let family_b = FamilyKey::new("family-b").expect("key");

    assert!(request_glyph(&mut svc, family_a, shared_scalar, 28, FontWeight::Regular).is_ok());
    assert_eq!(svc.cache.len(), 1);
    assert!(request_glyph(&mut svc, family_b, shared_scalar, 28, FontWeight::Regular).is_ok());
    assert_eq!(
        svc.cache.len(),
        2,
        "the same physical glyph from two different requesting families must not collide"
    );
    // Repeating family A's request must hit the existing entry, not grow it.
    assert!(request_glyph(&mut svc, family_a, shared_scalar, 28, FontWeight::Regular).is_ok());
    assert_eq!(svc.cache.len(), 2);
}

#[test]
fn a_size_outside_the_permitted_range_is_refused() {
    let mono = asset("mono/Inconsolata-EX.ttf");
    let mut store = mono_only_store(&mono);
    let mut svc = discover_roomy(&mut store);
    let key = FamilyKey::new("mono").expect("key");
    for height in [
        FONT_MIN_PIXEL_HEIGHT - 1,
        FONT_MAX_PIXEL_HEIGHT + 1,
        0,
        u32::MAX,
    ] {
        let mut reply = vec![0u8; FONT_MAX_GLYPH_REPLY];
        let n = svc.handle(
            &FontRequest::Glyphs {
                family: key,
                scalars: GlyphRun::new(&['A']).expect("a well-formed run"),
                pixel_height: height,
                weight: FontWeight::Regular,
            }
            .to_le_bytes(),
            &mut reply,
        );
        assert_eq!(
            decode_glyphs_reply(&reply[..n]).err(),
            Some(Errno::LengthOutOfRange),
            "pixel height {height} must stay refused"
        );
    }
}

#[test]
fn no_band_empties_the_atlas_below_its_reserve() {
    // Every glyph the desktop draws is served from here, so a service that
    // gave its atlas up would re-rasterise the whole visible text of every
    // client, per repaint, exactly when the machine can least afford it. The
    // atlas therefore declares an irreducible reserve no band takes.
    let mono = asset("mono/Inconsolata-EX.ttf");
    let mut store = mono_only_store(&mono);
    let (cache, gauge) = cache_for(ROOMY_MACHINE_BYTES, PressureBand::Normal);
    let mut svc = discover(&mut store, cache, &DiscardSink).expect("discovers");
    let key = FamilyKey::new("mono").expect("key");

    assert!(request_glyph(&mut svc, key, 'A', 28, FontWeight::Regular).is_ok());
    assert_eq!(svc.cache.len(), 1);

    for band in [
        PressureBand::Mild,
        PressureBand::Moderate,
        PressureBand::Severe,
        PressureBand::Critical,
    ] {
        gauge.report(band);
        assert_eq!(svc.trim_cache(), 0, "{band:?} took the reserve");
        assert_eq!(svc.cache.len(), 1, "{band:?}");
    }

    // And the reserve is fillable, not merely keepable: a glyph not seen
    // before is still rasterised *and* retained at the deepest band.
    assert!(
        request_glyph(&mut svc, key, 'B', 28, FontWeight::Regular).is_ok(),
        "the service still rasterises"
    );
    assert_eq!(svc.cache.len(), 2, "the reserve still admits");
}

#[test]
fn a_machine_that_answered_no_ram_reading_rasterises_every_glyph_uncached() {
    // Fail closed to uncached rather than to a hand-picked ceiling: the
    // budget is zero, so the reserve is zero too, and the service is correct
    // and merely slower.
    let mono = asset("mono/Inconsolata-EX.ttf");
    let mut store = mono_only_store(&mono);
    let (cache, _gauge) = cache_for(0, PressureBand::Normal);
    let mut svc = discover(&mut store, cache, &DiscardSink).expect("discovers");
    let key = FamilyKey::new("mono").expect("key");

    assert!(request_glyph(&mut svc, key, 'A', 28, FontWeight::Regular).is_ok());
    assert_eq!(svc.cache.len(), 0);
    assert!(request_glyph(&mut svc, key, 'A', 28, FontWeight::Regular).is_ok());
    assert_eq!(svc.cache.len(), 0);
    assert_eq!(svc.trim_cache(), 0);
}

#[test]
fn a_malformed_request_fails_closed_with_an_error_frame() {
    let mono = asset("mono/Inconsolata-EX.ttf");
    let mut store = mono_only_store(&mono);
    let mut svc = discover_roomy(&mut store);
    let key = FamilyKey::new("mono").expect("key");
    let mut request = FontRequest::Glyphs {
        family: key,
        scalars: GlyphRun::new(&['A']).expect("a well-formed run"),
        pixel_height: 28,
        weight: FontWeight::Regular,
    }
    .to_le_bytes();
    request[0] ^= 0xFF;
    let mut reply = vec![0u8; FONT_MAX_GLYPH_REPLY];
    let n = svc.handle(&request, &mut reply);
    assert_eq!(
        decode_glyphs_reply(&reply[..n]).err(),
        Some(Errno::BadMagic)
    );
}

#[test]
fn a_run_is_answered_whole_and_in_order_at_a_desktop_size() {
    // The measured burst this batching removes: a window opening asked for
    // 32 glyphs one call at a time. At the sizes a desktop draws text the
    // whole run fits one reply, and it comes back in the order it was asked
    // for — which is what lets the client pair the records with its run.
    let mono = asset("mono/Inconsolata-EX.ttf");
    let mut store = mono_only_store(&mono);
    let mut svc = discover_roomy(&mut store);
    let key = FamilyKey::new("mono").expect("key");

    let scalars: Vec<char> = ('A'..='Z').chain('0'..='5').collect();
    assert_eq!(scalars.len(), FONT_MAX_GLYPH_RUN);
    let batch = request_glyphs(&mut svc, key, &scalars, 28, FontWeight::Regular).expect("a batch");
    assert_eq!(
        batch.len(),
        FONT_MAX_GLYPH_RUN,
        "the longest permitted run fits one reply at a desktop size"
    );
    for (scalar, record) in scalars.iter().zip(&batch) {
        let alone =
            request_glyph(&mut svc, key, *scalar, 28, FontWeight::Regular).expect("one glyph");
        assert_eq!(
            *record, alone,
            "{scalar:?} must be the same glyph asked for in a run or alone"
        );
    }
}

#[test]
fn a_run_too_large_for_one_frame_is_answered_as_a_prefix() {
    // A glyph at the tallest permitted size is a large fraction of the reply
    // frame, so the batch answers what fits and says how many; the client
    // asks again for the remainder. It always answers at least one, or the
    // client could never make progress.
    let mono = asset("mono/Inconsolata-EX.ttf");
    let mut store = mono_only_store(&mono);
    let mut svc = discover_roomy(&mut store);
    let key = FamilyKey::new("mono").expect("key");

    let scalars: Vec<char> = ('A'..='Z').collect();
    let batch = request_glyphs(
        &mut svc,
        key,
        &scalars,
        FONT_MAX_PIXEL_HEIGHT,
        FontWeight::Regular,
    )
    .expect("a batch");
    assert!(!batch.is_empty(), "a successful batch answers at least one");
    assert!(
        batch.len() < scalars.len(),
        "{} records of {} cannot all fit one frame at the tallest size",
        batch.len(),
        scalars.len()
    );
    // The prefix is the head of the run, so the remainder the client asks
    // for again starts exactly where this stopped.
    for (scalar, record) in scalars.iter().zip(&batch) {
        let alone = request_glyph(
            &mut svc,
            key,
            *scalar,
            FONT_MAX_PIXEL_HEIGHT,
            FontWeight::Regular,
        )
        .expect("one glyph");
        assert_eq!(*record, alone, "{scalar:?} must be the run's own prefix");
    }
}

/// A monospace family's cell width at `pixel_height`: the advance every
/// glyph of a character grid steps by.
fn cell_width(svc: &mut FontService<'_>, family: FamilyKey, pixel_height: u32) -> u32 {
    let mut reply = vec![0u8; FONT_MAX_GLYPH_REPLY];
    let n = svc.handle(
        &FontRequest::Metrics {
            family,
            pixel_height,
            weight: FontWeight::Regular,
        }
        .to_le_bytes(),
        &mut reply,
    );
    decode_metrics_reply(&reply[..n])
        .expect("metrics decode")
        .monospace_advance
}

#[test]
fn a_monospace_family_is_drawn_into_its_character_cell() {
    // A character grid steps by one advance per column, so a glyph belongs
    // *in* that cell: the client blits at the cell origin and the engine
    // fits the outline to the cell as it rasterises. Served tight to its own
    // ink instead, every glyph carries a bearing the grid rounds away and a
    // width that agrees with the column only by luck.
    let mono = asset("mono/Inconsolata-EX.ttf");
    let mut store = mono_only_store(&mono);
    let mut svc = discover_roomy(&mut store);
    let key = FamilyKey::new("mono").expect("key");
    for pixel_height in [13, 16, 28] {
        let cell = cell_width(&mut svc, key, pixel_height);
        assert!(cell > 0, "the monospace face reports a cell");
        for scalar in ['i', 'M', 'g', '.', ' '] {
            let (width, height, advance, left, coverage) =
                request_glyph(&mut svc, key, scalar, pixel_height, FontWeight::Regular)
                    .expect("a covered scalar");
            assert_eq!(
                width, cell,
                "{scalar:?} at {pixel_height}px is not one cell"
            );
            assert_eq!(advance, cell, "{scalar:?} does not advance one cell");
            assert_eq!(left, 0, "{scalar:?} carries a bearing the grid cannot use");
            assert_eq!(height, pixel_height);
            assert_eq!(coverage.len(), (width * height) as usize);
        }
    }
}

#[test]
fn the_character_grid_is_sharp_at_the_terminal_size() {
    // The reader's complaint, as a number. A grid glyph fitted to its cell
    // puts its stems on whole pixels; drawn at the face's own subpixel
    // advance it lands between them and antialiases into two grey columns,
    // which is what a blurry terminal is. Measured over printable ASCII at
    // the size a terminal opens at, fitting takes the share of fully-opaque
    // ink from an eighth of the glyph to a third.
    const SOLID_SHARE_PERCENT: usize = 25;
    let mono = asset("mono/Inconsolata-EX.ttf");
    let mut store = mono_only_store(&mono);
    let mut svc = discover_roomy(&mut store);
    let key = FamilyKey::new("mono").expect("key");
    let (mut ink, mut solid) = (0usize, 0usize);
    for code in 0x21..0x7F {
        let scalar = char::from_u32(code).expect("printable ASCII");
        let (.., coverage) =
            request_glyph(&mut svc, key, scalar, 13, FontWeight::Regular).expect("covered");
        for sample in coverage {
            ink += usize::from(sample > 0);
            solid += usize::from(sample == u8::MAX);
        }
    }
    assert!(ink > 0, "printable ASCII drew no ink at all");
    assert!(
        solid * 100 / ink >= SOLID_SHARE_PERCENT,
        "only {}% of inked pixels are solid; the grid has gone soft",
        solid * 100 / ink
    );
}

#[test]
fn a_border_character_is_pixel_exact_and_tiles() {
    // Box Drawing and Block Elements exist to tile: a rule has to join its
    // neighbours and a full block has to abut the next with no seam. An
    // outline gives that only where its hairlines land on pixel boundaries,
    // so a grid draws them as geometry instead — the same geometry the
    // compiled-in console atlas is built from, at whatever cell the desktop
    // asks for. Emboldening them would break the tiling, so a bold border is
    // the same picture as a regular one.
    let mono = asset("mono/Inconsolata-EX.ttf");
    let mut store = mono_only_store(&mono);
    let mut svc = discover_roomy(&mut store);
    let key = FamilyKey::new("mono").expect("key");
    for weight in [FontWeight::Regular, FontWeight::Bold] {
        let (width, height, advance, left, rule) =
            request_glyph(&mut svc, key, '─', 13, weight).expect("a border rule");
        assert_eq!(advance, width, "a rule occupies exactly its cell");
        assert_eq!(left, 0);
        assert!(
            rule.iter().all(|&s| s == 0 || s == u8::MAX),
            "an antialiased rule cannot join its neighbours"
        );
        assert!(
            (0..width)
                .all(|column| (0..height)
                    .any(|row| rule[(row * width + column) as usize] == u8::MAX)),
            "the rule stops short of its cell edge and leaves a gap"
        );
        let (.., block) = request_glyph(&mut svc, key, '█', 13, weight).expect("a full block");
        assert!(
            block.iter().all(|&sample| sample == u8::MAX),
            "a full block is not solid, so a filled area will show seams"
        );
    }
}

#[test]
fn a_proportional_family_is_still_drawn_tight_to_its_ink() {
    // The cell is a monospace family's contract, not every family's: text
    // laid out by per-glyph advance needs the ink and its bearing, and
    // squaring it into a column would space it like a typewriter.
    let inter = asset("inter/Inter-Variable.ttf");
    let mut store = MemoryStore {
        dirs: vec![(
            "inter",
            MemoryFamily {
                manifest: "label = Inter\nkind = proportional\nface = Inter-Variable.ttf\n",
                faces: vec![("Inter-Variable.ttf", &inter)],
            },
        )],
    };
    let mut svc = discover_roomy(&mut store);
    let key = FamilyKey::new("inter").expect("key");
    let narrow = request_glyph(&mut svc, key, 'i', 28, FontWeight::Regular).expect("i");
    let wide = request_glyph(&mut svc, key, 'W', 28, FontWeight::Regular).expect("W");
    assert!(
        narrow.0 < wide.0 && narrow.2 < wide.2,
        "a proportional family must keep each glyph's own width and advance"
    );
}

#[test]
fn a_wide_scalar_does_not_lend_its_two_cells_to_a_narrow_one() {
    // A face maps many scalars onto one glyph — every scalar it does not
    // cover falls back to the replacement — and how many cells that glyph is
    // drawn across is a property of the *scalar*, not the glyph. Ask for a
    // double-width one first and the cache must not then serve its two-cell
    // bitmap for a single-width scalar that resolved to the same glyph.
    let mono = asset("mono/Inconsolata-EX.ttf");
    let mut store = mono_only_store(&mono);
    let mut svc = discover_roomy(&mut store);
    let key = FamilyKey::new("mono").expect("key");
    let cell = cell_width(&mut svc, key, 16);
    let wide = request_glyph(&mut svc, key, 'あ', 16, FontWeight::Regular).expect("uncovered");
    assert_eq!(wide.0, cell * 2, "a double-width scalar reserves two cells");
    assert_eq!(wide.2, cell * 2);
    let narrow = request_glyph(&mut svc, key, '\u{FFFD}', 16, FontWeight::Regular)
        .expect("the replacement is covered");
    assert_eq!(narrow.0, cell, "the replacement itself is one cell wide");
    assert_eq!(narrow.2, cell);
}
