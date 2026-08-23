//! Unit tests for the published-scope overlay store.
//!
//! They drive the shared fake app-data service, which speaks the real
//! `appdata-v1` codec, so what is exercised here is the wire the service
//! actually answers rather than a private idea of it.

use alloc::string::String;

use tairix_abi::Errno;
use tairix_appconf::Document;
use tairix_appdata::fake::FakeService;

use super::AppDataStore;
use crate::Store;

/// The command word this application's bundle is installed under. The
/// published scope has no bundle-shipped layer, so it selects nothing here.
const OWN_WORD: &str = "applib";

/// A document holding one declared entry.
fn declared() -> Document {
    let mut document = Document::new();
    document
        .set("os.tairix.editor.name", "Editor")
        .expect("key");
    document
        .set("os.tairix.editor.bundle", "/System/Applications/editor.app")
        .expect("key");
    document
}

#[test]
fn an_empty_scope_reads_as_no_store_at_all() {
    // Publishing nothing and never having run are the same answer, exactly
    // as they are to any other reader of a published scope.
    let store = AppDataStore::new(FakeService::for_word(OWN_WORD));
    assert!(store.read().expect("reads").is_none());
}

#[test]
fn a_published_overlay_reads_back_verbatim() {
    let store = AppDataStore::new(FakeService::for_word(OWN_WORD));
    store.write(&declared()).expect("publishes");
    let read = store.read().expect("reads").expect("holds a document");
    assert_eq!(read.render(), declared().render());
}

#[test]
fn a_publish_replaces_the_scope_rather_than_adding_to_it() {
    // A removed entry must leave nothing behind: a launcher row nobody can
    // account for is exactly what this store exists to prevent.
    let store = AppDataStore::new(FakeService::for_word(OWN_WORD));
    store.write(&declared()).expect("publishes");
    let mut replacement = Document::new();
    replacement.set("chess.hidden", "true").expect("key");
    store.write(&replacement).expect("publishes");

    let read = store.read().expect("reads").expect("holds a document");
    assert_eq!(read.get("chess.hidden"), Some("true"));
    assert_eq!(
        read.get("os.tairix.editor.name"),
        None,
        "the replaced entry left nothing behind"
    );
    assert_eq!(read.settings().count(), 1);
}

#[test]
fn a_publish_lands_in_the_published_scope_and_nowhere_else() {
    // The private scope is not what the desktop session reads, so an overlay
    // written there would be an overlay nothing could see.
    let mut host = FakeService::for_word(OWN_WORD);
    {
        let store = AppDataStore::new(&mut host);
        store.write(&declared()).expect("publishes");
    }
    assert_eq!(
        host.published().get("os.tairix.editor.name"),
        Some("Editor")
    );
    assert_eq!(host.committed().settings().count(), 0);
}

#[test]
fn a_store_the_service_cannot_serve_is_a_refusal_not_an_empty_library() {
    // An unreachable store must never read as "this account has no overlay":
    // the tool would then publish an edit over settings it never saw.
    let host = FakeService::for_word(OWN_WORD);
    host.refusal().set(Some(Errno::DeviceOffline));
    let store = AppDataStore::new(host);
    // `Document` implements no `Debug` and no `PartialEq` — it may hold a
    // sealed scope's plaintext — so the refusal is matched rather than
    // compared.
    assert!(matches!(store.read(), Err(Errno::DeviceOffline)));
    assert_eq!(store.write(&declared()), Err(Errno::DeviceOffline));
}

#[test]
fn a_refused_publish_changes_nothing() {
    let mut host = FakeService::for_word(OWN_WORD);
    let refusal = host.refusal();
    {
        let store = AppDataStore::new(&mut host);
        store.write(&declared()).expect("publishes");
        refusal.set(Some(Errno::NoSpace));
        let mut replacement = Document::new();
        replacement.set("chess.hidden", "true").expect("key");
        assert_eq!(store.write(&replacement), Err(Errno::NoSpace));
        refusal.set(None);
    }
    assert_eq!(
        host.published().get("os.tairix.editor.name"),
        Some("Editor")
    );
    assert_eq!(host.published().get("chess.hidden"), None);
}

#[test]
fn the_overlays_publisher_names_this_very_bundle() {
    // Two principals must agree on the identifier: this program, which is
    // what the kernel attests when it writes, and the desktop session, which
    // hands `LIBRARY_PUBLISHER` to a foreign read. They cannot be one
    // definition — one is a signed manifest, the other a Rust constant — so
    // this is what stops a bundle rename turning the desktop's overlay read
    // into a silent empty catalog.
    let manifest = include_str!("../AppInfo.toml");
    let declared = manifest
        .lines()
        .find_map(|line| line.strip_prefix("id = \""))
        .and_then(|rest| rest.strip_suffix('"'))
        .expect("the manifest source declares an id");
    assert_eq!(declared, tairix_proglib::LIBRARY_PUBLISHER);
}

#[test]
fn the_store_spells_no_path_of_its_own() {
    // A pin on the property the whole migration exists for: nothing in this
    // module names a store path, a user, or a bundle identifier. The service
    // derives all three from the identity the kernel attested.
    let source = include_str!("store.rs");
    for spelled in ["/Users", "Settings/", "library.conf", "os.tairix.applib"] {
        let quoted = String::from("\"") + spelled;
        assert!(
            !source.contains(&quoted),
            "the overlay store must not spell {spelled}"
        );
    }
}
