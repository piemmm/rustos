//! Unit tests for the shared icon-artwork layer.
//!
//! The crate is `no_std`, but a test module may use `std`: the shipped-asset
//! walk reads the crate's own `assets/` directory off the host filesystem.

extern crate std;

use alloc::boxed::Box;
use alloc::collections::{BTreeMap, BTreeSet};
use alloc::format;
use alloc::string::{String, ToString};
use alloc::vec;
use alloc::vec::Vec;

use tairix_abi::{
    AppInfoHeader, APPINFO_MAGIC, BUNDLE_ID_MAX, BUNDLE_NAME_MAX, BUNDLE_VERSION_MAX,
    LIBRARY_ICON_MAX, SYSCALL_TABLE_HASH_LEN,
};
use tairix_log::DiscardSink;
use tairix_reclaim::pressure::{PressureBand, ReportedPressure};

use super::{
    artwork_cache, artwork_kind_for_file, glyph_mask, icon_artwork_path, icon_vector_path,
    ArtworkCache, ArtworkKey, ArtworkOutcome, ArtworkRasteriser, ArtworkReader, ArtworkResolver,
    IconArtwork, IconArtworkSource, IconPicture, IconRequest, InlineArtwork, NoArtwork, Resolved,
    Surface, MAX_ARTWORK_BYTES, VECTOR_SUFFIX,
};
use crate::glyph::IconKind;
use crate::load::ICON_KINDS;

/// A reader over an in-memory file table that counts every read, so a test
/// can prove a second lookup for the same key served from the cache rather
/// than reading the asset again.
struct CountingReader {
    files: BTreeMap<String, Vec<u8>>,
    reads: usize,
    read_paths: Vec<String>,
}

impl CountingReader {
    fn new() -> Self {
        Self {
            files: BTreeMap::new(),
            reads: 0,
            read_paths: Vec::new(),
        }
    }

    fn with(mut self, path: &str, bytes: Vec<u8>) -> Self {
        self.files.insert(path.to_string(), bytes);
        self
    }
}

impl ArtworkReader for CountingReader {
    fn read(&mut self, path: &str) -> Option<Vec<u8>> {
        self.reads += 1;
        self.read_paths.push(path.to_string());
        self.files.get(path).cloned()
    }
}

/// A rasteriser that returns a correctly-shaped `side`×`side` block, so the
/// verified surface builds.
struct SquareRasteriser;

impl ArtworkRasteriser for SquareRasteriser {
    fn rasterise(&mut self, side: u32, _bytes: &[u8]) -> Option<Vec<u8>> {
        Some(vec![0xff; (side as usize) * (side as usize) * 4])
    }
}

/// A rasteriser whose reply is too short, so the length check rejects it.
struct ShortRasteriser;

impl ArtworkRasteriser for ShortRasteriser {
    fn rasterise(&mut self, _side: u32, _bytes: &[u8]) -> Option<Vec<u8>> {
        Some(vec![0u8; 3])
    }
}

/// A rasteriser that must never be called: the caller must refuse the input
/// before rasterisation.
struct PanicRasteriser;

impl ArtworkRasteriser for PanicRasteriser {
    fn rasterise(&mut self, _side: u32, _bytes: &[u8]) -> Option<Vec<u8>> {
        panic!("oversize input must be refused before rasterising");
    }
}

/// A cache wired like a real seat's, at a normal pressure band so it retains
/// entries.
fn cache() -> ArtworkCache {
    let gauge: &'static ReportedPressure = Box::leak(Box::new(ReportedPressure::unknown()));
    gauge.report(PressureBand::Normal);
    let sink: &'static DiscardSink = Box::leak(Box::new(DiscardSink));
    artwork_cache("test.icon-artwork", 1, 1920 * 1080 * 4, gauge, sink)
}

#[test]
fn icon_paths_spell_the_asset_id() {
    assert_eq!(
        icon_vector_path(IconKind::Folder),
        "/System/Graphics/Icons/folder.svg"
    );
    assert_eq!(
        icon_artwork_path(IconKind::Folder),
        "/System/Graphics/Icons/folder.png"
    );
    assert_eq!(
        icon_artwork_path(IconKind::DiskHard),
        "/System/Graphics/Icons/disk-hard.png"
    );
    assert_eq!(
        icon_vector_path(IconKind::ImageSvg),
        "/System/Graphics/Icons/image-svg-xml.svg"
    );
}

#[test]
fn artwork_kind_for_file_round_trips_every_kind_in_both_formats() {
    for kind in ICON_KINDS {
        for suffix in [".png", ".svg"] {
            let name = format!("{}{suffix}", kind.asset_id());
            assert_eq!(artwork_kind_for_file(&name), Some(kind), "{kind:?}{suffix}");
        }
    }
}

#[test]
fn artwork_kind_for_file_refuses_illegal_names() {
    // An unknown id in either class format.
    assert_eq!(artwork_kind_for_file("not-a-real-icon.png"), None);
    assert_eq!(artwork_kind_for_file("not-a-real-icon.svg"), None);
    // A known id in a format no class tier reads.
    assert_eq!(artwork_kind_for_file("folder.jpg"), None);
    // No extension at all.
    assert_eq!(artwork_kind_for_file("folder"), None);
    // An empty name, and each bare extension.
    assert_eq!(artwork_kind_for_file(""), None);
    assert_eq!(artwork_kind_for_file(".png"), None);
    assert_eq!(artwork_kind_for_file(".svg"), None);
    // A directory-bearing name (a path-traversal attempt) is not a bare id.
    assert_eq!(artwork_kind_for_file("../../etc/x.png"), None);
    assert_eq!(artwork_kind_for_file("../../etc/x.svg"), None);
    assert_eq!(artwork_kind_for_file("Icons/folder.png"), None);
    assert_eq!(artwork_kind_for_file("Icons/folder.svg"), None);
}

