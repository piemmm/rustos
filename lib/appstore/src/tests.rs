//! Host tests for the shared store walk.

use alloc::string::{String, ToString};
use alloc::vec::Vec;

use tairix_abi::{manifest_header, Errno, APPINFO_WIRE_MAX};

use crate::{
    manifest_path, store_roots, user_roots, walk, Bundle, DirEntry, StoreReader, Verdict,
    WalkError, MACHINE_ROOTS, MAX_WALK_DEPTH, MAX_WALK_ENTRIES,
};

/// A decodable manifest naming bundle `id`.
fn manifest(id: &str) -> Vec<u8> {
    manifest_header(id, "Fixture").to_le_bytes().to_vec()
}

/// An in-memory store tree: directories with their entries, and the manifest
/// bytes at each bundle path.
#[derive(Default)]
struct MemTree {
    dirs: Vec<(String, Vec<DirEntry>)>,
    manifests: Vec<(String, Vec<u8>)>,
    /// Directories whose listing is refused rather than absent.
    denied: Vec<String>,
}

impl MemTree {
    /// Record a child of `dir`, creating `dir` if this is its first entry.
    fn entry(&mut self, dir: &str, name: &str, directory: bool) -> &mut Self {
        let child = DirEntry {
            name: name.to_string(),
            directory,
        };
        match self.dirs.iter_mut().find(|(path, _)| path == dir) {
            Some((_, entries)) => entries.push(child),
            None => self.dirs.push((dir.to_string(), alloc::vec![child])),
        }
        self
    }

    /// Record a `.app` directory under `dir` holding manifest `bytes`.
    fn bundle(&mut self, dir: &str, name: &str, bytes: Vec<u8>) -> &mut Self {
        self.entry(dir, name, true);
        self.manifests.push((alloc::format!("{dir}/{name}"), bytes));
        self
    }

    /// Record a `.app` directory under `dir` with no manifest at all.
    fn manifestless(&mut self, dir: &str, name: &str) -> &mut Self {
        self.entry(dir, name, true)
    }

    fn denied(&mut self, dir: &str) -> &mut Self {
        self.denied.push(dir.to_string());
        self
    }
}

impl StoreReader for MemTree {
    fn list_dir(&self, path: &str) -> Result<Option<Vec<DirEntry>>, Errno> {
        if self.denied.iter().any(|dir| dir == path) {
            return Err(Errno::PermissionDenied);
        }
        Ok(self
            .dirs
            .iter()
            .find(|(dir, _)| dir == path)
            .map(|(_, entries)| entries.clone()))
    }

    fn read_appinfo(&self, bundle: &str) -> Result<Option<Vec<u8>>, Errno> {
        Ok(self
            .manifests
            .iter()
            .find(|(path, _)| path == bundle)
            .map(|(_, bytes)| bytes.clone()))
    }
}

/// Every bundle path a walk of `roots` visited, with the root each was found
/// under, plus the scan's own counts.
fn visited(tree: &MemTree, roots: &[&str]) -> (Vec<(String, usize)>, crate::Scan) {
    let mut seen = Vec::new();
    let scan = walk(tree, roots, |bundle: Bundle<'_>| {
        seen.push((bundle.path.to_string(), bundle.root));
        Verdict::Accepted
    })
    .expect("the walk completes");
    (seen, scan)
}

#[test]
fn the_roots_are_the_stores_in_resolution_precedence_and_a_homeless_session_gets_the_machine_pair()
{
    assert_eq!(
        store_roots(Some("/Users/ada")),
        [
            "/System/Commands",
            "/System/Applications",
            "/Apps",
            "/Users/ada/Commands",
            "/Users/ada/Applications",
        ]
    );
    // The system stores precede every user-writable directory, which is what
    // makes a duplicate resolve to the shipped bundle.
    assert_eq!(store_roots(None), MACHINE_ROOTS);
    assert!(user_roots(None).is_empty());
    assert!(user_roots(Some("")).is_empty());
    // A trailing separator is the same home, not a doubled one.
    assert_eq!(
        user_roots(Some("/Users/ada/")),
        user_roots(Some("/Users/ada"))
    );
}

