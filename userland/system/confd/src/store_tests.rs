//! Store tests: where a store lives, and who may be served out of it.

use alloc::string::{String, ToString};
use alloc::vec::Vec;

use tairix_abi::appdata_ipc::ConfigScope;
use tairix_abi::appinfo::{PublisherId, PUBLISHER_ID_LEN};
use tairix_abi::{AppIdentity, Errno};
use tairix_appconf::{Document, MAX_DOCUMENT_LEN};
use tairix_users::CONFD_UID;

use super::{
    published_document, AppStore, RootCache, StoreError, OWNER_FILE, PUBLIC_FILE, SETTINGS_FILE,
};
use crate::owner::OwnerPin;
use crate::testfs::{TestFs, ACCOUNT_UID, HOME};
use crate::Storage as _;

/// A publisher identity distinguishable from any other in these tests.
pub fn publisher(tag: u8) -> PublisherId {
    PublisherId::from_raw([tag; PUBLISHER_ID_LEN])
}

/// The app identity `os.tairix.terminal` published by `publisher(tag)`.
pub fn identity(tag: u8) -> AppIdentity {
    AppIdentity::new("os.tairix.terminal", publisher(tag)).expect("a well-formed identity")
}

/// Open a store through a fresh cache — every test here resolves exactly one
/// account, so a per-call cache is the same answer as a shared one.
fn open(
    fs: &mut TestFs,
    uid: u32,
    identity: &AppIdentity,
    create: bool,
) -> Result<AppStore, StoreError> {
    AppStore::open(fs, &mut RootCache::new(), uid, identity, create)
}

/// The store directory path a well-provisioned home puts that app's data in.
fn store_dir() -> String {
    alloc::format!("{HOME}/Settings/Apps/os.tairix.terminal")
}

#[test]
fn a_read_of_an_app_with_no_store_creates_nothing() {
    let mut fs = TestFs::provisioned();
    let store = open(&mut fs, ACCOUNT_UID, &identity(1), false).expect("opens");
    assert!(!store.is_pinned(), "a first launch has written nothing yet");
    assert!(
        !fs.exists(&store_dir()),
        "and reading must not have created one"
    );
    assert!(
        store
            .document(&mut fs, ConfigScope::Private)
            .expect("reads")
            .settings()
            .next()
            .is_none(),
        "an unpinned store has no user layer at all"
    );
}

#[test]
fn a_publish_creates_the_store_and_pins_its_publisher() {
    let mut fs = TestFs::provisioned();
    let store =
        open(&mut fs, ACCOUNT_UID, &identity(1), true).expect("a publish creates the store");
    assert!(store.is_pinned());

    let pin = fs
        .read(&alloc::format!("{}/{OWNER_FILE}", store_dir()))
        .expect("the pin is written");
    assert_eq!(
        OwnerPin::decode(&pin).map(|pin| pin.publisher()),
        Some(publisher(1))
    );

    let mut document = store
        .document(&mut fs, ConfigScope::Private)
        .expect("reads");
    document.set("font.size", "14").expect("a legal setting");
    store
        .publish(&mut fs, ConfigScope::Private, &document)
        .expect("publishes");
    assert_eq!(
        fs.read_text(&alloc::format!("{}/{SETTINGS_FILE}", store_dir())),
        Some("font.size = 14\n".to_string())
    );
}

#[test]
fn a_second_publisher_claiming_the_same_identifier_is_refused() {
    // The whole point of pinning the *publisher* rather than the build key: a
    // release re-signed with a fresh signing key opens the same store, while
    // a different developer claiming the identifier does not.
    let mut fs = TestFs::provisioned();
    open(&mut fs, ACCOUNT_UID, &identity(1), true).expect("the first publisher creates");
    assert_eq!(
        open(&mut fs, ACCOUNT_UID, &identity(2), true).err(),
        Some(StoreError::PublisherMismatch)
    );
    assert_eq!(
        StoreError::PublisherMismatch.errno(),
        Errno::PermissionDenied
    );
    // The same publisher reaches it again, whatever signed this build.
    assert!(open(&mut fs, ACCOUNT_UID, &identity(1), false)
        .expect("opens")
        .is_pinned());
}

