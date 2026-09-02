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

/// A frame with no room for a single [`SIDE`]-square decode's pixels, so the
/// artwork budget it derives has none either.
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

/// A decode the cache retained is never produced again: the retained answer is
/// what serves every later paint, however many there are.
#[test]
fn a_retained_decode_is_never_produced_twice() {
    let mut pipe = pipeline();
    assert!(!paint(&mut pipe));
    assert!(pipe.pump());
    assert!(paint(&mut pipe));

    assert!(paint(&mut pipe));
    assert!(!pipe.pump(), "a repaint re-asked for a retained decode");
    assert_eq!(work(&pipe), (1, 1));
}

/// A cache with no room for the decode declines it, and the desk stops
/// offering it. Without that, the repaint the landing drove asks again, the
/// answer is refused again, and every tile on screen is read and decoded on
/// every frame — precisely when memory is short.
#[test]
fn a_declined_decode_is_never_offered_again_until_the_band_moves() {
    let mut pipe = IconPipeline::new(cacheless(), CountingReader::new(), decoder());

    assert!(!paint(&mut pipe));
    assert!(pipe.pump());
    // Collected, and refused: nothing is retained, so the tile still draws
    // its glyph.
    assert!(!paint(&mut pipe));
    assert_eq!(work(&pipe), (1, 1));

    assert!(!paint(&mut pipe));
    assert!(!pipe.pump(), "a declined decode was offered again");
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
    assert!(!paint(&mut pipe));
    assert!(
        pipe.pump(),
        "a band change must remake the decision it refused"
    );
    assert_eq!(work(&pipe), (2, 2));
}

// ---------------------------------------------------------------------
// A whole window's grid, over the real renderer
// ---------------------------------------------------------------------
//
// The tests above drive one tile. These drive a window's worth through the
// real `lib/browse` grid renderer at the app's own default window size, which
// is what caught the defect the single-tile tests could not see: a repaint
// drew every tile's artwork *once* and then gave most of them back to the
// glyph, because the cache could not hold what one frame draws and an evicted
// key was answered "not yet" for ever.

mod grid {
    use alloc::format;
    use alloc::string::String;
    use alloc::vec;
    use alloc::vec::Vec;
    use core::cell::Cell;

    use tairix_abi::{
        AppInfoHeader, Errno, Time64, APPINFO_MAGIC, BUNDLE_ID_MAX, BUNDLE_NAME_MAX,
        BUNDLE_VERSION_MAX, LIBRARY_ICON_MAX, SYSCALL_TABLE_HASH_LEN,
    };
    use tairix_browse::render::scroll_lines;
    use tairix_browse::{
        render_into, Browser, DirectorySource, Entry, EntryKind, Listing, ManagerChrome,
        ManagerToolModel, ToolbarBand, ViewMode, MANAGER_TOOLS, WIN_HEIGHT, WIN_WIDTH,
    };
    use tairix_geometry::{Rect, Scale};
    use tairix_icon::{
        artwork_cache, ArtworkRasteriser, ArtworkReader, IconArtwork, IconArtworkSource,
        IconPicture, IconRequest,
    };
    use tairix_log::DiscardSink;
    use tairix_raster::Surface;
    use tairix_reclaim::{PressureBand, ReportedPressure};
    use tairix_theme::Theme;

    use crate::icons::IconPipeline;

    /// Bundles in the browsed directory: more than a window shows, so the view
    /// scrolls and the visible set is a true subset.
    const BUNDLES: usize = 64;

    /// The bundle icon every fixture bundle names in its own manifest.
    const ICON: &str = "icon.png";

    /// Held at normal for its whole life, so tests running in parallel cannot
    /// perturb one another's band.
    static PRESSURE: ReportedPressure = ReportedPressure::unknown();

    /// Discards audit records; the cache's audit path is covered where it is
    /// defined.
    static SINK: DiscardSink = DiscardSink;

    /// A directory of application bundles, as `/System/Commands` is.
    struct Bundles;

    impl DirectorySource for Bundles {
        fn list(&mut self, _components: &[String]) -> Result<Listing, Errno> {
            Ok(Listing::Ready(
                (0..BUNDLES)
                    .map(|i| {
                        Entry::new(
                            format!("cmd-{i:03}.app"),
                            EntryKind::Bundle,
                            0,
                            Time64::UNIX_EPOCH,
                        )
                    })
                    .collect(),
            ))
        }
    }

