//! Host tests for the grid's icon-artwork pipeline.
//!
//! The pinned defect is that a paint used to read and decode: the first frame
//! of a folder blocked on a sandbox round trip per visible tile. These drive
//! the real cache and desk over counting seams, so what a paint costs is
//! asserted rather than argued.

use alloc::string::String;
use alloc::vec;
use alloc::vec::Vec;

use tairix_icon::{
    artwork_cache, ArtworkCache, ArtworkRasteriser, ArtworkReader, IconArtwork, IconArtworkSource,
    IconKind, IconPicture, IconRequest, InlineArtwork,
};
use tairix_log::DiscardSink;
use tairix_reclaim::{PressureBand, ReportedPressure};

use super::IconPipeline;

/// The seat the caches under test are charged to.
const TEST_SEAT: u64 = 1;

/// A 1080p 32-bit frame: the pipeline derives its budget from the window it
/// draws on, so the ceiling exercised here is the real derivation.
const TEST_FRAME_BYTES: usize = 1920 * 1080 * 4;

/// The tile side every test resolves at.
const SIDE: u32 = 32;

/// The asset one tile's icon resolves to.
const ASSET: &str = "/System/Graphics/Icons/one.png";

/// How many tiers the tested request resolves through: the tile's own asset,
/// then its kind's raster master, then its kind's vector master.
const TIERS: usize = 3;

/// A frame so small that a sixteenth of it — the artwork budget — has no room
/// for a single [`SIDE`]-square decode's pixels.
const FRAME_TOO_SMALL_FOR_ONE_ICON: usize = 1024;

/// The gauge the pipelines under test are governed by, held at normal for its
/// whole life so tests running in parallel cannot perturb one another.
static NORMAL_PRESSURE: ReportedPressure = ReportedPressure::unknown();

/// Discards audit records; the caches' own audit path is covered where it is
/// defined.
static TEST_SINK: DiscardSink = DiscardSink;

/// A cache built through the one shared constructor, so the budget and
/// classification are the shipping policy's.
fn cache() -> ArtworkCache {
    NORMAL_PRESSURE.report(PressureBand::Normal);
    artwork_cache(
        "files.test-artwork",
        TEST_SEAT,
        TEST_FRAME_BYTES,
        &NORMAL_PRESSURE,
        &TEST_SINK,
    )
}

/// A reader over a fixed path→bytes table that counts every read.
struct CountingReader {
    /// The asset bytes the reader will serve, by path.
    assets: Vec<(String, Vec<u8>)>,
    /// How many reads have been asked for.
    reads: usize,
}

impl CountingReader {
    /// A reader holding [`ASSET`] alone.
    fn new() -> Self {
        Self {
            assets: vec![(String::from(ASSET), vec![0u8; 8])],
            reads: 0,
        }
    }
}

impl ArtworkReader for CountingReader {
    fn read(&mut self, path: &str) -> Option<Vec<u8>> {
        self.reads += 1;
        self.assets
            .iter()
            .find(|(held, _)| held == path)
            .map(|(_, bytes)| bytes.clone())
    }
}

/// A rasteriser that answers the exact square it was asked for and counts
/// every decode — the sandbox round trip, in the tests' terms.
struct CountingRasteriser {
    /// How many decodes have been asked for.
    decodes: usize,
}

impl ArtworkRasteriser for CountingRasteriser {
    fn rasterise(&mut self, side: u32, _bytes: &[u8]) -> Option<Vec<u8>> {
        self.decodes += 1;
        // Opaque, so a served picture is observably artwork rather than an
        // empty surface.
        Some(vec![255u8; (side as usize) * (side as usize) * 4])
    }
}

/// A cache whose whole artwork budget is smaller than one decode's pixels, so
/// every admission is refused — the only state in which the cache genuinely
/// cannot keep a picture, since no pressure band empties this class below its
/// working-set floor.
fn cacheless() -> ArtworkCache {
    NORMAL_PRESSURE.report(PressureBand::Normal);
    artwork_cache(
        "files.test-artwork-cacheless",
        TEST_SEAT,
        FRAME_TOO_SMALL_FOR_ONE_ICON,
        &NORMAL_PRESSURE,
        &TEST_SINK,
    )
}

/// A rasteriser that has decoded nothing yet.
const fn decoder() -> CountingRasteriser {
    CountingRasteriser { decodes: 0 }
}

/// A pipeline over the counting seams.
fn pipeline() -> IconPipeline<CountingReader, CountingRasteriser> {
    IconPipeline::new(cache(), CountingReader::new(), decoder())
}

/// What the seams have been asked to do so far.
fn work(pipe: &IconPipeline<CountingReader, CountingRasteriser>) -> (usize, usize) {
    (pipe.reader.reads, pipe.rasteriser.decodes)
}

/// The request one tile makes: a shipped asset, falling back to its kind.
fn request() -> IconRequest<'static> {
    IconRequest::asset(IconKind::File, ASSET)
}

/// Resolve one tile exactly as a paint does, reporting whether it drew real
/// artwork rather than the built-in glyph's coverage mask.
fn paint(pipe: &mut IconPipeline<CountingReader, CountingRasteriser>) -> bool {
    matches!(
        pipe.source().artwork(request(), SIDE),
        Some(IconPicture::Artwork(_))
    )
}