#[test]
fn a_malformed_pin_attests_nothing_and_refuses_the_store() {
    let mut fs = TestFs::provisioned();
    open(&mut fs, ACCOUNT_UID, &identity(1), true).expect("creates");
    fs.put(&alloc::format!("{}/{OWNER_FILE}", store_dir()), b"junk");
    assert_eq!(
        open(&mut fs, ACCOUNT_UID, &identity(1), false).err(),
        Some(StoreError::PinMalformed)
    );
}

#[test]
fn an_interrupted_create_is_finished_rather_than_wedged() {
    // A store directory with no pin is the tail of a create that died between
    // the two steps. Refusing it forever would strand the app's settings.
    let mut fs = TestFs::provisioned();
    fs.mkdir(&store_dir(), 0o700).expect("the directory alone");
    assert!(open(&mut fs, ACCOUNT_UID, &identity(1), true)
        .expect("finishes the create")
        .is_pinned());
    assert!(fs.exists(&alloc::format!("{}/{OWNER_FILE}", store_dir())));
}

#[test]
fn a_store_root_the_service_does_not_own_is_refused() {
    // The gated root's *parent* is writable by the account, so an application
    // can plant a directory of that name. The capability gate does not catch
    // it — this service holds the capability either way — so the ownership
    // check is what stops forged settings being served.
    let mut fs = TestFs::provisioned();
    fs.set_owner(&alloc::format!("{HOME}/Settings/Apps"), ACCOUNT_UID);
    assert_eq!(
        open(&mut fs, ACCOUNT_UID, &identity(1), false).err(),
        Some(StoreError::RootNotOwned)
    );
    assert_eq!(StoreError::RootNotOwned.errno(), Errno::PermissionDenied);
}

#[test]
fn a_missing_store_root_is_refused_rather_than_created() {
    // A home with no gated root was never provisioned by the OS; creating one
    // here would be this service manufacturing the very gate it checks.
    let mut fs = TestFs::provisioned();
    fs.remove(&alloc::format!("{HOME}/Settings/Apps"));
    assert_eq!(
        open(&mut fs, ACCOUNT_UID, &identity(1), true).err(),
        Some(StoreError::RootNotOwned)
    );
}

#[test]
fn a_uid_with_no_home_has_no_store() {
    let mut fs = TestFs::provisioned();
    assert_eq!(
        open(&mut fs, ACCOUNT_UID + 1, &identity(1), false).err(),
        Some(StoreError::NoHome)
    );
    assert_eq!(StoreError::NoHome.errno(), Errno::NotFound);
}

#[test]
fn the_home_is_the_one_the_calling_uid_owns() {
    // Two accounts, two homes. Each uid resolves to its own — the resolution
    // reads the owning uid off the volume, so it needs no reach into the
    // credential database and cannot be steered by a caller.
    let mut fs = TestFs::provisioned();
    fs.add_home("/Users/bob", ACCOUNT_UID + 1);
    open(&mut fs, ACCOUNT_UID, &identity(1), true).expect("ada's store");
    open(&mut fs, ACCOUNT_UID + 1, &identity(1), true).expect("bob's store");
    assert!(fs.exists(&alloc::format!("{}/{OWNER_FILE}", store_dir())));
    assert!(fs.exists("/Users/bob/Settings/Apps/os.tairix.terminal/.owner"));
}

#[test]
fn an_unreachable_volume_is_distinct_from_an_absent_store() {
    // Before the encrypted root is unlocked the store is not missing, it is
    // unreachable — and a caller must be told which, not handed a default.
    let mut fs = TestFs::provisioned();
    fs.fail_all(Errno::DeviceOffline);
    assert_eq!(
        open(&mut fs, ACCOUNT_UID, &identity(1), false).err(),
        Some(StoreError::Unavailable)
    );
    assert_eq!(StoreError::Unavailable.errno(), Errno::DeviceOffline);
}

#[test]
fn a_publish_replaces_the_document_atomically() {
    let mut fs = TestFs::provisioned();
    let store = open(&mut fs, ACCOUNT_UID, &identity(1), true).expect("created");
    let mut document = store
        .document(&mut fs, ConfigScope::Private)
        .expect("reads");
    document.set("scheme", "dark").expect("legal");
    store
        .publish(&mut fs, ConfigScope::Private, &document)
        .expect("publishes");

    // The temporary is gone: the rename consumed it, so no half-written
    // document is ever left where a reader would find one.
    assert!(!fs.exists(&alloc::format!("{}/settings.conf.new", store_dir())));
    assert_eq!(
        store
            .document(&mut fs, ConfigScope::Private)
            .expect("reads back")
            .get("scheme"),
        Some("dark")
    );
}

