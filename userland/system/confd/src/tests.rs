//! End-to-end dispatcher tests: a framed request plus a kernel-attested origin
//! in, a decodable reply out — and the isolation properties the service exists
//! to provide.

use alloc::string::String;
use alloc::vec::Vec;

use tairix_abi::appdata_ipc::{
    decode_value_reply, AppDataKeyRecord, AppDataRequest, APPDATA_LIST_PAGE_MAX, APPDATA_MAX_REPLY,
    APPDATA_MAX_REQUEST, APPDATA_VALUE_MAX,
};
use tairix_abi::origin::{CapabilitySummary, TrustDomain, ORIGIN_CONSOLE_NONE, PROC_ID_LEN};
use tairix_abi::reply::{decode_page_reply, decode_status_reply};
use tairix_abi::{AppIdentity, Errno, Origin, ProcId};
use tairix_log::DiscardSink;

use super::{AppData, MAX_PENDING_EDITS, STAGING_IDLE_NS};
use crate::store::tests::{identity, publisher};
use crate::testfs::{TestFs, ACCOUNT_UID, HOME};

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

/// A dispatcher over a freshly provisioned volume.
fn service() -> (AppData<DiscardSink>, TestFs) {
    (AppData::new(DiscardSink), TestFs::provisioned())
}

/// Serve `request` and hand back the raw reply frame.
fn call(
    service: &mut AppData<DiscardSink>,
    fs: &mut TestFs,
    origin: &Origin,
    request: &AppDataRequest<'_>,
) -> Vec<u8> {
    call_at(service, fs, origin, 0, request)
}

/// As [`call`], at monotonic instant `now_ns`.
fn call_at(
    service: &mut AppData<DiscardSink>,
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

/// Serve a `ConfigSet` and assert it was accepted.
fn set(
    service: &mut AppData<DiscardSink>,
    fs: &mut TestFs,
    origin: &Origin,
    key: &str,
    value: &str,
) {
    let reply = call(
        service,
        fs,
        origin,
        &AppDataRequest::ConfigSet { key, value },
    );
    assert_eq!(decode_status_reply(&reply), Ok(()), "set {key}");
}

/// Serve a `ConfigCommit` and assert it was accepted.
fn commit(service: &mut AppData<DiscardSink>, fs: &mut TestFs, origin: &Origin) {
    let reply = call(service, fs, origin, &AppDataRequest::ConfigCommit);
    assert_eq!(decode_status_reply(&reply), Ok(()), "commit");
}

/// Serve a `ConfigGet` and decode its outcome.
fn get(
    service: &mut AppData<DiscardSink>,
    fs: &mut TestFs,
    origin: &Origin,
    key: &str,
) -> Result<String, Errno> {
    let reply = call(service, fs, origin, &AppDataRequest::ConfigGet { key });
    decode_value_reply(&reply).map(String::from)
}

/// Serve a `ConfigList` page and decode the keys it names.
fn list(
    service: &mut AppData<DiscardSink>,
    fs: &mut TestFs,
    origin: &Origin,
    prefix: &str,
    cursor: u32,
) -> Vec<String> {
    let reply = call(
        service,
        fs,
        origin,
        &AppDataRequest::ConfigList { prefix, cursor },
    );
    let (_, body) = decode_page_reply(&reply, AppDataKeyRecord::WIRE_LEN, APPDATA_LIST_PAGE_MAX)
        .expect("a well-formed page");
    body.chunks(AppDataKeyRecord::WIRE_LEN)
        .map(|chunk| {
            String::from(
                AppDataKeyRecord::from_bytes(chunk)
                    .expect("a well-formed record")
                    .as_str(),
            )
        })
        .collect()
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
        list(&mut svc, &mut fs, &two, "", 0).is_empty(),
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
        AppDataRequest::ConfigGet { key: "scheme" },
        AppDataRequest::ConfigSet {
            key: "scheme",
            value: "dark",
        },
        AppDataRequest::ConfigUnset { key: "scheme" },
        AppDataRequest::ConfigCommit,
        AppDataRequest::ConfigList {
            prefix: "",
            cursor: 0,
        },
    ] {
        let reply = call(&mut svc, &mut fs, &anon, &request);
        assert_eq!(
            decode_status_reply(&reply),
            Err(Errno::PermissionDenied),
            "{request:?} must be refused"
        );
    }
    assert_eq!(svc.staging_sessions(), 0, "and nothing was staged");
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
            key: "scheme",
            value: "hostile",
        },
    );
    assert_eq!(decode_status_reply(&reply), Ok(()), "the set only stages");
    let reply = call(&mut svc, &mut fs, &squatter, &AppDataRequest::ConfigCommit);
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
        &AppDataRequest::ConfigUnset { key: "scheme" },
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
        list(&mut svc, &mut fs, &ada, "", 0),
        ["font.size", "scheme"],
        "and is listed"
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
        &AppDataRequest::ConfigUnset { key: "scheme" },
    );
    assert_eq!(decode_status_reply(&reply), Ok(()));
    commit(&mut svc, &mut fs, &ada);
    assert_eq!(
        get(&mut svc, &mut fs, &ada, "scheme").as_deref(),
        Ok("corporate")
    );
}

