//! End-to-end dispatcher tests: a framed request plus a kernel-attested origin
//! in, a decodable reply out — and the isolation properties the service exists
//! to provide.

use alloc::string::String;
use alloc::vec::Vec;

use tairix_abi::appdata_ipc::{
    decode_blob_list_reply, decode_document_reply, decode_grant_reply, decode_quota_reply,
    AppDataRequest, BlobMode, ConfigDocument, ConfigScope, APPDATA_BLOB_MAX_BYTES,
    APPDATA_BLOB_MAX_COUNT, APPDATA_DOCUMENT_MAX, APPDATA_MAX_REPLY, APPDATA_MAX_REQUEST,
    APPDATA_VALUE_MAX,
};
use tairix_abi::origin::{CapabilitySummary, TrustDomain, ORIGIN_CONSOLE_NONE, PROC_ID_LEN};
use tairix_abi::reply::decode_status_reply;
use tairix_abi::{AppIdentity, Errno, Origin, ProcId};
use tairix_appconf::Document;
use tairix_log::DiscardSink;

use super::{AppData, MAX_PENDING_EDITS, STAGING_IDLE_NS};
use crate::store::tests::{identity, publisher};
use crate::testfs::{TestFs, ACCOUNT_UID, HOME};
use crate::vault::tests::CountingEntropy;
use crate::Storage as _;

/// A distinct process instance. Never reused, exactly as the kernel's own
/// identifiers are not.
fn proc_id(tag: u8) -> ProcId {
    ProcId::from_bytes(&[tag; PROC_ID_LEN]).expect("a full-width identifier")
}

/// An attested origin: account `uid`, process instance `tag`, running the app
/// `identity`.
fn origin(uid: u32, tag: u8, identity: Option<AppIdentity>) -> Origin {
    let base = Origin::new(
        TrustDomain::User,
        uid,
        uid,
        u64::from(tag),
        proc_id(tag),
        CapabilitySummary::EMPTY,
        ORIGIN_CONSOLE_NONE,
    );
    match identity {
        Some(app) => base.with_app(app),
        None => base,
    }
}

/// A dispatcher over a freshly provisioned volume, drawing the sealed scope's
/// key material from a deterministic generator.
fn service() -> (AppData<DiscardSink, CountingEntropy>, TestFs) {
    (
        AppData::new(DiscardSink, CountingEntropy::new(1)),
        TestFs::provisioned(),
    )
}

/// Serve `request` and hand back the raw reply frame.
fn call(
    service: &mut AppData<DiscardSink, CountingEntropy>,
    fs: &mut TestFs,
    origin: &Origin,
    request: &AppDataRequest<'_>,
) -> Vec<u8> {
    call_at(service, fs, origin, 0, request)
}

/// As [`call`], at monotonic instant `now_ns`.
fn call_at(
    service: &mut AppData<DiscardSink, CountingEntropy>,
    fs: &mut TestFs,
    origin: &Origin,
    now_ns: u64,
    request: &AppDataRequest<'_>,
) -> Vec<u8> {
    let mut frame = [0u8; APPDATA_MAX_REQUEST];
    let len = request.encode(&mut frame).expect("a legal request");
    let mut reply = alloc::vec![0u8; APPDATA_MAX_REPLY];
    let reply_len = service.serve(fs, origin, now_ns, &frame[..len], &mut reply);
    reply.truncate(reply_len);
    reply
}

/// Serve a `ConfigSet` in `scope` and assert it was accepted.
fn set_in(
    service: &mut AppData<DiscardSink, CountingEntropy>,
    fs: &mut TestFs,
    origin: &Origin,
    scope: ConfigScope,
    key: &str,
    value: &str,
) {
    let reply = call(
        service,
        fs,
        origin,
        &AppDataRequest::ConfigSet { scope, key, value },
    );
    assert_eq!(
        decode_status_reply(&reply),
        Ok(()),
        "set {key} in {scope:?}"
    );
}

/// Serve a `ConfigCommit` for `scope` and assert it was accepted.
fn commit_in(
    service: &mut AppData<DiscardSink, CountingEntropy>,
    fs: &mut TestFs,
    origin: &Origin,
    scope: ConfigScope,
) {
    let reply = call(service, fs, origin, &AppDataRequest::ConfigCommit { scope });
    assert_eq!(decode_status_reply(&reply), Ok(()), "commit {scope:?}");
}

/// Serve a `ConfigRead` of `scope` and parse the document it answered with.
fn read_in(
    service: &mut AppData<DiscardSink, CountingEntropy>,
    fs: &mut TestFs,
    origin: &Origin,
    scope: ConfigScope,
) -> Result<Document, Errno> {
    let capacity = u32::try_from(APPDATA_DOCUMENT_MAX).expect("fits a u32");
    let reply = call(
        service,
        fs,
        origin,
        &AppDataRequest::ConfigRead { scope, capacity },
    );
    whole(&reply)
}

/// Serve a `PublicRead` of `bundle_id` and parse the document it answered
/// with.
fn read_published(
    service: &mut AppData<DiscardSink, CountingEntropy>,
    fs: &mut TestFs,
    origin: &Origin,
    bundle_id: &str,
) -> Result<Document, Errno> {
    let capacity = u32::try_from(APPDATA_DOCUMENT_MAX).expect("fits a u32");
    let reply = call(
        service,
        fs,
        origin,
        &AppDataRequest::PublicRead {
            bundle_id,
            capacity,
        },
    );
    whole(&reply)
}

/// Serve a `VaultSet` and assert it was accepted.
fn seal(
    service: &mut AppData<DiscardSink, CountingEntropy>,
    fs: &mut TestFs,
    origin: &Origin,
    key: &str,
    value: &str,
) {
    let reply = call(
        service,
        fs,
        origin,
        &AppDataRequest::VaultSet { key, value },
    );
    assert_eq!(decode_status_reply(&reply), Ok(()), "seal {key}");
}

/// Serve a `VaultRead` and parse the document it answered with.
fn read_vault(
    service: &mut AppData<DiscardSink, CountingEntropy>,
    fs: &mut TestFs,
    origin: &Origin,
) -> Result<Document, Errno> {
    let capacity = u32::try_from(APPDATA_DOCUMENT_MAX).expect("fits a u32");
    let reply = call(service, fs, origin, &AppDataRequest::VaultRead { capacity });
    whole(&reply)
}

/// Decode a document reply that must have fitted.
fn whole(reply: &[u8]) -> Result<Document, Errno> {
    match decode_document_reply(reply)? {
        ConfigDocument::Whole(text) => Ok(Document::parse(text).expect("the daemon renders it")),
        ConfigDocument::NeedsCapacity(len) => {
            panic!("the widest capacity still needed {len} bytes")
        }
    }
}

// The private scope is what nearly every test below is about, so it has
// unsuffixed helpers: naming it at sixty call sites would bury the handful of
// tests that are *about* which scope is reached.

/// [`set_in`] on the caller's private scope.
fn set(
    service: &mut AppData<DiscardSink, CountingEntropy>,
    fs: &mut TestFs,
    origin: &Origin,
    key: &str,
    value: &str,
) {
    set_in(service, fs, origin, ConfigScope::Private, key, value);
}

/// [`commit_in`] on the caller's private scope.
fn commit(service: &mut AppData<DiscardSink, CountingEntropy>, fs: &mut TestFs, origin: &Origin) {
    commit_in(service, fs, origin, ConfigScope::Private);
}

/// [`read_in`] on the caller's private scope.
fn read(
    service: &mut AppData<DiscardSink, CountingEntropy>,
    fs: &mut TestFs,
    origin: &Origin,
) -> Result<Document, Errno> {
    read_in(service, fs, origin, ConfigScope::Private)
}

/// Read `key` out of the caller's own merged document.
///
/// The document is the unit the wire carries, so "not set" is the *client's*
/// answer about a document it holds, not a second round trip.
fn get(
    service: &mut AppData<DiscardSink, CountingEntropy>,
    fs: &mut TestFs,
    origin: &Origin,
    key: &str,
) -> Result<String, Errno> {
    read(service, fs, origin)?
        .get(key)
        .map(String::from)
        .ok_or(Errno::NotFound)
}

/// The keys the caller's own merged document carries, in document order.
fn keys(
    service: &mut AppData<DiscardSink, CountingEntropy>,
    fs: &mut TestFs,
    origin: &Origin,
) -> Vec<String> {
    match read(service, fs, origin) {
        Ok(document) => document
            .settings()
            .map(|setting| String::from(setting.key))
            .collect(),
        Err(_) => Vec::new(),
    }
}