#[test]
fn a_publish_preserves_what_a_human_wrote() {
    let mut fs = TestFs::provisioned();
    let store = open(&mut fs, ACCOUNT_UID, &identity(1), true).expect("created");
    let hand_edited = "# ada's own note\n\nscheme = light\nnot a setting at all\n";
    fs.put(
        &alloc::format!("{}/{SETTINGS_FILE}", store_dir()),
        hand_edited.as_bytes(),
    );

    let mut document = store
        .document(&mut fs, ConfigScope::Private)
        .expect("reads");
    document.set("scheme", "dark").expect("legal");
    store
        .publish(&mut fs, ConfigScope::Private, &document)
        .expect("publishes");

    assert_eq!(
        fs.read_text(&alloc::format!("{}/{SETTINGS_FILE}", store_dir())),
        Some("# ada's own note\n\nscheme = dark\nnot a setting at all\n".to_string())
    );
}

#[test]
fn a_document_outside_the_formats_bounds_is_refused_whole() {
    let mut fs = TestFs::provisioned();
    let store = open(&mut fs, ACCOUNT_UID, &identity(1), true).expect("created");
    let oversize = "x".repeat(MAX_DOCUMENT_LEN + 1);
    fs.put(
        &alloc::format!("{}/{SETTINGS_FILE}", store_dir()),
        oversize.as_bytes(),
    );
    assert_eq!(
        store.document(&mut fs, ConfigScope::Private).err(),
        Some(StoreError::DocumentRefused)
    );

    // Non-UTF-8 bytes are equally not a document.
    fs.put(
        &alloc::format!("{}/{SETTINGS_FILE}", store_dir()),
        &[0xFF, 0xFE],
    );
    assert_eq!(
        store.document(&mut fs, ConfigScope::Private).err(),
        Some(StoreError::DocumentRefused)
    );
}

#[test]
fn the_policy_layer_is_read_from_the_read_only_system_tree() {
    let mut fs = TestFs::provisioned();
    fs.put(
        "/System/Settings/os.tairix.terminal/settings.conf",
        b"font.size = 18\n",
    );
    let store = open(&mut fs, ACCOUNT_UID, &identity(1), true).expect("created");
    assert_eq!(
        store
            .policy_document(&mut fs)
            .expect("reads")
            .get("font.size"),
        Some("18")
    );
    // A machine that ships no policy simply has an empty layer.
    let mut bare = TestFs::provisioned();
    let store = open(&mut bare, ACCOUNT_UID, &identity(1), true).expect("created");
    assert!(store
        .policy_document(&mut bare)
        .expect("reads")
        .settings()
        .next()
        .is_none());
}

#[test]
fn the_merged_document_is_canonical_and_the_user_layer_wins() {
    // Two layers become one document, so the result must name each key once —
    // a hand-edit that appends a line, and a policy default the user has
    // overridden, both collapse to the value a reader would have taken.
    let mut fs = TestFs::provisioned();
    fs.put(
        "/System/Settings/os.tairix.terminal/settings.conf",
        b"font.size = 18\nscheme = corporate\npolicy.only = 1\n",
    );
    let store = open(&mut fs, ACCOUNT_UID, &identity(1), true).expect("created");
    fs.put(
        &alloc::format!("{HOME}/Settings/Apps/os.tairix.terminal/{SETTINGS_FILE}"),
        b"# ada's note\nscheme = dark\nscheme = darker\n",
    );

    let merged = store
        .merged_document(&mut fs, ConfigScope::Private)
        .expect("merges");
    let keys: Vec<&str> = merged.settings().map(|setting| setting.key).collect();
    assert_eq!(keys, ["font.size", "scheme", "policy.only"]);
    assert_eq!(merged.get("scheme"), Some("darker"), "the user layer wins");
    assert_eq!(merged.get("font.size"), Some("18"));
    assert_eq!(
        merged.render(),
        "font.size = 18\nscheme = darker\npolicy.only = 1\n",
        "the served view is canonical; the user's own file keeps its comments"
    );
    assert_eq!(
        fs.read_text(&alloc::format!(
            "{HOME}/Settings/Apps/os.tairix.terminal/{SETTINGS_FILE}"
        ))
        .as_deref(),
        Some("# ada's note\nscheme = dark\nscheme = darker\n"),
        "reading must not rewrite the stored document"
    );
}