    /// A structurally valid manifest naming [`ICON`] as the bundle's own icon.
    /// The artwork layer decodes the header's shape, not its signature.
    fn manifest() -> Vec<u8> {
        fn inline<const N: usize>(text: &str) -> ([u8; N], u8) {
            let mut buf = [0u8; N];
            buf[..text.len()].copy_from_slice(text.as_bytes());
            (buf, u8::try_from(text.len()).expect("short"))
        }
        let (id, id_len) = inline::<BUNDLE_ID_MAX>("os.tairix.test");
        let (name, name_len) = inline::<BUNDLE_NAME_MAX>("test");
        let (version, version_len) = inline::<BUNDLE_VERSION_MAX>("0.1.0");
        let (library_icon, library_icon_len) = inline::<LIBRARY_ICON_MAX>(ICON);
        AppInfoHeader {
            magic: APPINFO_MAGIC,
            abi_version: tairix_abi::ABI_VERSION_CURRENT,
            flags: 0,
            capability_count: 0,
            mime_count: 0,
            id_len,
            name_len,
            version_len,
            purpose_len: 0,
            author_len: 0,
            library_icon_len,
            library: 0,
            reserved0: [0; 1],
            id,
            name,
            version,
            library_icon,
            purpose: [0; tairix_abi::BUNDLE_PURPOSE_MAX],
            author: [0; tairix_abi::BUNDLE_AUTHOR_MAX],
            syscall_table_hash: [0; SYSCALL_TABLE_HASH_LEN],
            content_hash: [0; 32],
            signer_pubkey: [0; 32],
            publisher_pubkey: [0; 32],
            publisher_cert: [0; 64],
            signature: [0; 64],
        }
        .to_le_bytes()
        .to_vec()
    }

    /// Serves every bundle's manifest and its named icon, counting reads.
    struct BundleReader {
        reads: usize,
    }

    impl ArtworkReader for BundleReader {
        fn read(&mut self, path: &str) -> Option<Vec<u8>> {
            self.reads += 1;
            if path.ends_with("/AppInfo") {
                return Some(manifest());
            }
            path.ends_with(ICON).then(|| vec![0u8; 16])
        }
    }

    /// Answers the exact square asked for, opaque so a served picture is
    /// observably artwork, and counts every sandbox round trip.
    struct Decoder {
        decodes: usize,
    }

    impl ArtworkRasteriser for Decoder {
        fn rasterise(&mut self, side: u32, _bytes: &[u8]) -> Option<Vec<u8>> {
            self.decodes += 1;
            Some(vec![255u8; (side as usize) * (side as usize) * 4])
        }
    }

    /// What one frame drew: artwork tiles against glyph-mask ones.
    #[derive(Default)]
    struct Drawn {
        artwork: Cell<usize>,
        glyph: Cell<usize>,
    }