#[test]
fn a_listing_covers_both_layers_and_the_callers_own_staging() {
    let (mut svc, mut fs) = service();
    fs.put(
        "/System/Settings/os.tairix.terminal/settings.conf",
        b"policy.only = 1\nscheme = corporate\n",
    );
    let ada = origin(ACCOUNT_UID, 1, Some(identity(1)));
    set(&mut svc, &mut fs, &ada, "scheme", "dark");
    set(&mut svc, &mut fs, &ada, "font.size", "14");
    commit(&mut svc, &mut fs, &ada);

    let mut keys = list(&mut svc, &mut fs, &ada, "", 0);
    keys.sort_unstable();
    assert_eq!(keys, ["font.size", "policy.only", "scheme"]);

    // A staged write appears; a staged removal disappears.
    set(&mut svc, &mut fs, &ada, "recent.0", "/notes.txt");
    let reply = call(
        &mut svc,
        &mut fs,
        &ada,
        &AppDataRequest::ConfigUnset { key: "font.size" },
    );
    assert_eq!(decode_status_reply(&reply), Ok(()));
    let mut keys = list(&mut svc, &mut fs, &ada, "", 0);
    keys.sort_unstable();
    assert_eq!(keys, ["policy.only", "recent.0", "scheme"]);

    // A prefix selects a family, and an unmatched prefix selects nothing.
    assert_eq!(list(&mut svc, &mut fs, &ada, "recent.", 0), ["recent.0"]);
    assert!(list(&mut svc, &mut fs, &ada, "nothing.", 0).is_empty());
}

#[test]
fn a_listing_pages_and_a_short_page_is_the_last() {
    let (mut svc, mut fs) = service();
    let ada = origin(ACCOUNT_UID, 1, Some(identity(1)));
    let total = usize::from(APPDATA_LIST_PAGE_MAX) + 5;
    for index in 0..total {
        set(
            &mut svc,
            &mut fs,
            &ada,
            &alloc::format!("recent.{index}"),
            "x",
        );
    }
    commit(&mut svc, &mut fs, &ada);

    let first = list(&mut svc, &mut fs, &ada, "", 0);
    assert_eq!(first.len(), usize::from(APPDATA_LIST_PAGE_MAX));
    let second = list(
        &mut svc,
        &mut fs,
        &ada,
        "",
        u32::from(APPDATA_LIST_PAGE_MAX),
    );
    assert_eq!(second.len(), 5, "the short page is the last");
    // Every key appears exactly once across the two pages.
    let mut all: Vec<String> = first;
    all.extend(second);
    all.sort_unstable();
    all.dedup();
    assert_eq!(all.len(), total);
    // A cursor past the end is an empty page, not an error.
    assert!(list(&mut svc, &mut fs, &ada, "", u32::MAX).is_empty());
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
            &AppDataRequest::ConfigSet { key, value: "x" },
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
        &AppDataRequest::ConfigGet { key: "scheme" },
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
                key: "scheme",
                value: "dark",
            },
        );
        assert_eq!(decode_status_reply(&reply), Ok(()), "step {step}");
        assert_eq!(svc.staging_sessions(), 1);
    }
    let reply = call_at(&mut svc, &mut fs, &ada, now, &AppDataRequest::ConfigCommit);
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
    let reply = call(&mut svc, &mut fs, &ada, &AppDataRequest::ConfigCommit);
    assert_eq!(decode_status_reply(&reply), Err(Errno::DeviceOffline));
    assert_eq!(svc.staging_sessions(), 1, "the edits survive the failure");
}

#[test]
fn an_unreachable_volume_answers_a_typed_refusal_not_a_default() {
    // The service comes up before the encrypted root is unlocked. An early
    // caller must be told the store cannot be reached, never handed a value.
    let mut svc = AppData::new(DiscardSink);
    let mut fs = TestFs::provisioned();
    fs.fail_all(Errno::DeviceOffline);
    let ada = origin(ACCOUNT_UID, 1, Some(identity(1)));
    assert_eq!(
        get(&mut svc, &mut fs, &ada, "scheme"),
        Err(Errno::DeviceOffline)
    );
}

#[test]
fn the_scope_parent_is_one_the_home_shape_provisions() {
    // The service composes `<home>/<parent>/Apps/<bundle-id>`; a parent the
    // provisioners never gate would leave every store unreachable, so the name
    // is pinned to the one shared definition rather than spelled twice.
    assert!(
        tairix_users::APPDATA_ROOT_PARENTS.contains(&super::APPDATA_PARENT),
        "{} is not a provisioned app-data parent",
        super::APPDATA_PARENT
    );
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