#[test]
fn the_manifest_sits_inside_the_bundle_directory() {
    assert_eq!(
        manifest_path("/Apps/Example.app"),
        "/Apps/Example.app/AppInfo"
    );
}

#[test]
fn a_walk_finds_nested_bundles_and_never_descends_into_one() {
    let mut tree = MemTree::default();
    tree.bundle("/Apps", "chess.app", manifest("os.tairix.chess"))
        .entry("/Apps", "games", true)
        .entry("/Apps", "README", false);
    tree.bundle("/Apps/games", "go.app", manifest("os.tairix.go"));
    // A `.app` is a sealed unit: a directory *inside* one is never listed,
    // so a bundle can never contain another.
    tree.bundle(
        "/Apps/chess.app",
        "nested.app",
        manifest("os.tairix.nested"),
    );

    let (seen, scan) = visited(&tree, &["/Apps"]);
    assert_eq!(
        seen,
        [
            ("/Apps/chess.app".to_string(), 0),
            ("/Apps/games/go.app".to_string(), 0),
        ]
    );
    assert_eq!(scan.accepted, 2);
    assert_eq!(scan.skipped, 0);
}

#[test]
fn each_bundle_carries_the_precedence_of_the_root_it_was_found_under() {
    let mut tree = MemTree::default();
    tree.bundle(
        "/System/Applications",
        "view.app",
        manifest("os.tairix.view"),
    );
    tree.bundle(
        "/Users/ada/Applications",
        "view.app",
        manifest("os.tairix.view"),
    );

    let (seen, _) = visited(&tree, &["/System/Applications", "/Users/ada/Applications"]);
    assert_eq!(
        seen,
        [
            ("/System/Applications/view.app".to_string(), 0),
            ("/Users/ada/Applications/view.app".to_string(), 1),
        ],
        "a consumer resolving two bundles claiming one identity reads the \
         precedence rather than re-deriving it from the path"
    );
}

#[test]
fn an_absent_root_contributes_nothing_and_a_refused_listing_fails_the_scan_closed() {
    let mut tree = MemTree::default();
    tree.bundle("/Apps", "chess.app", manifest("os.tairix.chess"));

    // A machine without a per-user store is the ordinary case.
    let (seen, _) = visited(&tree, &["/Users/ada/Applications", "/Apps"]);
    assert_eq!(seen.len(), 1);

    tree.denied("/System/Applications");
    let refused = walk(&tree, &["/System/Applications"], |_| Verdict::Accepted);
    assert_eq!(refused, Err(WalkError::Listing(Errno::PermissionDenied)));
}

#[test]
fn a_bundle_whose_manifest_cannot_be_used_is_skipped_and_the_scan_carries_on() {
    let mut tree = MemTree::default();
    tree.bundle("/Apps", "a-good.app", manifest("os.tairix.good"))
        .bundle("/Apps", "b-garbage.app", b"not a manifest".to_vec())
        .bundle(
            "/Apps",
            "c-huge.app",
            alloc::vec![0u8; APPINFO_WIRE_MAX + 1],
        )
        .manifestless("/Apps", "d-bare.app")
        .bundle("/Apps", "e-refused.app", manifest("os.tairix.refused"));

    let mut seen = Vec::new();
    let scan = walk(&tree, &["/Apps"], |bundle: Bundle<'_>| {
        seen.push(bundle.path.to_string());
        if bundle.path.ends_with("e-refused.app") {
            Verdict::Refused
        } else {
            Verdict::Accepted
        }
    })
    .expect("one broken bundle costs only itself");

    assert_eq!(
        seen,
        [
            "/Apps/a-good.app".to_string(),
            "/Apps/e-refused.app".to_string(),
        ],
        "only a bundle whose manifest decoded is offered"
    );
    assert_eq!(scan.accepted, 1);
    assert_eq!(
        scan.skipped, 4,
        "garbage, over-long, manifestless, and visitor-refused all count"
    );
}