#[test]
fn a_set_then_commit_then_get_round_trips() {
    let (mut svc, mut fs) = service();
    let ada = origin(ACCOUNT_UID, 1, Some(identity(1)));
    set(&mut svc, &mut fs, &ada, "font.size", "14");
    commit(&mut svc, &mut fs, &ada);
    assert_eq!(
        get(&mut svc, &mut fs, &ada, "font.size").as_deref(),
        Ok("14")
    );
}

#[test]
fn a_key_that_was_never_set_is_not_found() {
    let (mut svc, mut fs) = service();
    let ada = origin(ACCOUNT_UID, 1, Some(identity(1)));
    assert_eq!(get(&mut svc, &mut fs, &ada, "scheme"), Err(Errno::NotFound));
}

#[test]
fn an_app_cannot_reach_another_apps_settings() {
    // The property the whole service exists for. Two apps of the *same
    // account* — same uid, same everything the filesystem model can key on —
    // and neither sees the other's data. No request shape names a store, so
    // there is nothing to try.
    let (mut svc, mut fs) = service();
    let terminal = AppIdentity::new("os.tairix.terminal", publisher(1)).expect("well formed");
    let notes = AppIdentity::new("org.pty.notes", publisher(2)).expect("well formed");
    let one = origin(ACCOUNT_UID, 1, Some(terminal));
    let two = origin(ACCOUNT_UID, 2, Some(notes));

    set(&mut svc, &mut fs, &one, "secret.token", "terminal-only");
    commit(&mut svc, &mut fs, &one);

    assert_eq!(
        get(&mut svc, &mut fs, &two, "secret.token"),
        Err(Errno::NotFound),
        "the other app of the same user sees nothing"
    );
    assert!(
        keys(&mut svc, &mut fs, &two).is_empty(),
        "and cannot even enumerate it"
    );

    // Each app's own write lands in its own store.
    set(&mut svc, &mut fs, &two, "secret.token", "notes-only");
    commit(&mut svc, &mut fs, &two);
    assert_eq!(
        get(&mut svc, &mut fs, &one, "secret.token").as_deref(),
        Ok("terminal-only")
    );
    assert_eq!(
        get(&mut svc, &mut fs, &two, "secret.token").as_deref(),
        Ok("notes-only")
    );
}

#[test]
fn one_users_settings_are_invisible_to_another() {
    let (mut svc, mut fs) = service();
    fs.add_home("/Users/bob", ACCOUNT_UID + 1);
    let app = identity(1);
    let ada = origin(ACCOUNT_UID, 1, Some(app));
    let bob = origin(ACCOUNT_UID + 1, 2, Some(app));

    set(&mut svc, &mut fs, &ada, "scheme", "dark");
    commit(&mut svc, &mut fs, &ada);
    assert_eq!(get(&mut svc, &mut fs, &bob, "scheme"), Err(Errno::NotFound));

    set(&mut svc, &mut fs, &bob, "scheme", "light");
    commit(&mut svc, &mut fs, &bob);
    assert_eq!(
        get(&mut svc, &mut fs, &ada, "scheme").as_deref(),
        Ok("dark")
    );
    assert_eq!(
        get(&mut svc, &mut fs, &bob, "scheme").as_deref(),
        Ok("light")
    );
}

#[test]
fn a_caller_with_no_attested_app_identity_is_refused_every_operation() {
    // A kernel principal, a boot-floor program with no signed manifest, and a
    // parser-sandbox child all reach here. None of them has a store, and the
    // refusal happens before any state is touched.
    let (mut svc, mut fs) = service();
    let anon = origin(ACCOUNT_UID, 1, None);
    for request in [
        AppDataRequest::ConfigRead {
            scope: ConfigScope::Private,
            capacity: 4096,
        },
        AppDataRequest::ConfigSet {
            scope: ConfigScope::Private,
            key: "scheme",
            value: "dark",
        },
        AppDataRequest::ConfigUnset {
            scope: ConfigScope::Private,
            key: "scheme",
        },
        AppDataRequest::ConfigCommit {
            scope: ConfigScope::Private,
        },
        AppDataRequest::PublicRead {
            bundle_id: "org.pty.notes",
            capacity: 4096,
        },
        AppDataRequest::VaultRead { capacity: 4096 },
        AppDataRequest::BlobOpen {
            name: "index",
            mode: BlobMode::ReadWrite,
        },
        AppDataRequest::BlobDelete { name: "index" },
        AppDataRequest::BlobList { capacity: 4096 },
        AppDataRequest::QuotaGet {},
    ] {
        let reply = call(&mut svc, &mut fs, &anon, &request);
        assert_eq!(
            decode_status_reply(&reply),
            Err(Errno::PermissionDenied),
            "{request:?} must be refused"
        );
    }
    assert_eq!(svc.staging_sessions(), 0, "and nothing was staged");
    assert!(fs.grants().is_empty(), "and nothing was delegated");
}

#[test]
fn a_publisher_claiming_another_developers_identifier_is_refused() {
    let (mut svc, mut fs) = service();
    let honest = origin(ACCOUNT_UID, 1, Some(identity(1)));
    set(&mut svc, &mut fs, &honest, "scheme", "dark");
    commit(&mut svc, &mut fs, &honest);

    // Same bundle id, different publisher: a squatter, refused at the pin.
    let squatter = origin(ACCOUNT_UID, 2, Some(identity(2)));
    assert_eq!(
        get(&mut svc, &mut fs, &squatter, "scheme"),
        Err(Errno::PermissionDenied)
    );
    let reply = call(
        &mut svc,
        &mut fs,
        &squatter,
        &AppDataRequest::ConfigSet {
            scope: ConfigScope::Private,
            key: "scheme",
            value: "hostile",
        },
    );
    assert_eq!(decode_status_reply(&reply), Ok(()), "the set only stages");
    let reply = call(
        &mut svc,
        &mut fs,
        &squatter,
        &AppDataRequest::ConfigCommit {
            scope: ConfigScope::Private,
        },
    );
    assert_eq!(
        decode_status_reply(&reply),
        Err(Errno::PermissionDenied),
        "and the commit is refused"
    );
    assert_eq!(
        get(&mut svc, &mut fs, &honest, "scheme").as_deref(),
        Ok("dark"),
        "the real owner's data is untouched"
    );
}

#[test]
fn an_uncommitted_change_never_reaches_the_volume() {
    let (mut svc, mut fs) = service();
    let ada = origin(ACCOUNT_UID, 1, Some(identity(1)));
    set(&mut svc, &mut fs, &ada, "scheme", "dark");
    assert!(
        !fs.exists(&alloc::format!("{HOME}/Settings/Apps/os.tairix.terminal")),
        "staging creates no store at all"
    );
    // A fresh process instance of the same app sees nothing.
    let other = origin(ACCOUNT_UID, 2, Some(identity(1)));
    assert_eq!(
        get(&mut svc, &mut fs, &other, "scheme"),
        Err(Errno::NotFound)
    );
}

#[test]
fn a_caller_reads_back_its_own_staged_edits() {
    let (mut svc, mut fs) = service();
    let ada = origin(ACCOUNT_UID, 1, Some(identity(1)));
    set(&mut svc, &mut fs, &ada, "font.size", "14");
    commit(&mut svc, &mut fs, &ada);

    set(&mut svc, &mut fs, &ada, "font.size", "18");
    assert_eq!(
        get(&mut svc, &mut fs, &ada, "font.size").as_deref(),
        Ok("18"),
        "a settings sheet reads back what it just set"
    );
    // …and another process instance still sees the committed value.
    let other = origin(ACCOUNT_UID, 2, Some(identity(1)));
    assert_eq!(
        get(&mut svc, &mut fs, &other, "font.size").as_deref(),
        Ok("14")
    );
}

