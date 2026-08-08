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
    artwork_cache, artwork_kind_for_file, icon_artwork_path, icon_vector_path, ArtworkCache,
    ArtworkRasteriser, ArtworkReader, IconArtwork, IconArtworkSource, IconRequest, NoArtwork,
    MAX_ARTWORK_BYTES, VECTOR_SUFFIX,
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
fn a_kind_request_resolves_the_raster_class_master() {
    let mut c = cache();
    let path = icon_artwork_path(IconKind::Folder);
    let mut reader = CountingReader::new().with(&path, vec![0u8; 10]);
    let mut ras = SquareRasteriser;
    assert!(c
        .artwork(
            &mut reader,
            &mut ras,
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
            &mut reader,
            &mut ras,
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
            &mut reader,
            &mut ras,
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
    assert!(c.artwork(&mut reader, &mut ras, request, 8).is_none());
    assert_eq!(reader.reads, 2, "each class format was tried once");
    assert!(c.artwork(&mut reader, &mut ras, request, 8).is_none());
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
    let mut source = IconArtworkSource::new(&mut c, &mut reader, &mut ras);
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
        library_icon_len,
        library: 0,
        reserved0: [0; 3],
        id,
        name,
        version,
        library_icon,
        syscall_table_hash: [0; SYSCALL_TABLE_HASH_LEN],
        content_hash: [0; 32],
        signer_pubkey: [0; 32],
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
            &mut reader,
            &mut ras,
            IconRequest::bundle(IconKind::AppBundle, "/Apps/x.app"),
            8,
        )
        .expect("the bundle's own icon");
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
            &mut reader,
            &mut ras,
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
            &mut reader,
            &mut ras,
            IconRequest::bundle(IconKind::AppBundle, "/Apps/x.app"),
            8,
        )
        .is_some());

    // A bundle with no manifest at all resolves the same way.
    let mut bare = CountingReader::new().with(&kind_path, vec![0u8; 10]);
    assert!(c
        .artwork(
            &mut bare,
            &mut ras,
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
            &mut reader,
            &mut ras,
            IconRequest::bundle(IconKind::AppBundle, "/Apps/x.app"),
            8,
        )
        .expect("the class artwork still resolves");
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
    assert!(c.artwork(&mut reader, &mut ras, request, 8).is_some());
    let after_first = reader.reads;
    assert!(c.artwork(&mut reader, &mut ras, request, 8).is_some());
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
            &mut reader,
            &mut ras,
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