    /// The app's real lookup, tallying which tier each draw site ended on.
    struct Tally<'a> {
        source: IconArtworkSource<'a>,
        drawn: &'a Drawn,
    }

    impl IconArtwork for Tally<'_> {
        fn artwork(&mut self, request: IconRequest<'_>, side: u32) -> Option<IconPicture<'_>> {
            // Copied out before the lookup borrows `self` for the returned
            // picture's lifetime.
            let drawn = self.drawn;
            let picture = self.source.artwork(request, side);
            let counter = match picture {
                Some(IconPicture::Artwork(_)) => &drawn.artwork,
                Some(IconPicture::Mask(_)) | None => &drawn.glyph,
            };
            counter.set(counter.get() + 1);
            picture
        }
    }

    /// A window's worth of the real grid over the real pipeline.
    struct Window {
        pipeline: IconPipeline<BundleReader, Decoder>,
        browser: Browser<Bundles>,
        surface: Surface,
        theme: Theme,
        viewport: Rect,
    }

    impl Window {
        /// A grid window of the app's own default client size, its artwork
        /// budget derived from that window's frame exactly as the app derives
        /// it.
        fn open() -> Self {
            PRESSURE.report(PressureBand::Normal);
            let (w, h) = (WIN_WIDTH, WIN_HEIGHT);
            let frame_bytes = (w as usize) * 4 * (h as usize);
            let mut browser = Browser::open_root(Bundles).expect("the fixture root lists");
            browser.set_view_mode(ViewMode::Grid);
            Self {
                pipeline: IconPipeline::new(
                    artwork_cache("files.test-grid", 1, frame_bytes, &PRESSURE, &SINK),
                    BundleReader { reads: 0 },
                    Decoder { decodes: 0 },
                ),
                browser,
                surface: Surface::new(w, h).expect("a window-sized surface"),
                theme: Theme::dark(),
                viewport: Rect::new(0, 0, w, h),
            }
        }

        /// Paint the whole window, reporting what each tile drew.
        fn paint(&mut self) -> (usize, usize) {
            let drawn = Drawn::default();
            let chrome = ManagerChrome {
                tools: MANAGER_TOOLS,
                tool_model: ManagerToolModel::none(),
                sidebar: None,
                toolbar: ToolbarBand::Hidden,
            };
            let mut tally = Tally {
                source: self.pipeline.source(),
                drawn: &drawn,
            };
            render_into(
                &mut self.surface,
                &self.browser,
                Scale::ONE,
                &self.theme,
                self.viewport,
                &chrome,
                &mut tally,
            );
            (drawn.artwork.get(), drawn.glyph.get())
        }

        /// Run every decode the last paint recorded, as the loop does before
        /// the repaint that draws the batch.
        fn drain(&mut self) -> usize {
            let mut ran = 0;
            while self.pipeline.pump() {
                ran += 1;
            }
            ran
        }

        /// Scroll the grid by `lines`, as a wheel tick does.
        fn scroll(&mut self, lines: i64) {
            assert!(
                scroll_lines(
                    &mut self.browser,
                    Scale::ONE,
                    &self.theme,
                    self.viewport,
                    ToolbarBand::Hidden,
                    lines,
                ),
                "the grid did not scroll"
            );
        }

        /// Paint and drain until a paint records nothing more, then report what
        /// that settled paint drew.
        fn settle(&mut self) -> (usize, usize) {
            for _ in 0..8 {
                let drawn = self.paint();
                if self.drain() == 0 {
                    return drawn;
                }
            }
            panic!("the grid never settled");
        }
    }

    /// The reported defect: after the decodes had landed and a frame had drawn
    /// them, the *next* frame of the same unchanged window gave most tiles back
    /// to the built-in glyph — and nothing re-decoded them, so the window sat
    /// like that until unrelated input arrived. Scrolling made it frequent
    /// because a scroll is what brings undecoded tiles into view.
    #[test]
    fn a_grid_keeps_every_tile_s_artwork_across_repaints() {
        let mut win = Window::open();
        let (tiles, glyphs) = win.settle();
        assert!(tiles > 0, "the grid drew no tiles at all");
        assert_eq!(glyphs, 0, "a settled grid still drew {glyphs} glyphs");

        let decodes = win.pipeline.rasteriser.decodes;
        for frame in 1..=3 {
            assert_eq!(
                win.paint(),
                (tiles, 0),
                "repaint {frame} gave tiles back to the glyph"
            );
        }
        assert_eq!(
            win.pipeline.rasteriser.decodes, decodes,
            "a repaint of an unchanged window re-decoded artwork it already held"
        );
    }

    /// Scrolling to fresh tiles and back keeps every tile's artwork: the icons
    /// scrolled away are still retained, so returning to them costs no decode.
    #[test]
    fn scrolling_back_and_forth_costs_no_second_decode() {
        let mut win = Window::open();
        let (tiles, _) = win.settle();

        win.scroll(3);
        let scrolled = win.settle();
        assert_eq!(scrolled, (tiles, 0), "a scrolled grid drew glyphs");
        let decodes = win.pipeline.rasteriser.decodes;

        win.scroll(-3);
        assert_eq!(
            win.paint(),
            (tiles, 0),
            "scrolling back drew glyphs for artwork already decoded"
        );
        assert_eq!(
            win.pipeline.rasteriser.decodes, decodes,
            "scrolling back re-decoded what was still retained"
        );
    }
}