#[test]
fn a_staged_removal_reads_as_absent_and_publishes_as_removed() {
    let (mut svc, mut fs) = service();
    let ada = origin(ACCOUNT_UID, 1, Some(identity(1)));
    set(&mut svc, &mut fs, &ada, "scheme", "dark");
    commit(&mut svc, &mut fs, &ada);

    let reply = call(
        &mut svc,
        &mut fs,
        &ada,
        &AppDataRequest::ConfigUnset {
            scope: ConfigScope::Private,
            key: "scheme",
        },
    );
    assert_eq!(decode_status_reply(&reply), Ok(()));
    assert_eq!(get(&mut svc, &mut fs, &ada, "scheme"), Err(Errno::NotFound));
    commit(&mut svc, &mut fs, &ada);
    let other = origin(ACCOUNT_UID, 2, Some(identity(1)));
    assert_eq!(
        get(&mut svc, &mut fs, &other, "scheme"),
        Err(Errno::NotFound)
    );
}

#[test]
fn two_process_instances_of_one_app_stage_independently() {
    // Keyed on the process instance, not the app: one instance's commit must
    // never publish another's half-finished edits.
    let (mut svc, mut fs) = service();
    let one = origin(ACCOUNT_UID, 1, Some(identity(1)));
    let two = origin(ACCOUNT_UID, 2, Some(identity(1)));
    set(&mut svc, &mut fs, &one, "a", "1");
    set(&mut svc, &mut fs, &two, "b", "2");
    assert_eq!(svc.staging_sessions(), 2);

    commit(&mut svc, &mut fs, &one);
    assert_eq!(get(&mut svc, &mut fs, &two, "a").as_deref(), Ok("1"));
    // Instance two's own edit is still only staged for instance two.
    let three = origin(ACCOUNT_UID, 3, Some(identity(1)));
    assert_eq!(get(&mut svc, &mut fs, &three, "b"), Err(Errno::NotFound));
    commit(&mut svc, &mut fs, &two);
    assert_eq!(get(&mut svc, &mut fs, &three, "b").as_deref(), Ok("2"));
}

#[test]
fn a_commit_with_nothing_staged_writes_nothing() {
    let (mut svc, mut fs) = service();
    let ada = origin(ACCOUNT_UID, 1, Some(identity(1)));
    commit(&mut svc, &mut fs, &ada);
    assert!(
        !fs.exists(&alloc::format!("{HOME}/Settings/Apps/os.tairix.terminal")),
        "a caller that changed nothing must not create a store"
    );
}

#[test]
fn a_committed_change_preserves_the_users_own_hand_edits() {
    let (mut svc, mut fs) = service();
    let ada = origin(ACCOUNT_UID, 1, Some(identity(1)));
    set(&mut svc, &mut fs, &ada, "scheme", "light");
    commit(&mut svc, &mut fs, &ada);
    let path = alloc::format!("{HOME}/Settings/Apps/os.tairix.terminal/settings.conf");
    fs.put(
        &path,
        b"# ada's own note\nscheme = light\nsomething the parser cannot read\n",
    );

    set(&mut svc, &mut fs, &ada, "scheme", "dark");
    commit(&mut svc, &mut fs, &ada);
    assert_eq!(
        fs.read_text(&path).as_deref(),
        Some("# ada's own note\nscheme = dark\nsomething the parser cannot read\n"),
        "a save must never destroy what a human wrote"
    );
}

#[test]
fn the_policy_layer_sets_a_default_the_user_can_override() {
    let (mut svc, mut fs) = service();
    fs.put(
        "/System/Settings/os.tairix.terminal/settings.conf",
        b"font.size = 18\nscheme = corporate\n",
    );
    let ada = origin(ACCOUNT_UID, 1, Some(identity(1)));
    // An administrator's default applies from the app's very first launch,
    // before it has ever written anything of its own.
    assert_eq!(
        get(&mut svc, &mut fs, &ada, "font.size").as_deref(),
        Ok("18"),
        "the machine-wide default is visible with no user file at all"
    );
    assert_eq!(
        keys(&mut svc, &mut fs, &ada),
        ["font.size", "scheme"],
        "and is in the document it is served"
    );
    assert!(
        !fs.exists(&alloc::format!("{HOME}/Settings/Apps/os.tairix.terminal")),
        "reading a policy default must not create a store"
    );

    set(&mut svc, &mut fs, &ada, "scheme", "dark");
    commit(&mut svc, &mut fs, &ada);
    assert_eq!(
        get(&mut svc, &mut fs, &ada, "font.size").as_deref(),
        Ok("18"),
        "and still is once the app has a store of its own"
    );
    assert_eq!(
        get(&mut svc, &mut fs, &ada, "scheme").as_deref(),
        Ok("dark"),
        "and the user's own choice wins over it"
    );

    // A user's file must never absorb a policy value it never set.
    let path = alloc::format!("{HOME}/Settings/Apps/os.tairix.terminal/settings.conf");
    assert_eq!(fs.read_text(&path).as_deref(), Some("scheme = dark\n"));

    // Unsetting the override falls back to the policy layer rather than to
    // nothing.
    let reply = call(
        &mut svc,
        &mut fs,
        &ada,
        &AppDataRequest::ConfigUnset {
            scope: ConfigScope::Private,
            key: "scheme",
        },
    );
    assert_eq!(decode_status_reply(&reply), Ok(()));
    commit(&mut svc, &mut fs, &ada);
    assert_eq!(
        get(&mut svc, &mut fs, &ada, "scheme").as_deref(),
        Ok("corporate")
    );
}

#[test]
fn a_read_covers_both_layers_and_the_callers_own_staging() {
    let (mut svc, mut fs) = service();
    fs.put(
        "/System/Settings/os.tairix.terminal/settings.conf",
        b"policy.only = 1\nscheme = corporate\n",
    );
    let ada = origin(ACCOUNT_UID, 1, Some(identity(1)));
    set(&mut svc, &mut fs, &ada, "scheme", "dark");
    set(&mut svc, &mut fs, &ada, "font.size", "14");
    commit(&mut svc, &mut fs, &ada);

    let mut named = keys(&mut svc, &mut fs, &ada);
    named.sort_unstable();
    assert_eq!(named, ["font.size", "policy.only", "scheme"]);
    // The policy layer sets the default and the user's own value wins, in one
    // document rather than two round trips.
    let document = read(&mut svc, &mut fs, &ada).expect("reads");
    assert_eq!(document.get("scheme"), Some("dark"));
    assert_eq!(document.get("policy.only"), Some("1"));
    assert!(
        document.settings().count() == 3,
        "each key appears once, however many layers set it"
    );

    // A staged write appears; a staged removal disappears.
    set(&mut svc, &mut fs, &ada, "recent.0", "/notes.txt");
    let reply = call(
        &mut svc,
        &mut fs,
        &ada,
        &AppDataRequest::ConfigUnset {
            scope: ConfigScope::Private,
            key: "font.size",
        },
    );
    assert_eq!(decode_status_reply(&reply), Ok(()));
    let mut named = keys(&mut svc, &mut fs, &ada);
    named.sort_unstable();
    assert_eq!(named, ["policy.only", "recent.0", "scheme"]);
}

#[test]
fn a_document_past_the_callers_capacity_is_answered_with_its_length() {
    // A caller sizes a small buffer for the store it expects and is told
    // exactly what to ask again with — never handed a truncated prefix it
    // could parse as if it were the whole store.
    let (mut svc, mut fs) = service();
    let ada = origin(ACCOUNT_UID, 1, Some(identity(1)));
    for index in 0..40 {
        set(
            &mut svc,
            &mut fs,
            &ada,
            &alloc::format!("recent.{index}"),
            "/Users/ada/Documents/notes.txt",
        );
    }
    commit(&mut svc, &mut fs, &ada);

    let reply = call(
        &mut svc,
        &mut fs,
        &ada,
        &AppDataRequest::ConfigRead {
            scope: ConfigScope::Private,
            capacity: 16,
        },
    );
    let needed = match decode_document_reply(&reply) {
        Ok(ConfigDocument::NeedsCapacity(len)) => len,
        other => panic!("a 16-byte buffer cannot hold 40 settings: {other:?}"),
    };
    assert!(needed > 16);

    // Asking again with exactly that much yields the whole document.
    let reply = call(
        &mut svc,
        &mut fs,
        &ada,
        &AppDataRequest::ConfigRead {
            scope: ConfigScope::Private,
            capacity: u32::try_from(needed).expect("fits a u32"),
        },
    );
    let text = match decode_document_reply(&reply) {
        Ok(ConfigDocument::Whole(text)) => text,
        other => panic!("the declared length must suffice: {other:?}"),
    };
    assert_eq!(text.len(), needed);
    let document = Document::parse(text).expect("parses");
    assert_eq!(document.settings().count(), 40);
}