#[test]
fn a_readable_asset_rasterises_to_a_surface() {
    let mut c = cache();
    let mut reader = CountingReader::new().with("/a.png", vec![0u8; 10]);
    let mut ras = SquareRasteriser;
    let surface = c
        .path_artwork(&mut InlineArtwork::new(&mut reader, &mut ras), "/a.png", 4)
        .expect("artwork");
    assert_eq!(surface.width(), 4);
    assert_eq!(surface.height(), 4);
}

#[test]
fn a_cached_hit_does_not_re_read() {
    let mut c = cache();
    let mut reader = CountingReader::new().with("/a.png", vec![1, 2, 3]);
    let mut ras = SquareRasteriser;
    assert!(c
        .path_artwork(&mut InlineArtwork::new(&mut reader, &mut ras), "/a.png", 8)
        .is_some());
    assert!(c
        .path_artwork(&mut InlineArtwork::new(&mut reader, &mut ras), "/a.png", 8)
        .is_some());
    assert_eq!(reader.reads, 1, "the second lookup served from the cache");
}

#[test]
fn an_unreadable_asset_caches_the_refusal() {
    let mut c = cache();
    let mut reader = CountingReader::new();
    let mut ras = SquareRasteriser;
    assert!(c
        .path_artwork(
            &mut InlineArtwork::new(&mut reader, &mut ras),
            "/missing.png",
            8
        )
        .is_none());
    assert!(c
        .path_artwork(
            &mut InlineArtwork::new(&mut reader, &mut ras),
            "/missing.png",
            8
        )
        .is_none());
    assert_eq!(reader.reads, 1, "the negative result was cached");
}

#[test]
fn an_oversize_asset_is_refused_before_rasterising() {
    let mut c = cache();
    let mut reader = CountingReader::new().with("/big.png", vec![0u8; MAX_ARTWORK_BYTES + 1]);
    let mut ras = PanicRasteriser;
    assert!(c
        .path_artwork(
            &mut InlineArtwork::new(&mut reader, &mut ras),
            "/big.png",
            8
        )
        .is_none());
}

#[test]
fn a_wrong_length_reply_is_refused() {
    let mut c = cache();
    let mut reader = CountingReader::new().with("/a.png", vec![0u8; 10]);
    let mut ras = ShortRasteriser;
    assert!(c
        .path_artwork(&mut InlineArtwork::new(&mut reader, &mut ras), "/a.png", 8)
        .is_none());
}

#[test]
fn a_zero_side_is_refused_without_reading() {
    let mut c = cache();
    let mut reader = CountingReader::new().with("/a.png", vec![0u8; 10]);
    let mut ras = SquareRasteriser;
    assert!(c
        .path_artwork(&mut InlineArtwork::new(&mut reader, &mut ras), "/a.png", 0)
        .is_none());
    assert_eq!(reader.reads, 0, "a zero side never reaches the reader");
}

#[test]
fn a_kind_request_resolves_the_raster_class_master() {
    let mut c = cache();
    let path = icon_artwork_path(IconKind::Folder);
    let mut reader = CountingReader::new().with(&path, vec![0u8; 10]);
    let mut ras = SquareRasteriser;
    assert!(c
        .artwork(
            &mut InlineArtwork::new(&mut reader, &mut ras),
            IconRequest::kind(IconKind::Folder),
            8
        )
        .is_some());
}

/// A class shipping only a vector master still draws: the class tier falls
/// through the absent raster to the vector before the glyph.
#[test]
fn a_kind_with_only_a_vector_master_resolves_through_the_vector_tier() {
    let mut c = cache();
    let vector = icon_vector_path(IconKind::Folder);
    let mut reader = CountingReader::new().with(&vector, vec![0u8; 10]);
    let mut ras = SquareRasteriser;
    assert!(c
        .artwork(
            &mut InlineArtwork::new(&mut reader, &mut ras),
            IconRequest::kind(IconKind::Folder),
            8
        )
        .is_some());
    assert_eq!(
        reader.read_paths,
        vec![icon_artwork_path(IconKind::Folder), vector],
        "the raster is tried first, the vector second"
    );
}

/// Shipping one kind in both formats is a packaging defect the image build
/// refuses; the runtime is deterministic about it regardless — the raster
/// wins and the vector is never read.
#[test]
fn a_kind_shipping_both_formats_prefers_the_raster() {
    let mut c = cache();
    let raster = icon_artwork_path(IconKind::Folder);
    let vector = icon_vector_path(IconKind::Folder);
    let mut reader = CountingReader::new()
        .with(&raster, vec![0u8; 10])
        .with(&vector, vec![0u8; 10]);
    let mut ras = SquareRasteriser;
    assert!(c
        .artwork(
            &mut InlineArtwork::new(&mut reader, &mut ras),
            IconRequest::kind(IconKind::Folder),
            8
        )
        .is_some());
    assert_eq!(reader.read_paths, vec![raster], "the vector was never read");
}