#[test]
fn the_service_uid_is_the_one_the_provisioners_stamp() {
    // The store's authorisation check compares against this uid; a drift from
    // the value the three home provisioners stamp would make every store
    // unreachable, so the comparison is pinned to the shared definition.
    assert_eq!(super::CONFD_UID_RAW, CONFD_UID.0);
}

#[test]
fn the_home_is_recovered_exactly_from_a_resolved_root() {
    // The cache remembers the root and re-checks the *home*'s owner, so the
    // decomposition must be the exact inverse of the composition — one place
    // where a path is taken apart rather than built up.
    let mut fs = TestFs::provisioned();
    let mut roots = RootCache::new();
    let root = roots.resolve(&mut fs, ACCOUNT_UID).expect("resolves");
    assert_eq!(root, alloc::format!("{HOME}/Settings/Apps"));
    assert_eq!(super::home_of_root(&root), HOME);
    // And a path with nothing to strip degrades to itself rather than
    // indexing past the front.
    assert_eq!(super::home_of_root("Apps"), "Apps");
    assert_eq!(super::home_of_root(""), "");
}

#[test]
fn a_bundle_identifier_cannot_reach_outside_the_store_root() {
    // The only caller-influenced component of a store path is the bundle
    // identifier, and it arrives already inside the identifier grammar — so
    // a traversal, a separator, or a hidden entry is not constructible at
    // all, rather than being filtered here.
    for hostile in [
        "..",
        ".",
        "a/b",
        "/etc",
        "..%2f",
        "A",
        "a..b/../c",
        ".hidden",
        "a b",
    ] {
        assert!(
            AppIdentity::new(hostile, publisher(1)).is_err(),
            "`{hostile}` must never be an app identity"
        );
    }
}

#[test]
fn one_unreadable_home_does_not_deny_every_account() {
    // The scan walks other accounts' homes on the way to the caller's. A
    // foreign home this service may not stat is simply not the one it wants;
    // failing the whole resolution would let one broken home lock every
    // account out of its own settings.
    let mut fs = TestFs::provisioned();
    fs.add_home("/Users/bob", ACCOUNT_UID + 1);
    fs.hide("/Users/bob");
    assert!(open(&mut fs, ACCOUNT_UID, &identity(1), true).is_ok());
}

/// A second app identity, so a foreign read has someone to read.
fn other(tag: u8) -> AppIdentity {
    AppIdentity::new("org.pty.widgets", publisher(tag)).expect("a well-formed identity")
}

/// Publish `text` as `identity`'s document in `scope`, creating the store.
fn seed(fs: &mut TestFs, uid: u32, identity: &AppIdentity, scope: ConfigScope, text: &str) {
    let store = open(fs, uid, identity, true).expect("creates");
    let document = Document::parse(text).expect("a legal fixture");
    store.publish(fs, scope, &document).expect("publishes");
}

#[test]
fn the_two_scopes_are_separate_documents() {
    // The whole point of a published scope: an app's private settings and what
    // it says about itself are different files, and a write to one cannot
    // touch the other.
    let mut fs = TestFs::provisioned();
    seed(
        &mut fs,
        ACCOUNT_UID,
        &identity(1),
        ConfigScope::Private,
        "scheme = dark\n",
    );
    seed(
        &mut fs,
        ACCOUNT_UID,
        &identity(1),
        ConfigScope::Public,
        "font.family = berkeley\n",
    );

    assert_eq!(
        fs.read_text(&alloc::format!("{}/{SETTINGS_FILE}", store_dir())),
        Some("scheme = dark\n".to_string())
    );
    assert_eq!(
        fs.read_text(&alloc::format!("{}/{PUBLIC_FILE}", store_dir())),
        Some("font.family = berkeley\n".to_string())
    );

    let store = open(&mut fs, ACCOUNT_UID, &identity(1), false).expect("opens");
    let private = store
        .document(&mut fs, ConfigScope::Private)
        .expect("reads private");
    let public = store
        .document(&mut fs, ConfigScope::Public)
        .expect("reads public");
    assert_eq!(private.get("scheme"), Some("dark"));
    assert_eq!(private.get("font.family"), None);
    assert_eq!(public.get("font.family"), Some("berkeley"));
    assert_eq!(public.get("scheme"), None);
}