#[test]
fn a_malformed_key_or_value_is_refused_before_anything_is_staged() {
    let (mut svc, mut fs) = service();
    let ada = origin(ACCOUNT_UID, 1, Some(identity(1)));
    for key in ["Scheme", "font..size", ".leading", "font.size#"] {
        let reply = call(
            &mut svc,
            &mut fs,
            &ada,
            &AppDataRequest::ConfigSet {
                scope: ConfigScope::Private,
                key,
                value: "x",
            },
        );
        assert_eq!(
            decode_status_reply(&reply),
            Err(Errno::OutOfRange),
            "`{key}` is outside the key grammar"
        );
    }
    // A value carrying a control character the format cannot render.
    let reply = call(
        &mut svc,
        &mut fs,
        &ada,
        &AppDataRequest::ConfigSet {
            scope: ConfigScope::Private,
            key: "scheme",
            value: "da\u{7}rk",
        },
    );
    assert_eq!(decode_status_reply(&reply), Err(Errno::OutOfRange));
    assert_eq!(svc.staging_sessions(), 0, "nothing was staged");
}

#[test]
fn a_malformed_frame_is_refused_without_touching_a_store() {
    let (mut svc, mut fs) = service();
    let ada = origin(ACCOUNT_UID, 1, Some(identity(1)));
    let mut reply = alloc::vec![0u8; APPDATA_MAX_REPLY];
    for frame in [&b""[..], &b"not a frame at all"[..], &[0xFFu8; 32][..]] {
        let len = svc.serve(&mut fs, &ada, 0, frame, &mut reply);
        assert!(len > 0, "a refusal is still a reply");
        assert!(decode_status_reply(&reply[..len]).is_err());
    }
    assert_eq!(svc.staging_sessions(), 0);
}

#[test]
fn a_value_at_the_formats_bound_survives_the_round_trip() {
    let (mut svc, mut fs) = service();
    let ada = origin(ACCOUNT_UID, 1, Some(identity(1)));
    let widest = "v".repeat(APPDATA_VALUE_MAX);
    set(&mut svc, &mut fs, &ada, "big", &widest);
    commit(&mut svc, &mut fs, &ada);
    assert_eq!(
        get(&mut svc, &mut fs, &ada, "big").as_deref(),
        Ok(widest.as_str())
    );
}

#[test]
fn an_empty_value_is_stored_and_read_back_as_set() {
    let (mut svc, mut fs) = service();
    let ada = origin(ACCOUNT_UID, 1, Some(identity(1)));
    set(&mut svc, &mut fs, &ada, "greeting", "");
    commit(&mut svc, &mut fs, &ada);
    assert_eq!(get(&mut svc, &mut fs, &ada, "greeting").as_deref(), Ok(""));
}

#[test]
fn a_runaway_writer_is_bounded_rather_than_growing_without_limit() {
    let (mut svc, mut fs) = service();
    let ada = origin(ACCOUNT_UID, 1, Some(identity(1)));
    for index in 0..MAX_PENDING_EDITS {
        set(&mut svc, &mut fs, &ada, &alloc::format!("k{index}"), "v");
    }
    let reply = call(
        &mut svc,
        &mut fs,
        &ada,
        &AppDataRequest::ConfigSet {
            scope: ConfigScope::Private,
            key: "one.too.many",
            value: "v",
        },
    );
    assert_eq!(decode_status_reply(&reply), Err(Errno::LimitExceeded));
    // Re-setting a key already staged is not a new edit, so it still lands.
    set(&mut svc, &mut fs, &ada, "k0", "v2");
}

#[test]
fn an_abandoned_staging_session_is_reclaimed() {
    // No primitive tells a server that a peer died, so an idle session is
    // reclaimed by age. Losing an abandoned session's edits is exactly the
    // contract: a caller that never commits changes nothing.
    let (mut svc, mut fs) = service();
    let ada = origin(ACCOUNT_UID, 1, Some(identity(1)));
    set(&mut svc, &mut fs, &ada, "scheme", "dark");
    assert_eq!(svc.staging_sessions(), 1);

    // A later request from anyone ages the abandoned session out.
    let other = origin(ACCOUNT_UID, 2, Some(identity(1)));
    let _ = call_at(
        &mut svc,
        &mut fs,
        &other,
        STAGING_IDLE_NS,
        &AppDataRequest::ConfigRead {
            scope: ConfigScope::Private,
            capacity: 4096,
        },
    );
    assert_eq!(svc.staging_sessions(), 0);
    assert_eq!(get(&mut svc, &mut fs, &ada, "scheme"), Err(Errno::NotFound));
}

#[test]
fn a_session_touched_within_the_idle_window_survives() {
    let (mut svc, mut fs) = service();
    let ada = origin(ACCOUNT_UID, 1, Some(identity(1)));
    let mut now = 0u64;
    for step in 0..4 {
        now += STAGING_IDLE_NS / 2;
        let reply = call_at(
            &mut svc,
            &mut fs,
            &ada,
            now,
            &AppDataRequest::ConfigSet {
                scope: ConfigScope::Private,
                key: "scheme",
                value: "dark",
            },
        );
        assert_eq!(decode_status_reply(&reply), Ok(()), "step {step}");
        assert_eq!(svc.staging_sessions(), 1);
    }
    let reply = call_at(
        &mut svc,
        &mut fs,
        &ada,
        now,
        &AppDataRequest::ConfigCommit {
            scope: ConfigScope::Private,
        },
    );
    assert_eq!(decode_status_reply(&reply), Ok(()));
    assert_eq!(
        get(&mut svc, &mut fs, &ada, "scheme").as_deref(),
        Ok("dark")
    );
}

#[test]
fn a_failed_commit_leaves_the_edits_staged_for_a_retry() {
    let (mut svc, mut fs) = service();
    let ada = origin(ACCOUNT_UID, 1, Some(identity(1)));
    set(&mut svc, &mut fs, &ada, "scheme", "dark");
    fs.fail_all(Errno::DeviceOffline);
    let reply = call(
        &mut svc,
        &mut fs,
        &ada,
        &AppDataRequest::ConfigCommit {
            scope: ConfigScope::Private,
        },
    );
    assert_eq!(decode_status_reply(&reply), Err(Errno::DeviceOffline));
    assert_eq!(svc.staging_sessions(), 1, "the edits survive the failure");
}

#[test]
fn an_unreachable_volume_answers_a_typed_refusal_not_a_default() {
    // The service comes up before the encrypted root is unlocked. An early
    // caller must be told the store cannot be reached, never handed a value.
    let mut svc = AppData::new(DiscardSink, CountingEntropy::new(1));
    let mut fs = TestFs::provisioned();
    fs.fail_all(Errno::DeviceOffline);
    let ada = origin(ACCOUNT_UID, 1, Some(identity(1)));
    assert_eq!(
        get(&mut svc, &mut fs, &ada, "scheme"),
        Err(Errno::DeviceOffline)
    );
}

#[test]
fn every_store_tree_is_one_the_home_shape_provisions() {
    // The service composes `<home>/<parent>/Apps/<bundle-id>` for each tree; a
    // parent the provisioners never gate would leave that whole scope
    // unreachable, so the names come from the one shared definition.
    for tree in tairix_users::AppDataTree::ALL {
        assert!(
            tairix_users::APPDATA_ROOT_PARENTS.contains(&tree.parent()),
            "{} is not a provisioned app-data parent",
            tree.parent()
        );
    }
}

#[test]
fn the_key_and_value_bounds_are_the_formats_own() {
    // This crate is the one place that links both the wire surface and the
    // format engine, so it is where a drift between them would be caught —
    // and there is nothing to drift, because the format imports the wire
    // field widths rather than restating them.
    assert_eq!(APPDATA_VALUE_MAX, tairix_appconf::MAX_VALUE_LEN);
    assert_eq!(
        tairix_abi::appdata_ipc::APPDATA_KEY_MAX,
        tairix_appconf::MAX_KEY_LEN
    );
}

