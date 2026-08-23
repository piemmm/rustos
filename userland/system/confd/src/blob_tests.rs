//! Blob-store tests: where a blob lives, who may reach it, and what bounds
//! direct access to it.

use alloc::string::String;
use alloc::vec::Vec;

use tairix_abi::appdata_ipc::{
    BlobMode, APPDATA_BLOB_ENTRY_LEN, APPDATA_BLOB_MAX_BYTES, APPDATA_BLOB_MAX_COUNT,
};
use tairix_abi::{AppIdentity, Errno};

use super::{render_listing, BlobStore, BLOBS_DIR};
use crate::store::tests::{identity, publisher};
use crate::store::{AppStore, RootCache, StoreError};
use crate::testfs::{Grant, TestFs, ACCOUNT_UID, HOME};

/// The task a grant is minted to in these tests — an attested `pid`, never a
/// value the caller supplied.
const TASK: u64 = 42;

/// Resolve the configuration store, then the blob store beside it.
fn open(fs: &mut TestFs, create: bool) -> Result<BlobStore, StoreError> {
    open_for(fs, &identity(1), create)
}

/// As [`open`], for a named application.
fn open_for(fs: &mut TestFs, who: &AppIdentity, create: bool) -> Result<BlobStore, StoreError> {
    let mut roots = RootCache::new();
    // `create` on the configuration store so the ownership pin exists; the
    // blob store's own `create` is the argument under test.
    let store = AppStore::open(fs, &mut roots, ACCOUNT_UID, who, true)?;
    BlobStore::open(fs, &store, create)
}

/// The absolute path a blob of `name` occupies for `identity(1)`.
fn blob_path(name: &str) -> String {
    alloc::format!("{HOME}/Library/Apps/os.tairix.terminal/{BLOBS_DIR}/{name}")
}

#[test]
fn a_read_never_creates_a_store_and_answers_empty() {
    // An application's first launch reads its cache before it has one. That
    // must cost no write at all: a store the service creates on a read is a
    // store every probe can provision.
    let mut fs = TestFs::provisioned();
    let blobs = open(&mut fs, false).expect("resolves");
    assert_eq!(blobs.listing(&mut fs), Ok(Vec::new()));
    let quota = blobs.quota(&mut fs).expect("quota");
    assert_eq!(quota.blobs, 0);
    assert_eq!(quota.bytes, 0);
    assert!(!fs.exists(&alloc::format!("{HOME}/Library/Apps/os.tairix.terminal")));
    // And a read-only open of a blob in a store that does not exist is the
    // same refusal as one in a store that does: the absence of the store is
    // not reported as something else.
    assert_eq!(
        blobs.grant(&mut fs, "index", BlobMode::Read, TASK),
        Err(StoreError::BlobNotFound)
    );
    assert!(!fs.exists(&blob_path("index")));
}

#[test]
fn a_writable_open_creates_the_blob_and_bounds_the_delegation() {
    let mut fs = TestFs::provisioned();
    let blobs = open(&mut fs, true).expect("resolves");
    let handle = blobs
        .grant(&mut fs, "mail.index", BlobMode::ReadWrite, TASK)
        .expect("a writable open mints a grant");
    assert_ne!(handle, 0, "handle 0 is the reserved invalid value");
    assert_eq!(
        fs.grants(),
        [Grant {
            path: blob_path("mail.index"),
            write: true,
            ceiling: APPDATA_BLOB_MAX_BYTES,
            task: TASK,
        }],
        "a writable delegation is bounded by the per-blob extent ceiling"
    );

    // A read-only open of the same blob now succeeds, and carries no extent:
    // reads cannot grow a file, so there is nothing to bound.
    blobs
        .grant(&mut fs, "mail.index", BlobMode::Read, TASK)
        .expect("a read of an existing blob mints a grant");
    assert_eq!(
        fs.grants()[1],
        Grant {
            path: blob_path("mail.index"),
            write: false,
            ceiling: 0,
            task: TASK,
        }
    );
}

#[test]
fn a_grant_is_minted_only_to_the_task_it_was_asked_for() {
    // The recipient is the caller's kernel-attested task id, never anything
    // on the wire, so a handle that leaks is useless to whoever holds it.
    let mut fs = TestFs::provisioned();
    let blobs = open(&mut fs, true).expect("resolves");
    for task in [TASK, TASK + 1] {
        blobs
            .grant(&mut fs, "index", BlobMode::ReadWrite, task)
            .expect("mints");
    }
    assert_eq!(
        fs.grants().iter().map(|g| g.task).collect::<Vec<_>>(),
        [TASK, TASK + 1]
    );
}