#[test]
fn each_scope_publishes_through_its_own_sibling_temporary() {
    // The temporary is derived from the live name, so the two scopes can never
    // contend for it and a crash in one publish cannot corrupt the other's
    // document.
    let mut fs = TestFs::provisioned();
    seed(
        &mut fs,
        ACCOUNT_UID,
        &identity(1),
        ConfigScope::Public,
        "a = 1\n",
    );
    assert!(!fs.exists(&alloc::format!("{}/{PUBLIC_FILE}.new", store_dir())));
    assert!(!fs.exists(&alloc::format!("{}/{SETTINGS_FILE}.new", store_dir())));
    assert!(fs.exists(&alloc::format!("{}/{PUBLIC_FILE}", store_dir())));
    assert!(!fs.exists(&alloc::format!("{}/{SETTINGS_FILE}", store_dir())));
}

#[test]
fn the_published_scope_has_no_layer_beneath_it() {
    // A machine-wide layer would let an administrator make an application
    // appear to publish something it never published, and a bundle-shipped one
    // is not even reachable on the foreign path. So the published scope is
    // exactly the app's own document — while the private scope still layers.
    let mut fs = TestFs::provisioned();
    fs.put(
        "/System/Settings/os.tairix.terminal/settings.conf",
        b"font.size = 18\n",
    );
    fs.put(
        "/System/Settings/os.tairix.terminal/public.conf",
        b"font.family = policy\n",
    );
    seed(
        &mut fs,
        ACCOUNT_UID,
        &identity(1),
        ConfigScope::Public,
        "font.family = berkeley\n",
    );
    let store = open(&mut fs, ACCOUNT_UID, &identity(1), false).expect("opens");

    let public = store
        .merged_document(&mut fs, ConfigScope::Public)
        .expect("merges");
    assert_eq!(public.get("font.family"), Some("berkeley"));
    assert_eq!(
        public.settings().count(),
        1,
        "nothing but what the application itself published"
    );

    let private = store
        .merged_document(&mut fs, ConfigScope::Private)
        .expect("merges");
    assert_eq!(
        private.get("font.size"),
        Some("18"),
        "the private scope still reads through the policy layer"
    );
}

#[test]
fn an_unpublished_scope_reads_empty_without_touching_the_other() {
    let mut fs = TestFs::provisioned();
    seed(
        &mut fs,
        ACCOUNT_UID,
        &identity(1),
        ConfigScope::Private,
        "scheme = dark\n",
    );
    let store = open(&mut fs, ACCOUNT_UID, &identity(1), false).expect("opens");
    assert!(store
        .document(&mut fs, ConfigScope::Public)
        .expect("reads")
        .settings()
        .next()
        .is_none());
    assert!(!fs.exists(&alloc::format!("{}/{PUBLIC_FILE}", store_dir())));
}

#[test]
fn a_foreign_read_answers_what_that_application_published() {
    let mut fs = TestFs::provisioned();
    seed(
        &mut fs,
        ACCOUNT_UID,
        &other(2),
        ConfigScope::Public,
        "widget.count = 7\n",
    );
    let published = published_document(
        &mut fs,
        &mut RootCache::new(),
        ACCOUNT_UID,
        "org.pty.widgets",
    )
    .expect("reads");
    assert_eq!(published.get("widget.count"), Some("7"));
}

#[test]
fn a_foreign_read_cannot_see_the_private_scope() {
    // There is no request shape that asks for another app's private document,
    // and there is no code path either: the foreign read composes the
    // published name and nothing else.
    let mut fs = TestFs::provisioned();
    seed(
        &mut fs,
        ACCOUNT_UID,
        &other(2),
        ConfigScope::Private,
        "imap.user = ada\n",
    );
    let published = published_document(
        &mut fs,
        &mut RootCache::new(),
        ACCOUNT_UID,
        "org.pty.widgets",
    )
    .expect("reads");
    assert_eq!(published.get("imap.user"), None);
    assert!(published.settings().next().is_none());
}

#[test]
fn a_foreign_read_of_an_application_with_no_store_answers_the_empty_document() {
    // Indistinguishable from an application that publishes nothing, which is
    // what stops the read being an oracle for which applications an account
    // has ever run.
    let mut fs = TestFs::provisioned();
    seed(
        &mut fs,
        ACCOUNT_UID,
        &other(2),
        ConfigScope::Public,
        "widget.count = 7\n",
    );
    for absent in ["com.example.never-run", "os.tairix.terminal"] {
        let published =
            published_document(&mut fs, &mut RootCache::new(), ACCOUNT_UID, absent).expect("reads");
        assert!(
            published.settings().next().is_none(),
            "`{absent}` publishes nothing here"
        );
    }
}