#[test]
fn a_manifest_one_byte_past_the_ceiling_is_refused_rather_than_decoded() {
    let mut at_ceiling = manifest("os.tairix.pad");
    at_ceiling.resize(APPINFO_WIRE_MAX, 0);
    assert!(crate::decode_manifest(&at_ceiling).is_some());
    at_ceiling.push(0);
    assert!(crate::decode_manifest(&at_ceiling).is_none());
}

#[test]
fn the_walk_stops_descending_at_the_depth_bound() {
    let mut tree = MemTree::default();
    // One plain directory per level, with a bundle at every level, so the
    // deepest reachable bundle names the bound rather than the fixture.
    let mut dir = String::from("/Apps");
    for level in 0..=MAX_WALK_DEPTH {
        tree.bundle(
            &dir,
            &alloc::format!("level{level}.app"),
            manifest("os.tairix.level"),
        );
        tree.entry(&dir, "deeper", true);
        dir.push_str("/deeper");
    }

    let (seen, _) = visited(&tree, &["/Apps"]);
    let deepest: Vec<&str> = seen.iter().map(|(path, _)| path.as_str()).collect();
    assert_eq!(
        deepest.len(),
        MAX_WALK_DEPTH,
        "the root's own level plus {} descents",
        MAX_WALK_DEPTH - 1
    );
    assert!(deepest.contains(&"/Apps/level0.app"));
    assert!(
        !deepest
            .iter()
            .any(|path| path.ends_with(&alloc::format!("level{MAX_WALK_DEPTH}.app"))),
        "a tree deeper than the bound is not descended into"
    );
}

#[test]
fn a_tree_past_the_entry_ceiling_fails_the_whole_scan_closed() {
    let mut tree = MemTree::default();
    for index in 0..=MAX_WALK_ENTRIES {
        tree.entry("/Apps", &alloc::format!("file{index}"), false);
    }
    assert_eq!(
        walk(&tree, &["/Apps"], |_| Verdict::Accepted),
        Err(WalkError::TreeTooLarge),
        "an unbelievable tree yields nothing, never a partial answer"
    );

    // One entry under the ceiling still completes.
    let mut inside = MemTree::default();
    for index in 0..MAX_WALK_ENTRIES {
        inside.entry("/Apps", &alloc::format!("file{index}"), false);
    }
    assert!(walk(&inside, &["/Apps"], |_| Verdict::Accepted).is_ok());
}

#[test]
fn listings_are_consumed_in_sorted_order_so_a_scan_is_deterministic() {
    let mut tree = MemTree::default();
    tree.bundle("/Apps", "zebra.app", manifest("os.tairix.zebra"))
        .bundle("/Apps", "apple.app", manifest("os.tairix.apple"))
        .bundle("/Apps", "mango.app", manifest("os.tairix.mango"));

    let (seen, _) = visited(&tree, &["/Apps"]);
    assert_eq!(
        seen.iter()
            .map(|(path, _)| path.as_str())
            .collect::<Vec<_>>(),
        ["/Apps/apple.app", "/Apps/mango.app", "/Apps/zebra.app"]
    );
}

#[test]
fn the_visitor_sees_the_whole_manifest_beside_its_decoded_header() {
    let bytes = manifest("os.tairix.example");
    let mut tree = MemTree::default();
    tree.bundle("/Apps", "Example.app", bytes.clone());

    let mut saw = None;
    walk(&tree, &["/Apps"], |bundle: Bundle<'_>| {
        saw = Some((
            bundle.header.bundle_id().to_string(),
            bundle.manifest.to_vec(),
        ));
        Verdict::Accepted
    })
    .expect("the walk completes");
    assert_eq!(saw, Some(("os.tairix.example".to_string(), bytes)));
}