/// A class shipping neither master falls back to the glyph, and both
/// refusals are retained: such a kind costs one read per class format once,
/// not one per frame.
#[test]
fn a_kind_with_no_class_master_is_remembered_as_having_none() {
    let mut c = cache();
    let mut reader = CountingReader::new();
    let mut ras = SquareRasteriser;
    let request = IconRequest::kind(IconKind::Folder);
    assert!(
        matches!(
            c.artwork(&mut InlineArtwork::new(&mut reader, &mut ras), request, 8),
            Some(IconPicture::Mask(_))
        ),
        "neither class master resolved, so the built-in glyph answers"
    );
    assert_eq!(reader.reads, 2, "each class format was tried once");
    assert!(matches!(
        c.artwork(&mut InlineArtwork::new(&mut reader, &mut ras), request, 8),
        Some(IconPicture::Mask(_))
    ));
    assert_eq!(reader.reads, 2, "both refusals were retained");
}

#[test]
fn no_artwork_never_resolves() {
    let mut none = NoArtwork;
    assert!(none
        .artwork(IconRequest::kind(IconKind::Folder), 16)
        .is_none());
}

#[test]
fn icon_artwork_source_resolves_through_the_cache() {
    let mut c = cache();
    let path = icon_artwork_path(IconKind::AppBundle);
    let mut reader = CountingReader::new().with(&path, vec![0u8; 10]);
    let mut ras = SquareRasteriser;
    let mut inline = InlineArtwork::new(&mut reader, &mut ras);
    let mut source = IconArtworkSource::new(&mut c, &mut inline);
    assert!(source
        .artwork(IconRequest::kind(IconKind::AppBundle), 8)
        .is_some());
}