#[test]
fn a_home_is_resolved_once_and_re_authorised_every_time() {
    // Without the cache every settings read would re-list `/Users` and stat
    // each entry — a directory scan on the hot path, growing with the number
    // of accounts. What is remembered is the *path*; the ownership that
    // authorises it is re-checked on every use.
    let (mut svc, mut fs) = service();
    let ada = origin(ACCOUNT_UID, 1, Some(identity(1)));
    assert_eq!(svc.resolved_roots(), 0);
    set(&mut svc, &mut fs, &ada, "scheme", "dark");
    commit(&mut svc, &mut fs, &ada);
    assert_eq!(svc.resolved_roots(), 1);
    assert_eq!(
        get(&mut svc, &mut fs, &ada, "scheme").as_deref(),
        Ok("dark")
    );
    assert_eq!(
        svc.resolved_roots(),
        1,
        "a second read resolves nothing new"
    );

    // Reassigning the home (the one act that could invalidate a resolution,
    // and it needs CAP_FS_CHOWN) must not let the stale path serve it.
    fs.set_owner(HOME, ACCOUNT_UID + 5);
    assert_eq!(get(&mut svc, &mut fs, &ada, "scheme"), Err(Errno::NotFound));
    assert_eq!(svc.resolved_roots(), 0, "the stale entry was forgotten");

    // …and the account that now owns it reaches its own store.
    let other = origin(ACCOUNT_UID + 5, 2, Some(identity(1)));
    assert_eq!(
        get(&mut svc, &mut fs, &other, "scheme").as_deref(),
        Ok("dark")
    );
}

#[test]
fn a_home_created_after_startup_still_resolves() {
    // Only successful resolutions are remembered, so a miss is never cached
    // and an account created while the service is running is reachable.
    let (mut svc, mut fs) = service();
    let bob = origin(ACCOUNT_UID + 1, 1, Some(identity(1)));
    assert_eq!(get(&mut svc, &mut fs, &bob, "scheme"), Err(Errno::NotFound));
    fs.add_home("/Users/bob", ACCOUNT_UID + 1);
    set(&mut svc, &mut fs, &bob, "scheme", "light");
    commit(&mut svc, &mut fs, &bob);
    assert_eq!(
        get(&mut svc, &mut fs, &bob, "scheme").as_deref(),
        Ok("light")
    );
}

#[test]
fn the_two_scopes_are_separate_stores_to_a_caller() {
    // A write to what an app publishes must not appear in its private
    // settings, nor the reverse: the scope selector is the whole of what
    // separates them, and it is on every own-store request.
    let (mut svc, mut fs) = service();
    let ada = origin(ACCOUNT_UID, 1, Some(identity(1)));
    set_in(
        &mut svc,
        &mut fs,
        &ada,
        ConfigScope::Private,
        "imap.user",
        "ada",
    );
    commit_in(&mut svc, &mut fs, &ada, ConfigScope::Private);
    set_in(
        &mut svc,
        &mut fs,
        &ada,
        ConfigScope::Public,
        "font.family",
        "berkeley",
    );
    commit_in(&mut svc, &mut fs, &ada, ConfigScope::Public);

    let private = read_in(&mut svc, &mut fs, &ada, ConfigScope::Private).expect("reads");
    assert_eq!(private.get("imap.user"), Some("ada"));
    assert_eq!(private.get("font.family"), None);

    let public = read_in(&mut svc, &mut fs, &ada, ConfigScope::Public).expect("reads");
    assert_eq!(public.get("font.family"), Some("berkeley"));
    assert_eq!(public.get("imap.user"), None);
}

#[test]
fn a_commit_publishes_one_scope_and_leaves_the_others_edits_staged() {
    // One rename replaces one name, so a commit names the scope it publishes.
    // A settings sheet's unsaved work must survive the app publishing about
    // itself, and vice versa.
    let (mut svc, mut fs) = service();
    let ada = origin(ACCOUNT_UID, 1, Some(identity(1)));
    set_in(
        &mut svc,
        &mut fs,
        &ada,
        ConfigScope::Private,
        "scheme",
        "dark",
    );
    set_in(
        &mut svc,
        &mut fs,
        &ada,
        ConfigScope::Public,
        "font.family",
        "berkeley",
    );
    assert_eq!(svc.staging_sessions(), 1, "one session, two scopes' edits");

    commit_in(&mut svc, &mut fs, &ada, ConfigScope::Public);
    assert_eq!(
        svc.staging_sessions(),
        1,
        "the private edit is still staged"
    );

    // A fresh instance sees the published value and not the private one.
    let other = origin(ACCOUNT_UID, 2, Some(identity(1)));
    assert_eq!(
        read_in(&mut svc, &mut fs, &other, ConfigScope::Public)
            .expect("reads")
            .get("font.family"),
        Some("berkeley")
    );
    assert_eq!(
        read_in(&mut svc, &mut fs, &other, ConfigScope::Private)
            .expect("reads")
            .get("scheme"),
        None
    );

    commit_in(&mut svc, &mut fs, &ada, ConfigScope::Private);
    assert_eq!(svc.staging_sessions(), 0, "and now the session is spent");
    assert_eq!(
        read_in(&mut svc, &mut fs, &other, ConfigScope::Private)
            .expect("reads")
            .get("scheme"),
        Some("dark")
    );
}

#[test]
fn each_scope_gets_its_own_pending_edit_budget() {
    // The bound is per scope because each scope is a document of its own: one
    // of them filling up must not deny the other.
    let (mut svc, mut fs) = service();
    let ada = origin(ACCOUNT_UID, 1, Some(identity(1)));
    for index in 0..MAX_PENDING_EDITS {
        set_in(
            &mut svc,
            &mut fs,
            &ada,
            ConfigScope::Private,
            &alloc::format!("k{index}"),
            "v",
        );
    }
    let reply = call(
        &mut svc,
        &mut fs,
        &ada,
        &AppDataRequest::ConfigSet {
            scope: ConfigScope::Private,
            key: "one.too.many",
            value: "v",
        },
    );
    assert_eq!(decode_status_reply(&reply), Err(Errno::LimitExceeded));
    // The published scope is untouched by the private scope's runaway writer.
    set_in(
        &mut svc,
        &mut fs,
        &ada,
        ConfigScope::Public,
        "font.family",
        "berkeley",
    );
}

#[test]
fn an_app_reads_what_another_app_publishes() {
    // The opt-in the published scope exists for: one app writes it, another
    // app of the same account reads it — and neither names a path.
    let (mut svc, mut fs) = service();
    let terminal = origin(ACCOUNT_UID, 1, Some(identity(1)));
    let notes = origin(
        ACCOUNT_UID,
        2,
        Some(AppIdentity::new("org.pty.notes", publisher(2)).expect("well formed")),
    );
    set_in(
        &mut svc,
        &mut fs,
        &terminal,
        ConfigScope::Public,
        "font.family",
        "berkeley",
    );
    commit_in(&mut svc, &mut fs, &terminal, ConfigScope::Public);

    let published = read_published(&mut svc, &mut fs, &notes, "os.tairix.terminal").expect("reads");
    assert_eq!(published.get("font.family"), Some("berkeley"));
}

#[test]
fn a_foreign_read_cannot_reach_the_private_scope() {
    // The property AD6 must not weaken: opening a published scope must not
    // open a door onto the private one. There is no scope field on a foreign
    // read, so there is nothing to try.
    let (mut svc, mut fs) = service();
    let terminal = origin(ACCOUNT_UID, 1, Some(identity(1)));
    let notes = origin(
        ACCOUNT_UID,
        2,
        Some(AppIdentity::new("org.pty.notes", publisher(2)).expect("well formed")),
    );
    set_in(
        &mut svc,
        &mut fs,
        &terminal,
        ConfigScope::Private,
        "imap.password",
        "hunter2",
    );
    commit_in(&mut svc, &mut fs, &terminal, ConfigScope::Private);
    set_in(
        &mut svc,
        &mut fs,
        &terminal,
        ConfigScope::Public,
        "font.family",
        "berkeley",
    );
    commit_in(&mut svc, &mut fs, &terminal, ConfigScope::Public);

    let published = read_published(&mut svc, &mut fs, &notes, "os.tairix.terminal").expect("reads");
    assert_eq!(published.get("font.family"), Some("berkeley"));
    assert_eq!(
        published.get("imap.password"),
        None,
        "publishing one scope must not expose the other"
    );
    assert_eq!(published.settings().count(), 1);
}