#[test]
fn the_blob_count_is_the_one_thing_admission_decides() {
    // Admission bounds the count; the kernel's extent ceiling bounds the
    // bytes. Summing sizes here would be theatre — the caller can grow a blob
    // it already holds open — so the count is the whole of what is checked.
    let mut fs = TestFs::provisioned();
    let blobs = open(&mut fs, true).expect("resolves");
    for n in 0..APPDATA_BLOB_MAX_COUNT {
        blobs
            .grant(&mut fs, &alloc::format!("b{n}"), BlobMode::ReadWrite, TASK)
            .unwrap_or_else(|err| panic!("blob {n} must be admitted: {err:?}"));
    }
    assert_eq!(
        blobs.grant(&mut fs, "one-too-many", BlobMode::ReadWrite, TASK),
        Err(StoreError::BlobLimit)
    );
    assert!(
        !fs.exists(&blob_path("one-too-many")),
        "a refused admission creates nothing"
    );
    // A blob it already holds still opens: the ceiling is on how many exist,
    // not on how often one is opened.
    assert!(blobs
        .grant(&mut fs, "b0", BlobMode::ReadWrite, TASK)
        .is_ok());
    // And freeing one makes room again.
    blobs.delete(&mut fs, "b0").expect("deletes");
    assert!(blobs
        .grant(&mut fs, "one-too-many", BlobMode::ReadWrite, TASK)
        .is_ok());
}

#[test]
fn a_delete_is_idempotent_and_creates_nothing() {
    let mut fs = TestFs::provisioned();
    // A store that does not exist: the delete succeeds having done nothing,
    // so it is neither an oracle nor a way to provision a store.
    let blobs = open(&mut fs, false).expect("resolves");
    assert_eq!(blobs.delete(&mut fs, "index"), Ok(()));
    assert!(!fs.exists(&alloc::format!("{HOME}/Library/Apps/os.tairix.terminal")));

    let blobs = open(&mut fs, true).expect("resolves");
    blobs
        .grant(&mut fs, "index", BlobMode::ReadWrite, TASK)
        .expect("mints");
    assert!(fs.exists(&blob_path("index")));
    assert_eq!(blobs.delete(&mut fs, "index"), Ok(()));
    assert!(!fs.exists(&blob_path("index")));
    // Twice is the same answer as once.
    assert_eq!(blobs.delete(&mut fs, "index"), Ok(()));
}

#[test]
fn a_listing_is_sorted_and_carries_every_blobs_length() {
    let mut fs = TestFs::provisioned();
    let blobs = open(&mut fs, true).expect("resolves");
    for name in ["thumbnails", "index", "mail.index"] {
        blobs
            .grant(&mut fs, name, BlobMode::ReadWrite, TASK)
            .expect("mints");
    }
    fs.put(&blob_path("index"), b"0123456789");
    assert_eq!(
        blobs.listing(&mut fs),
        Ok(alloc::vec![
            (String::from("index"), 10),
            (String::from("mail.index"), 0),
            (String::from("thumbnails"), 0),
        ])
    );
    let quota = blobs.quota(&mut fs).expect("quota");
    assert_eq!(quota.blobs, 3);
    assert_eq!(quota.bytes, 10);
    assert_eq!(
        quota.blob_max,
        u64::try_from(APPDATA_BLOB_MAX_COUNT).expect("fits")
    );
    assert_eq!(quota.blob_bytes_max, APPDATA_BLOB_MAX_BYTES);
}

#[test]
fn a_listing_reports_only_names_a_caller_could_have_asked_for() {
    // Nothing but this service can create a child in the gated tree, so an
    // entry outside the blob-name grammar cannot arise in practice — but if
    // one did, reporting it would hand a caller a name it cannot address and
    // could not delete. It is left out, and admission does not count it.
    let mut fs = TestFs::provisioned();
    let blobs = open(&mut fs, true).expect("resolves");
    blobs
        .grant(&mut fs, "index", BlobMode::ReadWrite, TASK)
        .expect("mints");
    let dir = alloc::format!("{HOME}/Library/Apps/os.tairix.terminal/{BLOBS_DIR}");
    fs.put(&alloc::format!("{dir}/Uppercase"), b"x");
    fs.put(&alloc::format!("{dir}/.hidden"), b"x");
    fs.add_dir(&alloc::format!("{dir}/subdir"), tairix_users::CONFD_UID.0);
    assert_eq!(
        blobs.listing(&mut fs),
        Ok(alloc::vec![(String::from("index"), 0)])
    );
}

#[test]
fn a_blob_name_outside_the_grammar_is_refused_by_the_store_too() {
    // The wire decoder refuses one, so this can only be reached by a defect
    // on the way in — which is exactly why the store restates the grammar
    // instead of trusting a caller's discipline.
    let mut fs = TestFs::provisioned();
    let blobs = open(&mut fs, true).expect("resolves");
    for hostile in ["..", ".", "a/b", "/etc", "A", ".hidden", "", "a b"] {
        assert_eq!(
            blobs.grant(&mut fs, hostile, BlobMode::ReadWrite, TASK),
            Err(StoreError::BlobNameRefused),
            "`{hostile}` must never name a blob"
        );
        assert_eq!(
            blobs.delete(&mut fs, hostile),
            Err(StoreError::BlobNameRefused)
        );
    }
    assert!(fs.grants().is_empty(), "nothing was delegated");
}