/// A structurally valid manifest naming `icon` as the bundle's own icon (or
/// declaring none when `icon` is empty).
///
/// The artwork layer decodes the header's shape, not its signature — the
/// signed load gate is what admits a bundle — so a test manifest needs only
/// to be well-formed.
fn manifest(icon: &str) -> Vec<u8> {
    fn inline<const N: usize>(text: &str) -> ([u8; N], u8) {
        let mut buf = [0u8; N];
        buf[..text.len()].copy_from_slice(text.as_bytes());
        (buf, u8::try_from(text.len()).expect("short"))
    }
    let (id, id_len) = inline::<BUNDLE_ID_MAX>("os.tairix.test");
    let (name, name_len) = inline::<BUNDLE_NAME_MAX>("test");
    let (version, version_len) = inline::<BUNDLE_VERSION_MAX>("0.1.0");
    let (library_icon, library_icon_len) = inline::<LIBRARY_ICON_MAX>(icon);
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

#[test]
fn a_bundle_draws_the_icon_its_own_manifest_names() {
    let mut c = cache();
    let mut reader = CountingReader::new()
        .with("/Apps/x.app/AppInfo", manifest("x.png"))
        .with("/Apps/x.app/Resources/x.png", vec![0u8; 10]);
    let mut ras = SquareRasteriser;
    let surface = c
        .artwork(
            &mut InlineArtwork::new(&mut reader, &mut ras),
            IconRequest::bundle(IconKind::AppBundle, "/Apps/x.app"),
            8,
        )
        .expect("the bundle's own icon")
        .artwork()
        .expect("shipped artwork, not a glyph mask");
    assert_eq!(surface.width(), 8);
    assert_eq!(reader.reads, 2, "the manifest and the asset it names");
}

#[test]
fn a_bundle_with_no_icon_of_its_own_falls_back_to_its_kind() {
    let mut c = cache();
    let kind_path = icon_artwork_path(IconKind::AppBundle);
    let mut reader = CountingReader::new()
        .with("/Apps/x.app/AppInfo", manifest(""))
        .with(&kind_path, vec![0u8; 10]);
    let mut ras = SquareRasteriser;
    assert!(c
        .artwork(
            &mut InlineArtwork::new(&mut reader, &mut ras),
            IconRequest::bundle(IconKind::AppBundle, "/Apps/x.app"),
            8,
        )
        .is_some());
}

#[test]
fn a_bundle_whose_icon_will_not_serve_falls_back_and_never_blanks() {
    let mut c = cache();
    let kind_path = icon_artwork_path(IconKind::AppBundle);
    // The manifest names an icon, but the asset is absent from the bundle.
    let mut reader = CountingReader::new()
        .with("/Apps/x.app/AppInfo", manifest("gone.png"))
        .with(&kind_path, vec![0u8; 10]);
    let mut ras = SquareRasteriser;
    assert!(c
        .artwork(
            &mut InlineArtwork::new(&mut reader, &mut ras),
            IconRequest::bundle(IconKind::AppBundle, "/Apps/x.app"),
            8,
        )
        .is_some());

    // A bundle with no manifest at all resolves the same way.
    let mut bare = CountingReader::new().with(&kind_path, vec![0u8; 10]);
    assert!(c
        .artwork(
            &mut InlineArtwork::new(&mut bare, &mut ras),
            IconRequest::bundle(IconKind::AppBundle, "/Apps/bare.app"),
            8,
        )
        .is_some());
}

#[test]
fn a_bundle_icon_escaping_its_own_resources_is_refused() {
    let mut c = cache();
    let kind_path = icon_artwork_path(IconKind::AppBundle);
    let mut reader = CountingReader::new()
        .with("/Apps/x.app/AppInfo", manifest("../../../System/secret"))
        // The file the hostile name aims at, reachable if the name were ever
        // joined as a path rather than validated as a leaf.
        .with("/System/secret", vec![0u8; 10])
        .with(&kind_path, vec![0u8; 10]);
    let mut ras = SquareRasteriser;
    let surface = c
        .artwork(
            &mut InlineArtwork::new(&mut reader, &mut ras),
            IconRequest::bundle(IconKind::AppBundle, "/Apps/x.app"),
            8,
        )
        .expect("the class artwork still resolves")
        .artwork()
        .expect("shipped artwork, not a glyph mask");
    assert_eq!(surface.width(), 8);
    assert!(
        !reader
            .read_paths
            .iter()
            .any(|path| path == "/System/secret"),
        "a traversing icon name must never be read: {:?}",
        reader.read_paths
    );
}

#[test]
fn a_bundle_with_no_icon_re_reads_neither_its_manifest_nor_the_kind_asset() {
    let mut c = cache();
    let kind_path = icon_artwork_path(IconKind::AppBundle);
    let mut reader = CountingReader::new()
        .with("/Apps/x.app/AppInfo", manifest(""))
        .with(&kind_path, vec![0u8; 10]);
    let mut ras = SquareRasteriser;
    let request = IconRequest::bundle(IconKind::AppBundle, "/Apps/x.app");
    assert!(c
        .artwork(&mut InlineArtwork::new(&mut reader, &mut ras), request, 8)
        .is_some());
    let after_first = reader.reads;
    assert!(c
        .artwork(&mut InlineArtwork::new(&mut reader, &mut ras), request, 8)
        .is_some());
    assert_eq!(
        reader.reads, after_first,
        "both the manifest refusal and the class artwork were retained"
    );
}

#[test]
fn an_asset_request_prefers_the_named_asset_over_the_kind() {
    let mut c = cache();
    let kind_path = icon_artwork_path(IconKind::AppBundle);
    let mut reader = CountingReader::new()
        .with("/Apps/x.app/Resources/x.png", vec![0u8; 10])
        .with(&kind_path, vec![0u8; 10]);
    let mut ras = SquareRasteriser;
    assert!(c
        .artwork(
            &mut InlineArtwork::new(&mut reader, &mut ras),
            IconRequest::asset(IconKind::AppBundle, "/Apps/x.app/Resources/x.png"),
            8,
        )
        .is_some());
    assert_eq!(reader.reads, 1, "the class asset was never consulted");
}

#[test]
fn charged_bytes_grows_on_admit_and_teardown_clears() {
    let mut c = cache();
    assert_eq!(c.charged_bytes(), 0);
    let mut reader = CountingReader::new().with("/a.png", vec![0u8; 10]);
    let mut ras = SquareRasteriser;
    assert!(c
        .path_artwork(&mut InlineArtwork::new(&mut reader, &mut ras), "/a.png", 8)
        .is_some());
    assert!(c.charged_bytes() > 0);
    c.teardown();
    assert_eq!(c.charged_bytes(), 0);
}

#[test]
fn trim_under_normal_pressure_releases_nothing() {
    let mut c = cache();
    assert_eq!(c.trim(), 0);
}

#[test]
fn ledger_forwards_the_wrapped_caches_ledger() {
    let c = cache();
    let ledger = c.ledger().expect("a classified cache has a ledger");
    assert_eq!(ledger.label(), "test.icon-artwork");
}

/// Every shipped class master is artwork the desktop can resolve and draw:
/// its name maps back to exactly one kind in one of the two class formats,
/// it is within the artwork byte bound, and a vector one decodes through the
/// shared SVG decoder to a visible silhouette.
///
/// A kind ships at most one master. Two files claiming one asset id in
/// different formats would leave the vector unreachable, because the class
/// tier prefers the raster.
#[test]
fn every_shipped_asset_is_artwork_the_desktop_can_resolve_and_draw() {
    let dir = concat!(env!("CARGO_MANIFEST_DIR"), "/assets");
    let mut ids = BTreeSet::new();
    let mut vectors = 0usize;
    for entry in std::fs::read_dir(dir).expect("assets directory") {
        let entry = entry.expect("directory entry");
        let name = entry.file_name();
        let name = name.to_str().expect("utf-8 asset name");
        let kind = artwork_kind_for_file(name)
            .unwrap_or_else(|| panic!("shipped asset {name} is not a resolvable artwork name"));
        assert!(
            ids.insert(kind.asset_id()),
            "asset id {} ships a master in both formats ({name})",
            kind.asset_id()
        );
        let len = entry.metadata().expect("asset metadata").len();
        assert!(
            usize::try_from(len).is_ok_and(|bytes| bytes <= MAX_ARTWORK_BYTES),
            "shipped asset {name} is {len} bytes, over the artwork cap"
        );
        if name.strip_suffix(VECTOR_SUFFIX).is_some() {
            let bytes = std::fs::read(entry.path()).expect("asset bytes");
            let icon = crate::svg::decode(&bytes)
                .unwrap_or_else(|err| panic!("shipped vector {name} is out of subset: {err:?}"));
            let image = icon.rasterise(64).expect("renderable");
            assert!(
                image.pixels().iter().any(|pixel| pixel.a > 0),
                "shipped vector {name} rasterised to nothing"
            );
            vectors += 1;
        }
    }
    assert!(
        ids.len() >= 20,
        "expected the shipped icon assets, found {}",
        ids.len()
    );
    assert!(vectors > 0, "the vector class tier ships no master");
}

/// The shipped vector master `name`, decoded.
#[track_caller]
fn shipped_vector(name: &str) -> crate::vector::VectorIcon {
    let path = format!("{}/assets/{name}", env!("CARGO_MANIFEST_DIR"));
    let bytes = std::fs::read(&path).unwrap_or_else(|_| panic!("{name} should ship"));
    crate::svg::decode(&bytes).unwrap_or_else(|err| panic!("{name} should decode: {err:?}"))
}

/// How many of `icon`'s layers are painted flat in `color`.
fn layers_painted(icon: &crate::vector::VectorIcon, color: tairix_raster::Color) -> usize {
    icon.layers()
        .iter()
        .filter(|layer| layer.paint == tairix_raster::Paint::Solid(color))
        .count()
}

/// The folder masters are the designer's own drawings rather than traced
/// approximations, and they are the vector class tier's whole shipped set.
/// Pinning what they decode to is what would catch the decoder quietly
/// dropping a shape and leaving a wrong picture on every folder on screen.
///
/// The illustrative class masters beside them are raster, so nothing here
/// decodes those; the image build proves each is artwork the desktop will draw.
#[test]
fn the_folder_icons_draw_the_artwork_they_were_authored_with() {
    let plate = tairix_raster::Color::rgb(0x3A, 0x86, 0xE8);
    let body = tairix_raster::Color::rgb(0x4C, 0x9A, 0xF0);
    let accent = tairix_raster::Color::rgb(0x65, 0xAA, 0xF5);
    let paper = tairix_raster::Color::rgb(0xFF, 0xFF, 0xFF);

    // A back plate with its tab, a front body, and a flat top accent.
    let folder = shipped_vector("folder.svg");
    assert_eq!(folder.layers().len(), 3);
    for ink in [plate, body, accent] {
        assert_eq!(layers_painted(&folder, ink), 1);
    }

    // The same folder with three papers stacked between plate and body, each
    // paper an edge, a fill, and a two-layer turned corner.
    let filled = shipped_vector("folder-filled.svg");
    assert_eq!(filled.layers().len(), 15);
    for ink in [plate, body, accent] {
        assert_eq!(layers_painted(&filled, ink), 1);
    }
    assert_eq!(layers_painted(&filled, paper), 3, "one fill per paper");

    // Each covers a substantial part of its slot: a silhouette that decoded
    // but drew almost nothing would still pass a "not empty" check.
    for (name, icon) in [("folder", &folder), ("folder-filled", &filled)] {
        let image = icon.rasterise(64).expect("renderable");
        let drawn = image.pixels().iter().filter(|pixel| pixel.a > 0).count();
        assert!(
            drawn > 64 * 64 / 4,
            "{name} covers only {drawn} of {} pixels",
            64 * 64
        );
    }
}

// ---------------------------------------------------------------------
// The deferring resolver
// ---------------------------------------------------------------------

/// A resolver standing in for a worker thread: it answers `Pending` for a key
/// it has not been *primed* with, records the ask, and answers `Done` once the
/// test says the decode has landed.
///
/// This is the whole of what an off-thread producer looks like to the cache, so
/// the rules below are the ones a real decoder thread depends on.
struct Deferring {
    ready: BTreeMap<(ArtworkKey, u32), Option<Surface>>,
    asked: Vec<(ArtworkKey, u32)>,
    warmed: Vec<(ArtworkKey, u32)>,
}

impl Deferring {
    fn new() -> Self {
        Self {
            ready: BTreeMap::new(),
            asked: Vec::new(),
            warmed: Vec::new(),
        }
    }

    /// Say that the decode of `key` at `side` has landed, as `artwork`.
    fn land(&mut self, key: &ArtworkKey, side: u32, artwork: Option<Surface>) {
        self.ready.insert((key.clone(), side), artwork);
    }
}

impl ArtworkResolver for Deferring {
    fn resolve(&mut self, key: &ArtworkKey, side: u32) -> Resolved {
        self.asked.push((key.clone(), side));
        self.ready
            .remove(&(key.clone(), side))
            .map_or(Resolved::Pending, Resolved::Done)
    }

    fn prefetch(&mut self, key: &ArtworkKey, side: u32) {
        self.warmed.push((key.clone(), side));
    }
}

fn square(side: u32) -> Surface {
    Surface::new(side, side).expect("a square surface")
}

#[test]
fn a_pending_decode_retains_nothing_and_draws_the_glyph() {
    let mut c = cache();
    let mut deferring = Deferring::new();
    assert!(c.path_artwork(&mut deferring, "/a.png", 8).is_none());
    assert_eq!(c.charged_bytes(), 0, "an unfinished decode retains nothing");
    assert_eq!(deferring.asked.len(), 1);
}

#[test]
fn a_landed_decode_is_served_and_retained_on_the_next_ask() {
    let mut c = cache();
    let mut deferring = Deferring::new();
    assert!(c.path_artwork(&mut deferring, "/a.png", 8).is_none());

    deferring.land(&ArtworkKey::Asset("/a.png".to_string()), 8, Some(square(8)));
    assert_eq!(
        c.path_artwork(&mut deferring, "/a.png", 8)
            .map(Surface::width),
        Some(8)
    );
    assert!(c.charged_bytes() > 0);

    // Retained, so the resolver is not consulted a third time.
    let asked = deferring.asked.len();
    assert!(c.path_artwork(&mut deferring, "/a.png", 8).is_some());
    assert_eq!(
        deferring.asked.len(),
        asked,
        "the third ask was a cache hit"
    );
}

/// Whether a later tier is reached at all depends on what this one turns out
/// to be, so a pending tier stops the walk rather than racing ahead and
/// decoding artwork the bundle's own icon would have replaced.
#[test]
fn a_pending_tier_stops_the_walk_rather_than_falling_through() {
    let mut c = cache();
    let mut deferring = Deferring::new();
    let request = IconRequest::asset(IconKind::AppBundle, "/Apps/One.app/Resources/icon.png");
    assert!(matches!(
        c.artwork(&mut deferring, request, 8),
        Some(IconPicture::Mask(_))
    ));
    assert_eq!(
        deferring.asked,
        vec![(
            ArtworkKey::Asset("/Apps/One.app/Resources/icon.png".to_string()),
            8
        )],
        "the class tiers were asked for before the thing's own icon resolved"
    );
}

/// A refusal is an answer: it advances the walk, one tier per landing, and the
/// request costs exactly the reads a synchronous walk would.
#[test]
fn a_landed_refusal_advances_the_walk_one_tier_at_a_time() {
    let mut c = cache();
    let mut deferring = Deferring::new();
    let own = ArtworkKey::Asset("/Apps/One.app/Resources/icon.png".to_string());
    let raster = ArtworkKey::Asset(icon_artwork_path(IconKind::AppBundle));
    let request = IconRequest::asset(IconKind::AppBundle, "/Apps/One.app/Resources/icon.png");

    deferring.land(&own, 8, None);
    assert!(
        matches!(
            c.artwork(&mut deferring, request, 8),
            Some(IconPicture::Mask(_))
        ),
        "the own-icon tier refused, so the next tier is only now asked for"
    );
    assert_eq!(deferring.asked, vec![(own, 8), (raster.clone(), 8)]);

    deferring.land(&raster, 8, Some(square(8)));
    assert!(
        c.artwork(&mut deferring, request, 8).is_some(),
        "and the class master serves once it lands"
    );
}

#[test]
fn owned_artwork_tells_a_storing_caller_pending_from_refused() {
    let mut c = cache();
    let mut deferring = Deferring::new();
    let request = IconRequest::kind(IconKind::AppBundle);
    let raster = ArtworkKey::Asset(icon_artwork_path(IconKind::AppBundle));
    let vector = ArtworkKey::Asset(icon_vector_path(IconKind::AppBundle));

    assert!(matches!(
        c.owned_artwork(&mut deferring, request, 8),
        ArtworkOutcome::Pending
    ));

    deferring.land(&raster, 8, None);
    assert!(
        matches!(
            c.owned_artwork(&mut deferring, request, 8),
            ArtworkOutcome::Pending
        ),
        "the raster tier refused, so the vector tier is only now asked for"
    );

    deferring.land(&vector, 8, None);
    assert!(
        matches!(
            c.owned_artwork(&mut deferring, request, 8),
            ArtworkOutcome::Refused
        ),
        "both class tiers refused, so asking again would only repeat them"
    );
}

#[test]
fn owned_artwork_hands_back_a_picture_the_cache_could_not_retain() {
    // An output too small for its budget to hold one decode: the state in
    // which the cache genuinely retains nothing, since no band empties it
    // below the reserve it declares.
    let (_gauge, mut c) = cache_at_on(PressureBand::Normal, FB_TOO_SMALL_FOR_ONE_ICON);
    let mut reader = CountingReader::new().with("/a.png", vec![0u8; 10]);
    let mut ras = SquareRasteriser;
    let request = IconRequest::asset(IconKind::AppBundle, "/a.png");

    let inline = &mut InlineArtwork::new(&mut reader, &mut ras);
    assert!(
        c.artwork(inline, request, 8).is_none(),
        "a borrow cannot be served out of a cache retaining nothing"
    );
    assert!(
        matches!(
            c.owned_artwork(inline, request, 8),
            ArtworkOutcome::Ready(_)
        ),
        "but a caller that copies the pixels out gets the decode rather than \
         seeing it thrown away"
    );
    assert_eq!(c.charged_bytes(), 0, "and still nothing is retained");
}

/// Warming asks the producer for the tier that will serve, and draws nothing —
/// which is what lets a desktop have its icons decoded before the surface that
/// shows them is ever painted.
#[test]
fn a_warm_up_asks_for_the_serving_tier_and_retains_nothing() {
    let mut c = cache();
    let mut deferring = Deferring::new();
    let own = ArtworkKey::Asset("/Apps/One.app/Resources/icon.png".to_string());
    let request = IconRequest::asset(IconKind::AppBundle, "/Apps/One.app/Resources/icon.png");

    c.prefetch(&mut deferring, request, 8);
    assert_eq!(deferring.warmed, vec![(own.clone(), 8)]);
    assert!(deferring.asked.is_empty(), "a warm-up resolves nothing");
    assert_eq!(c.charged_bytes(), 0, "and retains nothing");

    // Warming the same request again asks for nothing new: the producer is
    // already on it, and a desktop re-reading its catalog must not re-ask.
    c.prefetch(&mut deferring, request, 8);
    assert_eq!(
        deferring.warmed.len(),
        2,
        "the cache holds no record of what was asked for, so the producer \
         dedupes — this is only that it is asked, not that it decodes twice"
    );

    // Once it lands and is retained, the warm-up asks for nothing at all.
    deferring.land(&own, 8, Some(square(8)));
    assert!(c.artwork(&mut deferring, request, 8).is_some());
    let warmed = deferring.warmed.len();
    c.prefetch(&mut deferring, request, 8);
    assert_eq!(
        deferring.warmed.len(),
        warmed,
        "an icon already in the cache is not warmed again"
    );
}

/// The tier a warm-up asks for is the first one not already held, so a class
/// master whose raster form was refused is warmed at its vector form — the same
/// tier the paint would reach.
#[test]
fn a_warm_up_skips_the_tiers_already_refused() {
    let mut c = cache();
    let mut deferring = Deferring::new();
    let raster = ArtworkKey::Asset(icon_artwork_path(IconKind::AppBundle));
    let vector = ArtworkKey::Asset(icon_vector_path(IconKind::AppBundle));
    let request = IconRequest::kind(IconKind::AppBundle);

    deferring.land(&raster, 8, None);
    assert!(matches!(
        c.artwork(&mut deferring, request, 8),
        Some(IconPicture::Mask(_))
    ));

    deferring.warmed.clear();
    c.prefetch(&mut deferring, request, 8);
    assert_eq!(
        deferring.warmed,
        vec![(vector, 8)],
        "the refused raster tier is retained, so the vector tier is what is wanted"
    );
}

/// A resolver that counts decodes and records every key it was asked for,
/// so a test can prove the desktop does not ask again for an answer that
/// cannot be kept.
struct DecliningResolver<'r> {
    inner: InlineArtwork<&'r mut CountingReader, &'r mut SquareRasteriser>,
    declined: Vec<(ArtworkKey, u32)>,
    decodes: usize,
}