#[test]
fn a_foreign_read_answers_committed_data_not_a_publishers_staging() {
    // A published value is what every other app sees, so it is the committed
    // document — a publisher's unsaved work is nobody else's business, and a
    // reader must not act on a value that may never be published.
    let (mut svc, mut fs) = service();
    let terminal = origin(ACCOUNT_UID, 1, Some(identity(1)));
    let notes = origin(
        ACCOUNT_UID,
        2,
        Some(AppIdentity::new("org.pty.notes", publisher(2)).expect("well formed")),
    );
    set_in(
        &mut svc,
        &mut fs,
        &terminal,
        ConfigScope::Public,
        "font.family",
        "berkeley",
    );
    commit_in(&mut svc, &mut fs, &terminal, ConfigScope::Public);
    set_in(
        &mut svc,
        &mut fs,
        &terminal,
        ConfigScope::Public,
        "font.family",
        "not-yet-published",
    );

    assert_eq!(
        read_published(&mut svc, &mut fs, &notes, "os.tairix.terminal")
            .expect("reads")
            .get("font.family"),
        Some("berkeley")
    );
    // The publisher's own read still shows its staged value.
    assert_eq!(
        read_in(&mut svc, &mut fs, &terminal, ConfigScope::Public)
            .expect("reads")
            .get("font.family"),
        Some("not-yet-published")
    );
}

#[test]
fn a_foreign_read_of_an_app_that_publishes_nothing_answers_the_empty_document() {
    // An app with no store, an app that has published nothing, and an app
    // whose store cannot be attested answer identically — so a caller cannot
    // use the endpoint to discover which applications an account has run.
    let (mut svc, mut fs) = service();
    let notes = origin(
        ACCOUNT_UID,
        2,
        Some(AppIdentity::new("org.pty.notes", publisher(2)).expect("well formed")),
    );
    let terminal = origin(ACCOUNT_UID, 1, Some(identity(1)));
    // A store that exists but publishes nothing.
    set(&mut svc, &mut fs, &terminal, "scheme", "dark");
    commit(&mut svc, &mut fs, &terminal);

    for target in ["os.tairix.terminal", "com.example.never-run"] {
        let published = read_published(&mut svc, &mut fs, &notes, target).expect("reads");
        assert!(
            published.settings().next().is_none(),
            "`{target}` publishes nothing"
        );
    }

    // A store whose pin attests nothing reads the same way.
    fs.put(
        &alloc::format!("{HOME}/Settings/Apps/os.tairix.terminal/.owner"),
        b"junk",
    );
    assert!(
        read_published(&mut svc, &mut fs, &notes, "os.tairix.terminal")
            .expect("reads")
            .settings()
            .next()
            .is_none(),
        "a broken target publishes nothing, and says no more than that"
    );
}

#[test]
fn a_foreign_read_reports_the_callers_own_unreachable_volume() {
    // A target's defect is answered empty; the caller's own environment is
    // reported as itself, so an early caller is not told an app publishes
    // nothing when the truth is that no store can be read at all.
    let (mut svc, mut fs) = service();
    let notes = origin(
        ACCOUNT_UID,
        2,
        Some(AppIdentity::new("org.pty.notes", publisher(2)).expect("well formed")),
    );
    fs.fail_all(Errno::DeviceOffline);
    assert_eq!(
        read_published(&mut svc, &mut fs, &notes, "os.tairix.terminal").err(),
        Some(Errno::DeviceOffline)
    );
}

#[test]
fn a_foreign_read_never_crosses_an_account() {
    let (mut svc, mut fs) = service();
    fs.add_home("/Users/bob", ACCOUNT_UID + 1);
    let notes = AppIdentity::new("org.pty.notes", publisher(2)).expect("well formed");
    let ada_terminal = origin(ACCOUNT_UID, 1, Some(identity(1)));
    let bob_terminal = origin(ACCOUNT_UID + 1, 2, Some(identity(1)));
    set_in(
        &mut svc,
        &mut fs,
        &ada_terminal,
        ConfigScope::Public,
        "owner",
        "ada",
    );
    commit_in(&mut svc, &mut fs, &ada_terminal, ConfigScope::Public);
    set_in(
        &mut svc,
        &mut fs,
        &bob_terminal,
        ConfigScope::Public,
        "owner",
        "bob",
    );
    commit_in(&mut svc, &mut fs, &bob_terminal, ConfigScope::Public);

    let ada_notes = origin(ACCOUNT_UID, 3, Some(notes));
    let bob_notes = origin(ACCOUNT_UID + 1, 4, Some(notes));
    assert_eq!(
        read_published(&mut svc, &mut fs, &ada_notes, "os.tairix.terminal")
            .expect("reads")
            .get("owner"),
        Some("ada")
    );
    assert_eq!(
        read_published(&mut svc, &mut fs, &bob_notes, "os.tairix.terminal")
            .expect("reads")
            .get("owner"),
        Some("bob")
    );
}

#[test]
fn a_squatting_publisher_cannot_publish_over_the_owners_document() {
    // The pin governs both scopes: a different developer claiming a bundle
    // identifier is refused before it can put anything in front of readers of
    // the real app's published document.
    let (mut svc, mut fs) = service();
    let honest = origin(ACCOUNT_UID, 1, Some(identity(1)));
    set_in(
        &mut svc,
        &mut fs,
        &honest,
        ConfigScope::Public,
        "font.family",
        "berkeley",
    );
    commit_in(&mut svc, &mut fs, &honest, ConfigScope::Public);

    let squatter = origin(ACCOUNT_UID, 2, Some(identity(2)));
    set_in(
        &mut svc,
        &mut fs,
        &squatter,
        ConfigScope::Public,
        "font.family",
        "hostile",
    );
    let reply = call(
        &mut svc,
        &mut fs,
        &squatter,
        &AppDataRequest::ConfigCommit {
            scope: ConfigScope::Public,
        },
    );
    assert_eq!(decode_status_reply(&reply), Err(Errno::PermissionDenied));

    let notes = origin(
        ACCOUNT_UID,
        3,
        Some(AppIdentity::new("org.pty.notes", publisher(3)).expect("well formed")),
    );
    assert_eq!(
        read_published(&mut svc, &mut fs, &notes, "os.tairix.terminal")
            .expect("reads")
            .get("font.family"),
        Some("berkeley"),
        "readers still see the real publisher's document"
    );
}

#[test]
fn a_foreign_read_naming_a_traversal_is_refused_before_a_store_is_touched() {
    // The bundle identifier is the only caller-supplied component of any store
    // path, and it arrives already inside the one identifier grammar. This is
    // that guarantee at the service's own door: the frame encodes (the codec
    // bounds lengths, not grammars) and the *decode* refuses it, so no path is
    // ever composed from it.
    let (mut svc, mut fs) = service();
    let ada = origin(ACCOUNT_UID, 1, Some(identity(1)));
    set(&mut svc, &mut fs, &ada, "scheme", "dark");
    commit(&mut svc, &mut fs, &ada);

    for hostile in ["..", ".", "../../etc", "a/b", "OS.tairix", ".hidden", "a b"] {
        let reply = call(
            &mut svc,
            &mut fs,
            &ada,
            &AppDataRequest::PublicRead {
                bundle_id: hostile,
                capacity: 4096,
            },
        );
        assert!(
            matches!(
                decode_status_reply(&reply),
                Err(Errno::OutOfRange | Errno::LengthOutOfRange)
            ),
            "`{hostile}` must be refused, not resolved"
        );
    }
    // And the store the caller does own is untouched by any of it.
    assert_eq!(
        get(&mut svc, &mut fs, &ada, "scheme").as_deref(),
        Ok("dark")
    );
}

// --- The sealed scope ----------------------------------------------------

#[test]
fn a_secret_survives_the_round_trip_and_is_never_on_the_volume_in_the_clear() {
    let (mut svc, mut fs) = service();
    let ada = origin(ACCOUNT_UID, 1, Some(identity(1)));
    seal(&mut svc, &mut fs, &ada, "imap.password", "hunter2");
    assert_eq!(
        read_vault(&mut svc, &mut fs, &ada)
            .expect("reads")
            .get("imap.password"),
        Some("hunter2")
    );
    assert!(
        !fs.read_text(&alloc::format!(
            "{HOME}/Settings/Apps/os.tairix.terminal/secret.vault"
        ))
        .is_some_and(|text| text.contains("hunter2")),
        "the sealed document must not carry the plaintext"
    );
}