#[test]
fn a_squatting_publisher_reaches_no_blob_of_the_real_application() {
    // One `.owner` pin governs both trees, so the publisher check happens
    // before the bulk tree is composed at all: a different developer claiming
    // the identifier is refused, not handed an empty store it could then fill
    // in front of the real application's data.
    let mut fs = TestFs::provisioned();
    let blobs = open(&mut fs, true).expect("resolves");
    blobs
        .grant(&mut fs, "index", BlobMode::ReadWrite, TASK)
        .expect("mints");

    let squatter = AppIdentity::new("os.tairix.terminal", publisher(9)).expect("valid identity");
    assert_eq!(
        open_for(&mut fs, &squatter, true).err(),
        Some(StoreError::PublisherMismatch)
    );
    assert!(fs.exists(&blob_path("index")), "the real blob is untouched");
}

#[test]
fn a_bulk_root_that_is_not_the_services_own_is_refused() {
    // `Library/` is the user's own directory and carries no gate, so an
    // application can plant a directory named `Apps` inside it. The store must
    // become unavailable, never served out of the decoy — and the
    // configuration tree's root being sound says nothing about this one.
    let mut fs = TestFs::provisioned();
    let root = alloc::format!("{HOME}/Library/Apps");
    fs.remove(&root);
    fs.add_dir(&root, ACCOUNT_UID);
    assert_eq!(open(&mut fs, true).err(), Some(StoreError::RootNotOwned));
}

#[test]
fn an_unreachable_volume_is_reported_as_itself() {
    let mut fs = TestFs::provisioned();
    let blobs = open(&mut fs, true).expect("resolves");
    fs.fail_all(Errno::DeviceOffline);
    assert_eq!(
        blobs.grant(&mut fs, "index", BlobMode::ReadWrite, TASK),
        Err(StoreError::Unavailable)
    );
    assert_eq!(blobs.delete(&mut fs, "index"), Err(StoreError::Unavailable));
    assert_eq!(blobs.listing(&mut fs), Err(StoreError::Unavailable));
    assert_eq!(blobs.quota(&mut fs), Err(StoreError::Unavailable));
}

#[test]
fn a_rendered_listing_is_whole_entries_the_wire_accepts() {
    let listing = alloc::vec![
        (String::from("index"), 1),
        (String::from("mail.index"), APPDATA_BLOB_MAX_BYTES),
    ];
    let rendered = render_listing(&listing).expect("renders");
    assert_eq!(rendered.len(), listing.len() * APPDATA_BLOB_ENTRY_LEN);
    let mut out = alloc::vec![0u8; tairix_abi::appdata_ipc::APPDATA_MAX_REPLY];
    let len = tairix_abi::appdata_ipc::encode_blob_list_reply(&rendered, u32::MAX, &mut out)
        .expect("encodes");
    let decoded =
        tairix_abi::appdata_ipc::decode_blob_list_reply(&out[..len]).expect("round trips");
    assert_eq!(
        decoded
            .entries()
            .map(|entry| (String::from(entry.name), entry.len))
            .collect::<Vec<_>>(),
        listing
    );
}

#[test]
fn an_unpinned_store_holds_no_blobs_and_a_write_pins_before_it_creates_one() {
    // The regression this pins: opening the configuration store without
    // `create` on the blob path left an application that had written no
    // setting *unpinned*, so a blob it created had no recorded owner — and a
    // later publisher claiming the same identifier passed the pin check
    // vacuously and read it. Creating a blob must pin first.
    let mut fs = TestFs::provisioned();
    let mut roots = RootCache::new();

    // An unpinned configuration store: no `.owner`, nothing attested.
    let unpinned =
        AppStore::open(&mut fs, &mut roots, ACCOUNT_UID, &identity(1), false).expect("resolves");
    assert!(!unpinned.is_pinned());
    let blobs = BlobStore::open(&mut fs, &unpinned, true).expect("resolves");
    // No pin, no owner, so no blobs — whatever `create` asked for.
    assert_eq!(blobs.listing(&mut fs), Ok(Vec::new()));
    assert_eq!(
        blobs.grant(&mut fs, "index", BlobMode::ReadWrite, TASK),
        Err(StoreError::BlobNotFound)
    );
    assert!(fs.grants().is_empty());

    // The dispatcher's own path pins first, so the blob it then creates has a
    // recorded owner and a squatter is refused rather than handed it.
    let blobs = open(&mut fs, true).expect("resolves");
    blobs
        .grant(&mut fs, "index", BlobMode::ReadWrite, TASK)
        .expect("mints");
    let squatter = AppIdentity::new("os.tairix.terminal", publisher(9)).expect("valid identity");
    assert_eq!(
        AppStore::open(&mut fs, &mut roots, ACCOUNT_UID, &squatter, false).err(),
        Some(StoreError::PublisherMismatch),
        "the blob's owner was recorded, so the identifier cannot be inherited"
    );
}