impl<'r> DecliningResolver<'r> {
    fn new(reader: &'r mut CountingReader, rasteriser: &'r mut SquareRasteriser) -> Self {
        Self {
            inner: InlineArtwork::new(reader, rasteriser),
            declined: Vec::new(),
            decodes: 0,
        }
    }
}

impl ArtworkResolver for DecliningResolver<'_> {
    fn resolve(&mut self, key: &ArtworkKey, side: u32) -> Resolved {
        self.decodes += 1;
        self.inner.resolve(key, side)
    }

    fn declined(&mut self, key: &ArtworkKey, side: u32) {
        self.declined.push((key.clone(), side));
    }
}

/// A cache at `band`, wired like a real seat's.
fn cache_at(band: PressureBand) -> (&'static ReportedPressure, ArtworkCache) {
    cache_at_on(band, 1920 * 1080 * 4)
}

/// The same, on an output of `fb_bytes` — how a caller reaches a budget too
/// small to hold one decode, which is the only state in which the cache
/// genuinely cannot keep a picture (no band empties it below its reserve).
fn cache_at_on(band: PressureBand, fb_bytes: usize) -> (&'static ReportedPressure, ArtworkCache) {
    let gauge: &'static ReportedPressure = Box::leak(Box::new(ReportedPressure::unknown()));
    gauge.report(band);
    let sink: &'static DiscardSink = Box::leak(Box::new(DiscardSink));
    (
        gauge,
        artwork_cache("test.icon-artwork", 1, fb_bytes, gauge, sink),
    )
}