/// A sealed write is immediate: there is no commit, and nothing is staged — so
/// a caller that seals a secret and exits has sealed it.
#[test]
fn a_sealed_write_needs_no_commit_and_stages_nothing() {
    let (mut svc, mut fs) = service();
    let ada = origin(ACCOUNT_UID, 1, Some(identity(1)));
    seal(&mut svc, &mut fs, &ada, "k", "v");
    assert_eq!(
        svc.staging_sessions(),
        0,
        "the sealed scope holds no session"
    );
    // A second process instance of the same application reads it at once,
    // where a staged configuration edit would still be invisible to it.
    let other = origin(ACCOUNT_UID, 2, Some(identity(1)));
    assert_eq!(
        read_vault(&mut svc, &mut fs, &other)
            .expect("reads")
            .get("k"),
        Some("v")
    );
}

/// The whole point of the scope: one application's secrets are unreachable to
/// another, in every request shape there is.
#[test]
fn an_app_cannot_reach_another_apps_secrets() {
    let (mut svc, mut fs) = service();
    let terminal = origin(ACCOUNT_UID, 1, Some(identity(1)));
    seal(&mut svc, &mut fs, &terminal, "imap.password", "hunter2");

    let mail = origin(
        ACCOUNT_UID,
        2,
        Some(AppIdentity::new("os.tairix.mail", publisher(1)).expect("legal")),
    );
    // Its own sealed scope is empty...
    assert_eq!(
        read_vault(&mut svc, &mut fs, &mail)
            .expect("reads")
            .settings()
            .count(),
        0
    );
    // ...and the one request shape that names an application reaches only the
    // published scope, which carries no secret.
    assert_eq!(
        read_published(&mut svc, &mut fs, &mail, "os.tairix.terminal")
            .expect("reads")
            .get("imap.password"),
        None
    );
}

#[test]
fn one_users_secrets_are_invisible_to_another() {
    let (mut svc, mut fs) = service();
    fs.add_home("/Users/grace", ACCOUNT_UID + 1);
    let ada = origin(ACCOUNT_UID, 1, Some(identity(1)));
    let grace = origin(ACCOUNT_UID + 1, 2, Some(identity(1)));
    seal(&mut svc, &mut fs, &ada, "imap.password", "ada's");
    seal(&mut svc, &mut fs, &grace, "imap.password", "grace's");
    assert_eq!(
        read_vault(&mut svc, &mut fs, &ada)
            .expect("reads")
            .get("imap.password"),
        Some("ada's")
    );
    assert_eq!(
        read_vault(&mut svc, &mut fs, &grace)
            .expect("reads")
            .get("imap.password"),
        Some("grace's")
    );
}

#[test]
fn a_caller_with_no_attested_app_identity_has_no_sealed_scope() {
    let (mut svc, mut fs) = service();
    let stranger = origin(ACCOUNT_UID, 1, None);
    for request in [
        AppDataRequest::VaultRead { capacity: 4096 },
        AppDataRequest::VaultSet {
            key: "k",
            value: "v",
        },
        AppDataRequest::VaultUnset { key: "k" },
    ] {
        let reply = call(&mut svc, &mut fs, &stranger, &request);
        assert_eq!(
            decode_status_reply(&reply),
            Err(Errno::PermissionDenied),
            "{request:?} from a principal running no verified bundle"
        );
    }
    assert!(!fs.exists(&alloc::format!("{HOME}/Settings/Apps/.vault-master")));
}

#[test]
fn removing_a_secret_uncovers_nothing_beneath_it() {
    // The sealed scope has no layer under it — no bundle defaults, no
    // machine-wide policy — because a secret an application did not write is
    // not one it may be made to believe. A removal therefore leaves the key
    // absent, not falling back to something else.
    let (mut svc, mut fs) = service();
    let ada = origin(ACCOUNT_UID, 1, Some(identity(1)));
    fs.put(
        &alloc::format!("/System/Settings/os.tairix.terminal/{}", "settings.conf"),
        b"imap.password = a policy secret\n",
    );
    seal(&mut svc, &mut fs, &ada, "imap.password", "hunter2");
    let reply = call(
        &mut svc,
        &mut fs,
        &ada,
        &AppDataRequest::VaultUnset {
            key: "imap.password",
        },
    );
    assert_eq!(decode_status_reply(&reply), Ok(()));
    assert_eq!(
        read_vault(&mut svc, &mut fs, &ada)
            .expect("reads")
            .get("imap.password"),
        None,
        "no layer beneath the sealed scope may supply a secret"
    );
}

/// The sealed and configuration scopes are separate documents: a secret never
/// appears in a configuration read, and a setting never in a sealed one.
#[test]
fn the_sealed_scope_and_the_configuration_scopes_do_not_leak_into_each_other() {
    let (mut svc, mut fs) = service();
    let ada = origin(ACCOUNT_UID, 1, Some(identity(1)));
    seal(&mut svc, &mut fs, &ada, "imap.password", "hunter2");
    set_in(
        &mut svc,
        &mut fs,
        &ada,
        ConfigScope::Private,
        "font.size",
        "14",
    );
    commit_in(&mut svc, &mut fs, &ada, ConfigScope::Private);

    for scope in [ConfigScope::Private, ConfigScope::Public] {
        assert_eq!(
            read_in(&mut svc, &mut fs, &ada, scope)
                .expect("reads")
                .get("imap.password"),
            None,
            "{scope:?} must not carry a secret"
        );
    }
    assert_eq!(
        read_vault(&mut svc, &mut fs, &ada)
            .expect("reads")
            .get("font.size"),
        None,
        "the sealed scope must not carry a setting"
    );
}

#[test]
fn a_sealed_key_or_value_outside_the_grammar_is_refused_before_anything_is_sealed() {
    let (mut svc, mut fs) = service();
    let ada = origin(ACCOUNT_UID, 1, Some(identity(1)));
    for request in [
        AppDataRequest::VaultSet {
            key: "Not.A.Key",
            value: "v",
        },
        AppDataRequest::VaultUnset { key: "Not.A.Key" },
        // A value the wire carries but the format will not hold, so the
        // refusal is the engine's rather than the codec's.
        AppDataRequest::VaultSet {
            key: "k",
            value: "a \u{7} bell",
        },
    ] {
        let reply = call(&mut svc, &mut fs, &ada, &request);
        assert_eq!(
            decode_status_reply(&reply),
            Err(Errno::OutOfRange),
            "{request:?}"
        );
    }
    assert!(
        !fs.exists(&alloc::format!("{HOME}/Settings/Apps/.vault-master")),
        "a refused write draws the account no key material"
    );
}

#[test]
fn an_unreachable_volume_refuses_the_sealed_scope_rather_than_answering_empty() {
    let (mut svc, mut fs) = service();
    let ada = origin(ACCOUNT_UID, 1, Some(identity(1)));
    seal(&mut svc, &mut fs, &ada, "k", "v");
    fs.fail_all(Errno::DeviceOffline);
    let capacity = u32::try_from(APPDATA_DOCUMENT_MAX).expect("fits a u32");
    let reply = call(
        &mut svc,
        &mut fs,
        &ada,
        &AppDataRequest::VaultRead { capacity },
    );
    assert_eq!(decode_status_reply(&reply), Err(Errno::DeviceOffline));
}

/// A vault that cannot be opened is reported as damaged, not as empty: an
/// application must never conclude "no password saved" from a tampered record.
#[test]
fn a_tampered_sealed_document_is_reported_and_not_answered_as_empty() {
    let (mut svc, mut fs) = service();
    let ada = origin(ACCOUNT_UID, 1, Some(identity(1)));
    seal(&mut svc, &mut fs, &ada, "imap.password", "hunter2");
    let path = alloc::format!("{HOME}/Settings/Apps/os.tairix.terminal/secret.vault");
    let mut record = fs.read(&path).expect("sealed");
    let last = record.len() - 1;
    record[last] ^= 0x01;
    fs.put(&path, &record);
    let capacity = u32::try_from(APPDATA_DOCUMENT_MAX).expect("fits a u32");
    let reply = call(
        &mut svc,
        &mut fs,
        &ada,
        &AppDataRequest::VaultRead { capacity },
    );
    assert_eq!(decode_status_reply(&reply), Err(Errno::SignatureInvalid));
}

#[test]
fn a_sealed_read_negotiates_capacity_like_any_other_document() {
    let (mut svc, mut fs) = service();
    let ada = origin(ACCOUNT_UID, 1, Some(identity(1)));
    seal(&mut svc, &mut fs, &ada, "imap.password", "hunter2");
    let reply = call(
        &mut svc,
        &mut fs,
        &ada,
        &AppDataRequest::VaultRead { capacity: 1 },
    );
    match decode_document_reply(&reply).expect("a reply") {
        ConfigDocument::NeedsCapacity(needed) => assert!(needed > 1),
        ConfigDocument::Whole(text) => panic!("a one-byte buffer must not fit {text:?}"),
    }
}