/// The defect this pipeline exists to fix: a paint must perform no read and no
/// sandbox round trip. The tile draws its built-in glyph and the decode is
/// merely *recorded*.
#[test]
fn a_paint_reads_nothing_and_decodes_nothing() {
    let mut pipe = pipeline();

    assert!(
        !paint(&mut pipe),
        "an unresolved tile draws its built-in glyph, never blank"
    );
    assert_eq!(
        work(&pipe),
        (0, 0),
        "a paint performed a read or a sandbox round trip"
    );

    // Repainting the same unresolved tile — a scroll step, a hover — still
    // costs nothing, so a frozen window cannot come back by way of a repaint.
    assert!(!paint(&mut pipe));
    assert_eq!(work(&pipe), (0, 0));
}

/// What the paint used to cost, so the assertion above is not vacuous:
/// resolving the very same tile through the inline resolver — the one this
/// pipeline replaced — reads and decodes before it returns.
///
/// The inline resolver is still the right answer where there is nothing to
/// defer to (the desktop session's no-thread fallback), so this is a contrast,
/// not a relic.
#[test]
fn the_inline_resolver_is_what_made_the_paint_block() {
    let mut pipe = pipeline();
    let IconPipeline {
        cache,
        reader,
        rasteriser,
        ..
    } = &mut pipe;
    let mut inline = InlineArtwork::new(reader, rasteriser);

    assert!(
        matches!(
            IconArtworkSource::new(cache, &mut inline).artwork(request(), SIDE),
            Some(IconPicture::Artwork(_))
        ),
        "the inline resolver draws the artwork in the frame that asked"
    );
    assert_eq!(
        work(&pipe),
        (1, 1),
        "and pays a read and a sandbox round trip to do it"
    );
}

/// The pump runs the recorded decode off the paint, and the next paint serves
/// it. One pump per call, and it reports when there is nothing left.
#[test]
fn the_pump_delivers_what_the_paint_recorded() {
    let mut pipe = pipeline();
    assert!(!paint(&mut pipe));

    assert!(pipe.pump(), "the paint recorded a decode to run");
    assert_eq!(work(&pipe), (1, 1));
    assert!(pipe.take_landed(), "a delivery owes the loop a repaint");
    assert!(!pipe.pump(), "one paint records exactly one decode");

    assert!(paint(&mut pipe), "the landed decode is drawn");
    assert_eq!(
        work(&pipe),
        (1, 1),
        "the retained decode was produced a second time"
    );
    assert!(
        !pipe.take_landed(),
        "a paint that landed nothing must not force a frame"
    );
}

/// A tile none of whose assets exist settles on its built-in glyph, having
/// read each tier exactly once. A retained refusal is what makes the next tier
/// wanted, so the walk costs one paint/pump turn per tier and then stops.
#[test]
fn a_request_whose_every_tier_refuses_settles_on_the_glyph() {
    let mut pipe = IconPipeline::new(
        cache(),
        CountingReader {
            assets: Vec::new(),
            reads: 0,
        },
        CountingRasteriser { decodes: 0 },
    );

    // The tile's own asset, then its kind's raster master, then its kind's
    // vector master.
    for tier in 1..=TIERS {
        assert!(!paint(&mut pipe), "no tier has artwork to draw");
        assert!(pipe.pump(), "tier {tier} was not offered");
        assert_eq!(
            work(&pipe),
            (tier, 0),
            "an unreadable asset must never reach the decoder"
        );
    }

    assert!(
        !paint(&mut pipe),
        "an exhausted request draws the built-in glyph, never blank"
    );
    assert!(
        !pipe.pump(),
        "every tier is a retained refusal, so nothing is asked for again"
    );
    assert_eq!(work(&pipe), (TIERS, 0));
}

/// A decode the cache retained is not produced again inside one round, and a
/// round boundary does not make it produced again either — the retained answer
/// is what serves it.
#[test]
fn a_retained_decode_is_never_produced_twice() {
    let mut pipe = pipeline();
    assert!(!paint(&mut pipe));
    assert!(pipe.pump());
    assert!(paint(&mut pipe));

    pipe.begin_round();
    assert!(paint(&mut pipe));
    assert!(!pipe.pump(), "a fresh round re-asked for a retained decode");
    assert_eq!(work(&pipe), (1, 1));
}

/// A cache with no room for the decode declines it, and the desk stops
/// offering it. Without that, the repaint the landing drove asks again, the
/// answer is refused again, and every tile on screen is read and decoded on
/// every frame — precisely when memory is short.
#[test]
fn a_declined_decode_is_not_re_asked_within_a_round_or_by_the_next() {
    let mut pipe = IconPipeline::new(cacheless(), CountingReader::new(), decoder());

    assert!(!paint(&mut pipe));
    assert!(pipe.pump());
    // Collected, and refused: nothing is retained, so the tile still draws
    // its glyph.
    assert!(!paint(&mut pipe));
    assert_eq!(work(&pipe), (1, 1));

    pipe.begin_round();
    assert!(!paint(&mut pipe));
    assert!(
        !pipe.pump(),
        "a declined decode was offered again by a fresh round"
    );
    assert_eq!(work(&pipe), (1, 1));
}

/// A pressure-band change remakes the decision: what a band refused to keep
/// may now be retainable, so the declined key is offered once more. This is
/// what [`IconPipeline::trim`] is for beyond releasing bytes.
#[test]
fn a_band_change_offers_a_declined_decode_again() {
    let mut pipe = IconPipeline::new(cacheless(), CountingReader::new(), decoder());
    assert!(!paint(&mut pipe));
    assert!(pipe.pump());
    assert!(!paint(&mut pipe), "the decode was declined");

    let _ = pipe.trim();
    pipe.begin_round();
    assert!(!paint(&mut pipe));
    assert!(
        pipe.pump(),
        "a band change must remake the decision it refused"
    );
    assert_eq!(work(&pipe), (2, 2));
}