/// An output whose whole artwork budget — one screenful of it — is smaller
/// than a single decode's pixels plus its bookkeeping, so every admission is
/// refused. The reserve is clamped to that ceiling, which is what makes
/// "retains nothing" reachable without a band that empties the cache.
const FB_TOO_SMALL_FOR_ONE_ICON: usize = 256;

#[test]
fn tightening_pressure_never_takes_the_drawn_icon_away() {
    // The regression: a desktop whose machine merely *tightened* — a screenful
    // of windows on a small board is enough — drew a built-in glyph in place of
    // every icon it had already decoded, and re-read and re-decoded each of
    // them on every repaint thereafter.
    let path = icon_artwork_path(IconKind::Folder);
    for band in [
        PressureBand::Normal,
        PressureBand::Mild,
        PressureBand::Moderate,
    ] {
        let (_gauge, mut cache) = cache_at(band);
        let mut reader = CountingReader::new().with(&path, vec![0u8; 64]);
        let mut ras = SquareRasteriser;
        let mut resolver = DecliningResolver::new(&mut reader, &mut ras);
        assert!(
            cache
                .artwork(&mut resolver, IconRequest::kind(IconKind::Folder), 32)
                .is_some(),
            "{band:?} draws the decoded artwork"
        );
        assert!(resolver.declined.is_empty(), "{band:?} retained the decode");
        // Drawing it again is a cache hit, so nothing is read twice.
        assert!(cache
            .artwork(&mut resolver, IconRequest::kind(IconKind::Folder), 32)
            .is_some());
        assert_eq!(resolver.decodes, 1, "{band:?} decoded once");
    }
}