#[test]
fn a_blob_open_answers_a_bounded_grant_and_never_bytes() {
    // The whole point of the scope: the service decides once, hands over a
    // descriptor, and is off the data path. The reply carries a handle the
    // caller redeems — and the delegation behind it is bounded by the extent
    // ceiling, so direct access is not unbounded access.
    let (mut svc, mut fs) = service();
    let ada = origin(ACCOUNT_UID, 1, Some(identity(1)));
    let reply = call(
        &mut svc,
        &mut fs,
        &ada,
        &AppDataRequest::BlobOpen {
            name: "mail.index",
            mode: BlobMode::ReadWrite,
        },
    );
    let handle = decode_grant_reply(&reply).expect("a grant handle");
    assert_ne!(handle, 0);
    let minted = fs.grants().last().expect("one delegation");
    assert!(minted.write);
    assert_eq!(minted.ceiling, APPDATA_BLOB_MAX_BYTES);
    assert_eq!(
        minted.task,
        ada.pid(),
        "the grant is minted to the caller's attested task, never a wire value"
    );
    assert!(
        minted.path.starts_with(&alloc::format!(
            "{HOME}/Library/Apps/os.tairix.terminal/Blobs/"
        )),
        "a blob lives in the caller's own gated bulk store: {}",
        minted.path
    );
}

#[test]
fn one_apps_blobs_are_unreachable_from_another() {
    // The isolation the scope exists for: two applications of one account ask
    // for the same blob name and reach two different files, and neither can
    // name the other's at all — there is no request shape that carries an
    // application identifier into the blob scope.
    let (mut svc, mut fs) = service();
    let terminal = origin(ACCOUNT_UID, 1, Some(identity(1)));
    let notes = origin(
        ACCOUNT_UID,
        2,
        Some(AppIdentity::new("org.pty.notes", publisher(2)).expect("valid identity")),
    );
    for who in [&terminal, &notes] {
        let reply = call(
            &mut svc,
            &mut fs,
            who,
            &AppDataRequest::BlobOpen {
                name: "index",
                mode: BlobMode::ReadWrite,
            },
        );
        decode_grant_reply(&reply).expect("each app opens its own");
    }
    let paths: Vec<&str> = fs.grants().iter().map(|g| g.path.as_str()).collect();
    assert_eq!(paths.len(), 2);
    assert_ne!(paths[0], paths[1], "one name, two applications, two files");

    // And each sees only its own in a listing.
    for who in [&terminal, &notes] {
        let reply = call(
            &mut svc,
            &mut fs,
            who,
            &AppDataRequest::BlobList { capacity: 4096 },
        );
        let listing = decode_blob_list_reply(&reply).expect("a listing");
        assert_eq!(
            listing
                .entries()
                .map(|entry| String::from(entry.name))
                .collect::<Vec<_>>(),
            [String::from("index")]
        );
    }
}

#[test]
fn a_blob_read_of_a_blob_that_does_not_exist_creates_nothing() {
    let (mut svc, mut fs) = service();
    let ada = origin(ACCOUNT_UID, 1, Some(identity(1)));
    let reply = call(
        &mut svc,
        &mut fs,
        &ada,
        &AppDataRequest::BlobOpen {
            name: "index",
            mode: BlobMode::Read,
        },
    );
    assert_eq!(decode_grant_reply(&reply), Err(Errno::NotFound));
    assert!(fs.grants().is_empty(), "nothing was delegated");
    assert!(
        !fs.exists(&alloc::format!("{HOME}/Library/Apps/os.tairix.terminal")),
        "and no store was provisioned by a read"
    );
}

#[test]
fn a_quota_read_reports_usage_against_the_ceilings_it_is_bounded_by() {
    let (mut svc, mut fs) = service();
    let ada = origin(ACCOUNT_UID, 1, Some(identity(1)));
    let empty = decode_quota_reply(&call(&mut svc, &mut fs, &ada, &AppDataRequest::QuotaGet {}))
        .expect("a quota");
    assert_eq!(empty.blobs, 0);
    assert_eq!(empty.bytes, 0);
    assert_eq!(
        empty.blob_max,
        u64::try_from(APPDATA_BLOB_MAX_COUNT).expect("fits")
    );
    assert_eq!(empty.blob_bytes_max, APPDATA_BLOB_MAX_BYTES);

    call(
        &mut svc,
        &mut fs,
        &ada,
        &AppDataRequest::BlobOpen {
            name: "index",
            mode: BlobMode::ReadWrite,
        },
    );
    let path = &fs.grants().last().expect("one delegation").path.clone();
    fs.put(path, b"12345");
    let used = decode_quota_reply(&call(&mut svc, &mut fs, &ada, &AppDataRequest::QuotaGet {}))
        .expect("a quota");
    assert_eq!(used.blobs, 1);
    assert_eq!(used.bytes, 5);
}

#[test]
fn a_blob_delete_removes_the_file_and_frees_its_slot() {
    let (mut svc, mut fs) = service();
    let ada = origin(ACCOUNT_UID, 1, Some(identity(1)));
    call(
        &mut svc,
        &mut fs,
        &ada,
        &AppDataRequest::BlobOpen {
            name: "index",
            mode: BlobMode::ReadWrite,
        },
    );
    let path = fs.grants().last().expect("one delegation").path.clone();
    assert!(fs.exists(&path));
    let reply = call(
        &mut svc,
        &mut fs,
        &ada,
        &AppDataRequest::BlobDelete { name: "index" },
    );
    assert_eq!(decode_status_reply(&reply), Ok(()));
    assert!(!fs.exists(&path));
    // A second delete is the same answer: a refusal here would be an oracle
    // for what the store holds.
    let reply = call(
        &mut svc,
        &mut fs,
        &ada,
        &AppDataRequest::BlobDelete { name: "index" },
    );
    assert_eq!(decode_status_reply(&reply), Ok(()));
}

#[test]
fn a_blob_operation_is_refused_when_the_volume_is_unreachable() {
    // Early boot, before any volume is unlocked: a typed refusal, never a
    // guess and never an empty listing that would read as "you have no blobs".
    let (mut svc, mut fs) = service();
    let ada = origin(ACCOUNT_UID, 1, Some(identity(1)));
    fs.fail_all(Errno::DeviceOffline);
    for request in [
        AppDataRequest::BlobOpen {
            name: "index",
            mode: BlobMode::ReadWrite,
        },
        AppDataRequest::BlobDelete { name: "index" },
        AppDataRequest::BlobList { capacity: 4096 },
        AppDataRequest::QuotaGet {},
    ] {
        let reply = call(&mut svc, &mut fs, &ada, &request);
        assert_eq!(
            decode_status_reply(&reply),
            Err(Errno::DeviceOffline),
            "{request:?} must report the volume rather than an absence"
        );
    }
}

#[test]
fn a_listing_past_the_callers_capacity_is_answered_with_its_length() {
    // The same whole-or-nothing contract a document read has: a caller either
    // holds the whole listing or knows exactly how big a buffer to ask again
    // with, so nothing acts on a listing missing entries it would have read.
    use tairix_abi::appdata_ipc::BlobListing;
    let (mut svc, mut fs) = service();
    let ada = origin(ACCOUNT_UID, 1, Some(identity(1)));
    for name in ["a", "b", "c"] {
        call(
            &mut svc,
            &mut fs,
            &ada,
            &AppDataRequest::BlobOpen {
                name,
                mode: BlobMode::ReadWrite,
            },
        );
    }
    let reply = call(
        &mut svc,
        &mut fs,
        &ada,
        &AppDataRequest::BlobList { capacity: 1 },
    );
    let needed = match decode_blob_list_reply(&reply) {
        Ok(BlobListing::NeedsCapacity(needed)) => needed,
        other => panic!("expected a capacity refusal, got {other:?}"),
    };
    let reply = call(
        &mut svc,
        &mut fs,
        &ada,
        &AppDataRequest::BlobList {
            capacity: u32::try_from(needed).expect("fits"),
        },
    );
    assert_eq!(
        decode_blob_list_reply(&reply)
            .expect("a listing")
            .entries()
            .count(),
        3
    );
}
