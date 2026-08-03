//! Unit tests for the shared icon-artwork layer.
//!
//! The crate is `no_std`, but a test module may use `std`: the shipped-asset
//! walk reads the crate's own `assets/` directory off the host filesystem.

extern crate std;

use alloc::boxed::Box;
use alloc::collections::BTreeMap;
use alloc::format;
use alloc::string::{String, ToString};
use alloc::vec;
use alloc::vec::Vec;

use tairix_log::DiscardSink;
use tairix_reclaim::pressure::{PressureBand, ReportedPressure};

use super::{
    artwork_cache, artwork_kind_for_file, icon_artwork_path, icon_vector_path, ArtworkCache,
    ArtworkRasteriser, ArtworkReader, IconArtwork, IconArtworkSource, NoArtwork, MAX_ARTWORK_BYTES,
};
use crate::glyph::IconKind;
use crate::load::ICON_KINDS;

/// A reader over an in-memory file table that counts every read, so a test
/// can prove a second lookup for the same key served from the cache rather
/// than reading the asset again.
struct CountingReader {
    files: BTreeMap<String, Vec<u8>>,
    reads: usize,
}

impl CountingReader {
    fn new() -> Self {
        Self {
            files: BTreeMap::new(),
            reads: 0,
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
fn artwork_kind_for_file_round_trips_every_kind() {
    for kind in ICON_KINDS {
        let name = format!("{}.png", kind.asset_id());
        assert_eq!(artwork_kind_for_file(&name), Some(kind), "{kind:?}");
    }
}

#[test]
fn artwork_kind_for_file_refuses_illegal_names() {
    assert_eq!(artwork_kind_for_file("not-a-real-icon.png"), None);
    // The right stem but the wrong extension is not shipped artwork.
    assert_eq!(artwork_kind_for_file("folder.svg"), None);
    // No extension at all.
    assert_eq!(artwork_kind_for_file("folder"), None);
    // An empty name, and the bare extension.
    assert_eq!(artwork_kind_for_file(""), None);
    assert_eq!(artwork_kind_for_file(".png"), None);
    // A directory-bearing name (a path-traversal attempt) is not a bare id.
    assert_eq!(artwork_kind_for_file("../../etc/x.png"), None);
    assert_eq!(artwork_kind_for_file("Icons/folder.png"), None);
}

#[test]
fn a_readable_asset_rasterises_to_a_surface() {
    let mut c = cache();
    let mut reader = CountingReader::new().with("/a.png", vec![0u8; 10]);
    let mut ras = SquareRasteriser;
    let surface = c
        .path_artwork(&mut reader, &mut ras, "/a.png", 4)
        .expect("artwork");
    assert_eq!(surface.width(), 4);
    assert_eq!(surface.height(), 4);
}

#[test]
fn a_cached_hit_does_not_re_read() {
    let mut c = cache();
    let mut reader = CountingReader::new().with("/a.png", vec![1, 2, 3]);
    let mut ras = SquareRasteriser;
    assert!(c.path_artwork(&mut reader, &mut ras, "/a.png", 8).is_some());
    assert!(c.path_artwork(&mut reader, &mut ras, "/a.png", 8).is_some());
    assert_eq!(reader.reads, 1, "the second lookup served from the cache");
}

#[test]
fn an_unreadable_asset_caches_the_refusal() {
    let mut c = cache();
    let mut reader = CountingReader::new();
    let mut ras = SquareRasteriser;
    assert!(c
        .path_artwork(&mut reader, &mut ras, "/missing.png", 8)
        .is_none());
    assert!(c
        .path_artwork(&mut reader, &mut ras, "/missing.png", 8)
        .is_none());
    assert_eq!(reader.reads, 1, "the negative result was cached");
}

#[test]
fn an_oversize_asset_is_refused_before_rasterising() {
    let mut c = cache();
    let mut reader = CountingReader::new().with("/big.png", vec![0u8; MAX_ARTWORK_BYTES + 1]);
    let mut ras = PanicRasteriser;
    assert!(c
        .path_artwork(&mut reader, &mut ras, "/big.png", 8)
        .is_none());
}

#[test]
fn a_wrong_length_reply_is_refused() {
    let mut c = cache();
    let mut reader = CountingReader::new().with("/a.png", vec![0u8; 10]);
    let mut ras = ShortRasteriser;
    assert!(c.path_artwork(&mut reader, &mut ras, "/a.png", 8).is_none());
}

#[test]
fn a_zero_side_is_refused_without_reading() {
    let mut c = cache();
    let mut reader = CountingReader::new().with("/a.png", vec![0u8; 10]);
    let mut ras = SquareRasteriser;
    assert!(c.path_artwork(&mut reader, &mut ras, "/a.png", 0).is_none());
    assert_eq!(reader.reads, 0, "a zero side never reaches the reader");
}

#[test]
fn kind_artwork_reads_the_kind_png_path() {
    let mut c = cache();
    let path = icon_artwork_path(IconKind::Folder);
    let mut reader = CountingReader::new().with(&path, vec![0u8; 10]);
    let mut ras = SquareRasteriser;
    assert!(c
        .kind_artwork(&mut reader, &mut ras, IconKind::Folder, 8)
        .is_some());
}

#[test]
fn no_artwork_never_resolves() {
    let mut none = NoArtwork;
    assert!(none.artwork(IconKind::Folder, 16).is_none());
}

#[test]
fn icon_artwork_source_resolves_through_the_cache() {
    let mut c = cache();
    let path = icon_artwork_path(IconKind::AppBundle);
    let mut reader = CountingReader::new().with(&path, vec![0u8; 10]);
    let mut ras = SquareRasteriser;
    let mut source = IconArtworkSource::new(&mut c, &mut reader, &mut ras);
    assert!(source.artwork(IconKind::AppBundle, 8).is_some());
}

#[test]
fn charged_bytes_grows_on_admit_and_teardown_clears() {
    let mut c = cache();
    assert_eq!(c.charged_bytes(), 0);
    let mut reader = CountingReader::new().with("/a.png", vec![0u8; 10]);
    let mut ras = SquareRasteriser;
    assert!(c.path_artwork(&mut reader, &mut ras, "/a.png", 8).is_some());
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

#[test]
fn every_shipped_asset_is_recognised_and_within_the_cap() {
    let dir = concat!(env!("CARGO_MANIFEST_DIR"), "/assets");
    let mut count = 0usize;
    for entry in std::fs::read_dir(dir).expect("assets directory") {
        let entry = entry.expect("directory entry");
        let name = entry.file_name();
        let name = name.to_str().expect("utf-8 asset name");
        assert!(
            artwork_kind_for_file(name).is_some(),
            "shipped asset {name} is not a resolvable artwork file name"
        );
        let len = entry.metadata().expect("asset metadata").len();
        assert!(
            usize::try_from(len).is_ok_and(|bytes| bytes <= MAX_ARTWORK_BYTES),
            "shipped asset {name} is {len} bytes, over the artwork cap"
        );
        count += 1;
    }
    assert!(
        count >= 20,
        "expected the shipped icon assets, found {count}"
    );
}