#[test]
fn a_decode_the_cache_cannot_keep_is_reported_once_not_repeated() {
    // A budget with no room for the decode at all. What must not happen is the
    // desktop asking for it again on every frame: the resolver is told the
    // answer was declined, so it can hold the key back until something moves.
    let path = icon_artwork_path(IconKind::Folder);
    let (_gauge, mut cache) = cache_at_on(PressureBand::Normal, FB_TOO_SMALL_FOR_ONE_ICON);
    let mut reader = CountingReader::new().with(&path, vec![0u8; 64]);
    let mut ras = SquareRasteriser;
    let mut resolver = DecliningResolver::new(&mut reader, &mut ras);
    let request = IconRequest::kind(IconKind::Folder);

    assert!(
        cache.artwork(&mut resolver, request, 32).is_none(),
        "a budget that cannot hold the decode falls back to the glyph tier"
    );
    assert_eq!(
        resolver.declined,
        vec![(ArtworkKey::Asset(path.clone()), 32)],
        "the refusal is reported to the resolver that produced it"
    );
    assert_eq!(cache.charged_bytes(), 0);

    // On an output whose budget does hold it, the picture is retained.
    let (_gauge, mut roomy) = cache_at(PressureBand::Normal);
    assert!(roomy.artwork(&mut resolver, request, 32).is_some());
    assert!(roomy.charged_bytes() > 0);
}