#[test]
fn a_foreign_read_of_a_store_that_attests_nothing_is_the_targets_defect() {
    // A malformed pin or an out-of-bounds document belongs to the store being
    // read, not to the reader — so it is classified as the target's defect and
    // the dispatcher answers empty rather than reporting another app's state.
    let mut fs = TestFs::provisioned();
    seed(
        &mut fs,
        ACCOUNT_UID,
        &other(2),
        ConfigScope::Public,
        "widget.count = 7\n",
    );
    let dir = alloc::format!("{HOME}/Settings/Apps/org.pty.widgets");
    fs.put(&alloc::format!("{dir}/{OWNER_FILE}"), b"junk");
    assert_eq!(
        published_document(
            &mut fs,
            &mut RootCache::new(),
            ACCOUNT_UID,
            "org.pty.widgets"
        )
        .err(),
        Some(StoreError::PinMalformed)
    );

    fs.put(
        &alloc::format!("{dir}/{OWNER_FILE}"),
        &OwnerPin::new(publisher(2)).encode(),
    );
    fs.put(
        &alloc::format!("{dir}/{PUBLIC_FILE}"),
        "x".repeat(MAX_DOCUMENT_LEN + 1).as_bytes(),
    );
    assert_eq!(
        published_document(
            &mut fs,
            &mut RootCache::new(),
            ACCOUNT_UID,
            "org.pty.widgets"
        )
        .err(),
        Some(StoreError::DocumentRefused)
    );
}

#[test]
fn only_the_targets_own_defects_are_answered_as_an_absence() {
    // The classification is the whole of the "a foreign read is not an oracle"
    // rule, so it is pinned here rather than left to the call site: a defect of
    // the caller's account or of the volume is the caller's to hear about.
    for err in [StoreError::PinMalformed, StoreError::DocumentRefused] {
        assert!(err.is_target_defect(), "{err:?} belongs to the target");
    }
    for err in [
        StoreError::NoAppIdentity,
        StoreError::NoHome,
        StoreError::RootNotOwned,
        StoreError::PublisherMismatch,
        StoreError::Unavailable,
    ] {
        assert!(!err.is_target_defect(), "{err:?} is the caller's own");
    }
}

#[test]
fn a_foreign_read_never_crosses_an_account() {
    // Stores are per-user: a read answers what *this* account's copy of that
    // application published, so one user's published data is not another's.
    let mut fs = TestFs::provisioned();
    fs.add_home("/Users/bob", ACCOUNT_UID + 1);
    seed(
        &mut fs,
        ACCOUNT_UID,
        &other(2),
        ConfigScope::Public,
        "owner = ada\n",
    );
    seed(
        &mut fs,
        ACCOUNT_UID + 1,
        &other(2),
        ConfigScope::Public,
        "owner = bob\n",
    );
    let mut roots = RootCache::new();
    assert_eq!(
        published_document(&mut fs, &mut roots, ACCOUNT_UID, "org.pty.widgets")
            .expect("reads")
            .get("owner"),
        Some("ada")
    );
    assert_eq!(
        published_document(&mut fs, &mut roots, ACCOUNT_UID + 1, "org.pty.widgets")
            .expect("reads")
            .get("owner"),
        Some("bob")
    );
}

#[test]
fn a_foreign_read_reports_the_callers_own_account_failures() {
    // A caller with no home, or one whose gated root is not the service's, has
    // no store to read *from* — that is the caller's own state and is reported
    // as itself rather than silently answering empty.
    let mut fs = TestFs::provisioned();
    assert_eq!(
        published_document(
            &mut fs,
            &mut RootCache::new(),
            ACCOUNT_UID + 1,
            "org.pty.widgets"
        )
        .err(),
        Some(StoreError::NoHome)
    );
    fs.set_owner(&alloc::format!("{HOME}/Settings/Apps"), ACCOUNT_UID);
    assert_eq!(
        published_document(
            &mut fs,
            &mut RootCache::new(),
            ACCOUNT_UID,
            "org.pty.widgets"
        )
        .err(),
        Some(StoreError::RootNotOwned)
    );
}