#[test]
fn no_band_takes_the_decoded_icons_away() {
    // The whole icon working set is a fraction of one frame, and rebuilding an
    // entry costs a capability-gated read plus a parser-sandbox round trip —
    // the resources a machine short of memory has least of. So the reserve
    // holds at every band, critical included, and a desktop under pressure
    // keeps drawing its real artwork rather than re-deriving it per repaint.
    let request = IconRequest::kind(IconKind::Folder);
    for band in PressureBand::ALL {
        let (gauge, mut cache) = cache_at(PressureBand::Normal);
        let path = icon_artwork_path(IconKind::Folder);
        let mut reader = CountingReader::new().with(&path, vec![0u8; 64]);
        let mut ras = SquareRasteriser;
        let mut resolver = DecliningResolver::new(&mut reader, &mut ras);
        assert!(cache.artwork(&mut resolver, request, 32).is_some());
        let charged = cache.charged_bytes();
        assert!(charged > 0);

        gauge.report(band);
        assert_eq!(cache.trim(), 0, "{band:?} released a decoded icon");
        assert_eq!(cache.charged_bytes(), charged, "{band:?}");
        assert!(
            cache.artwork(&mut resolver, request, 32).is_some(),
            "{band:?} draws the retained artwork"
        );
        assert_eq!(resolver.decodes, 1, "{band:?} decoded once");
    }
}

/// The glyph tier is retained like any other: a kind drawn on a hundred rows
/// resolves its coverage once, not once per draw.
///
/// This is the defect the tier exists to close. Resolving a multi-layer glyph
/// means painting its layers enlarged and averaging them back down, and doing
/// that per icon per frame is what made a long listing crawl.
#[test]
fn a_glyph_is_resolved_once_however_many_times_it_is_drawn() {
    let mut c = cache();
    let mut reader = CountingReader::new();
    let mut ras = SquareRasteriser;
    // A kind that ships no asset, so every request falls through to the glyph.
    let request = IconRequest::kind(IconKind::Refresh);

    let first = c.artwork(&mut InlineArtwork::new(&mut reader, &mut ras), request, 18);
    let Some(IconPicture::Mask(mask)) = first else {
        panic!("the glyph tier always resolves");
    };
    assert_eq!(mask.width(), 18);
    assert_eq!(mask.height(), 18);
    let after_first = reader.reads;

    // Ninety-nine more draws of the same kind at the same side read nothing
    // further and produce the same retained mask.
    for _ in 0..99 {
        assert!(matches!(
            c.artwork(&mut InlineArtwork::new(&mut reader, &mut ras), request, 18),
            Some(IconPicture::Mask(_))
        ));
    }
    assert_eq!(
        reader.reads, after_first,
        "a retained glyph costs no further asset reads"
    );

    // A different pixel side is a different picture, so it resolves on its own.
    let Some(IconPicture::Mask(other)) =
        c.artwork(&mut InlineArtwork::new(&mut reader, &mut ras), request, 24)
    else {
        panic!("the glyph tier always resolves at any side");
    };
    assert_eq!(other.width(), 24);
}

/// A glyph mask carries coverage, not colour, so the same retained mask serves
/// every tint a control draws it in — which is what keeps the cache key free of
/// the theme's state colours.
#[test]
fn one_glyph_mask_serves_every_tint() {
    let mask = glyph_mask(IconKind::Folder, 16).expect("mask");
    // Opaque white: the alpha channel is the coverage and the colour channels
    // never tint what is drawn from it.
    let opaque = (0..16)
        .flat_map(|y| (0..16).map(move |x| (x, y)))
        .filter_map(|(x, y)| mask.get(x, y))
        .find(|pixel| pixel.a == 255)
        .expect("the glyph covers some pixel fully");
    assert_eq!((opaque.r, opaque.g, opaque.b), (255, 255, 255));
}
