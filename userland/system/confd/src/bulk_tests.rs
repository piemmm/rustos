//! Bulk-store tests: where a blob and a temporary file live, who may reach
//! them, what bounds direct access, and how a boot's scratch is told from an
//! earlier boot's.

use alloc::string::String;
use alloc::vec::Vec;

use tairix_abi::appdata_ipc::{
    BlobMode, APPDATA_BLOB_ENTRY_LEN, APPDATA_BLOB_MAX_COUNT, APPDATA_BULK_FILE_MAX_BYTES,
    APPDATA_TEMP_MAX_COUNT,
};
use tairix_abi::{AppIdentity, BootId, Errno, ProcId, BOOT_ID_LEN};

use super::{render_listing, BlobStore, TempNames, TempStore, BLOBS_DIR, TEMP_DIR};
use crate::bulk;
use crate::store::tests::{identity, publisher};
use crate::store::{AppStore, RootCache, StoreError};
use crate::testfs::{Grant, TestFs, ACCOUNT_UID, HOME};
use crate::vault::tests::CountingEntropy;

/// The process instance a grant is minted to in these tests — attested by
/// the kernel, never a value the caller supplied.
const TASK: ProcId = ProcId::from_raw([42u8; tairix_abi::PROC_ID_LEN]);

/// A second, distinct instance, so a test can show two recipients apart.
const OTHER_TASK: ProcId = ProcId::from_raw([43u8; tairix_abi::PROC_ID_LEN]);

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
    assert_eq!(blobs.usage(&mut fs), Ok((0, 0)));
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
            ceiling: APPDATA_BULK_FILE_MAX_BYTES,
            recipient: TASK,
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
            recipient: TASK,
        }
    );
}

#[test]
fn a_grant_is_minted_only_to_the_instance_it_was_asked_for() {
    // The recipient is the caller's kernel-attested process instance, never
    // anything on the wire and never a task id that could change hands, so a
    // handle that leaks is useless to whoever holds it.
    let mut fs = TestFs::provisioned();
    let blobs = open(&mut fs, true).expect("resolves");
    for task in [TASK, OTHER_TASK] {
        blobs
            .grant(&mut fs, "index", BlobMode::ReadWrite, task)
            .expect("mints");
    }
    assert_eq!(
        fs.grants().iter().map(|g| g.recipient).collect::<Vec<_>>(),
        [TASK, OTHER_TASK]
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
    assert_eq!(blobs.usage(&mut fs), Ok((3, 10)));

    // The reported ceilings are the ones admission and the delegation enforce,
    // assembled in one place so a caller's gauge and the refusal it will meet
    // cannot disagree.
    let quota = bulk::quota(blobs.usage(&mut fs).expect("usage"), (0, 0));
    assert_eq!(
        quota.blob_max,
        u64::try_from(APPDATA_BLOB_MAX_COUNT).expect("fits")
    );
    assert_eq!(
        quota.temp_max,
        u64::try_from(APPDATA_TEMP_MAX_COUNT).expect("fits")
    );
    assert_eq!(quota.file_bytes_max, APPDATA_BULK_FILE_MAX_BYTES);
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
            Err(StoreError::StoreNameRefused),
            "`{hostile}` must never name a blob"
        );
        assert_eq!(
            blobs.delete(&mut fs, hostile),
            Err(StoreError::StoreNameRefused)
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
    assert_eq!(blobs.usage(&mut fs), Err(StoreError::Unavailable));
}

#[test]
fn a_rendered_listing_is_whole_entries_the_wire_accepts() {
    let listing = alloc::vec![
        (String::from("index"), 1),
        (String::from("mail.index"), APPDATA_BULK_FILE_MAX_BYTES),
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

/// The naming rule of the boot `tag`.
fn names(tag: u8) -> TempNames {
    TempNames::of(BootId::from_raw([tag; BOOT_ID_LEN])).expect("a boot with an identity")
}

/// Resolve the configuration store, then the temporary files beside it, under
/// the naming rule of the boot `tag`.
fn open_temp(fs: &mut TestFs, tag: u8, create: bool) -> Result<TempStore, StoreError> {
    let mut roots = RootCache::new();
    let store = AppStore::open(fs, &mut roots, ACCOUNT_UID, &identity(1), true)?;
    TempStore::open(fs, &store, names(tag), create)
}

/// The absolute path a temporary file of `name` occupies for `identity(1)`.
fn temp_path(name: &str) -> String {
    alloc::format!("{HOME}/Library/Apps/os.tairix.terminal/{TEMP_DIR}/{name}")
}

/// A source drawing a different slot each time, as the real generator does.
fn drawing(first: u8) -> CountingEntropy {
    CountingEntropy::new(first)
}

#[test]
fn a_boot_with_no_identity_has_no_naming_rule() {
    // The unset identity is what a port whose random reserve never seeded
    // reports. With it there is no way to tell this boot's scratch from an
    // earlier boot's, so the scope is refused whole rather than serving files
    // it could never reclaim.
    assert_eq!(TempNames::of(BootId::UNSET), None);
    assert_ne!(names(1), names(2));
}

#[test]
fn a_created_file_is_named_for_this_boot_and_granted_writable() {
    let mut fs = TestFs::provisioned();
    let temp = open_temp(&mut fs, 1, true).expect("resolves");
    let (handle, name) = temp
        .create(&mut fs, &mut drawing(1), TASK)
        .expect("a fresh file");
    assert_ne!(handle, 0, "a minted handle is never the invalid one");
    assert!(names(1).is_live(&name), "the name says which boot it is");
    assert!(!names(2).is_live(&name), "and no other boot's");
    assert_eq!(
        fs.grants(),
        [Grant {
            path: temp_path(&name),
            write: true,
            ceiling: APPDATA_BULK_FILE_MAX_BYTES,
            recipient: TASK,
        }]
    );

    // Every create answers a name of its own: freshness without coordination
    // is the whole of what the scope is for.
    let (_, second) = temp
        .create(&mut fs, &mut drawing(9), TASK)
        .expect("a fresh file");
    assert_ne!(name, second);
    assert_eq!(temp.usage(&mut fs), Ok((2, 0)));
}

#[test]
fn an_earlier_boots_scratch_is_invisible_and_reclaimed_before_the_next_create() {
    // The lifetime of a temporary file is the boot. Nothing an application
    // asks may see an earlier boot's, and the bytes go back to the volume the
    // first time the scope needs room — not on a walk of every account's every
    // store at start-up, and never on a service restart, whose boot identity
    // is unchanged.
    let mut fs = TestFs::provisioned();
    let before = open_temp(&mut fs, 1, true).expect("resolves");
    let (_, stale) = before
        .create(&mut fs, &mut drawing(1), TASK)
        .expect("a fresh file");
    fs.put(&temp_path(&stale), b"secrets from the last run");

    let after = open_temp(&mut fs, 2, true).expect("resolves");
    assert_eq!(after.usage(&mut fs), Ok((0, 0)), "invisible at once");
    assert!(
        fs.exists(&temp_path(&stale)),
        "and still there until needed"
    );

    let (_, fresh) = after
        .create(&mut fs, &mut drawing(1), TASK)
        .expect("a fresh file");
    assert!(
        !fs.exists(&temp_path(&stale)),
        "reclaimed by the next create"
    );
    assert!(fs.exists(&temp_path(&fresh)));
    assert_eq!(after.usage(&mut fs), Ok((1, 0)));

    // A blob is not scratch: a reap must never reach one, whatever boot it was
    // written in.
    let blobs = open(&mut fs, true).expect("resolves");
    blobs
        .grant(&mut fs, "index", BlobMode::ReadWrite, TASK)
        .expect("mints");
    open_temp(&mut fs, 3, true)
        .expect("resolves")
        .create(&mut fs, &mut drawing(1), TASK)
        .expect("a fresh file");
    assert_eq!(
        blobs.listing(&mut fs),
        Ok(alloc::vec![(String::from("index"), 0)])
    );
}

#[test]
fn a_release_frees_a_slot_and_removing_an_absent_file_is_not_an_error() {
    let mut fs = TestFs::provisioned();
    let temp = open_temp(&mut fs, 1, true).expect("resolves");
    let (_, name) = temp
        .create(&mut fs, &mut drawing(1), TASK)
        .expect("a fresh file");
    assert_eq!(temp.release(&mut fs, &name), Ok(()));
    assert!(!fs.exists(&temp_path(&name)));
    assert_eq!(temp.usage(&mut fs), Ok((0, 0)));

    // Idempotent, so a release is never an oracle for what the store holds —
    // and a second one cannot reach a file drawn after it, because a slot is
    // drawn afresh rather than re-issued.
    assert_eq!(temp.release(&mut fs, &name), Ok(()));
    let (_, later) = temp
        .create(&mut fs, &mut drawing(7), TASK)
        .expect("a fresh file");
    assert_ne!(name, later);
    assert_eq!(temp.release(&mut fs, &name), Ok(()));
    assert!(fs.exists(&temp_path(&later)));
}

#[test]
fn a_release_refuses_a_name_outside_the_store_name_grammar() {
    // The wire decoder refuses one first; re-stating it here is what makes the
    // path this module composes safe on its own terms.
    let mut fs = TestFs::provisioned();
    let temp = open_temp(&mut fs, 1, true).expect("resolves");
    for name in ["..", "", "/etc/passwd", "Scratch"] {
        assert_eq!(
            temp.release(&mut fs, name),
            Err(StoreError::StoreNameRefused)
        );
    }
}

#[test]
fn the_count_ceiling_bounds_a_leaking_application_and_a_release_lifts_it() {
    let mut fs = TestFs::provisioned();
    let temp = open_temp(&mut fs, 1, true).expect("resolves");
    let mut entropy = drawing(1);
    let mut held = Vec::new();
    for _ in 0..APPDATA_TEMP_MAX_COUNT {
        let (_, name) = temp
            .create(&mut fs, &mut entropy, TASK)
            .expect("a fresh file");
        held.push(name);
    }
    assert_eq!(
        temp.create(&mut fs, &mut entropy, TASK).err(),
        Some(StoreError::TempLimit)
    );
    // Releasing one frees its slot at once, so an application that finishes
    // with its scratch is never held to what it once held.
    temp.release(&mut fs, &held[0]).expect("released");
    assert!(temp.create(&mut fs, &mut entropy, TASK).is_ok());
}

#[test]
fn a_generator_that_answers_a_name_already_taken_refuses_rather_than_hands_it_over() {
    // A drawn slot cannot collide in any run this machine will see, so a name
    // that is taken means the generator is not delivering what it claimed —
    // and handing the descriptor over would give the caller another instance's
    // open scratch file.
    let mut fs = TestFs::provisioned();
    let temp = open_temp(&mut fs, 1, true).expect("resolves");
    let mut stuck = CountingEntropy::broken();
    temp.create(&mut fs, &mut stuck, TASK)
        .expect("a fresh file");
    assert_eq!(
        temp.create(&mut fs, &mut stuck, TASK).err(),
        Some(StoreError::TempUnavailable)
    );

    // A generator that refuses outright is the same refusal: no file, no name.
    let mut refusing = CountingEntropy::refusing(Errno::EntropyNotReady);
    assert_eq!(
        temp.create(&mut fs, &mut refusing, TASK).err(),
        Some(StoreError::TempUnavailable)
    );
}

#[test]
fn an_unpinned_store_holds_no_temporary_files() {
    // The same rule the blob scope keeps: with no ownership pin there is no
    // attested owner, so there is nothing this service may serve — and nothing
    // a later publisher claiming the identifier could inherit.
    let mut fs = TestFs::provisioned();
    let mut roots = RootCache::new();
    let unpinned =
        AppStore::open(&mut fs, &mut roots, ACCOUNT_UID, &identity(1), false).expect("resolves");
    assert!(!unpinned.is_pinned());
    let temp = TempStore::open(&mut fs, &unpinned, names(1), true).expect("resolves");
    assert_eq!(temp.usage(&mut fs), Ok((0, 0)));
    assert_eq!(
        temp.create(&mut fs, &mut drawing(1), TASK).err(),
        Some(StoreError::Unavailable)
    );
    assert!(fs.grants().is_empty());
}

#[test]
fn an_unreachable_volume_refuses_every_temporary_operation_as_itself() {
    let mut fs = TestFs::provisioned();
    let temp = open_temp(&mut fs, 1, true).expect("resolves");
    fs.fail_all(Errno::DeviceOffline);
    assert_eq!(
        temp.create(&mut fs, &mut drawing(1), TASK).err(),
        Some(StoreError::Unavailable)
    );
    assert_eq!(
        temp.release(&mut fs, "scratch"),
        Err(StoreError::Unavailable)
    );
    assert_eq!(temp.usage(&mut fs), Err(StoreError::Unavailable));
}

#[test]
fn a_reap_composes_no_path_from_a_name_this_service_did_not_create() {
    // The reap unlinks what the boot does not own, so it is the one path that
    // takes a name *from the volume* and turns it back into a path. Every such
    // name goes through the store-name grammar at enumeration, so a directory
    // entry that could traverse is never composed into one — it is left where
    // it is, counted by nothing and unlinked by nothing.
    let mut fs = TestFs::provisioned();
    let temp = open_temp(&mut fs, 1, true).expect("resolves");
    let (_, live) = temp
        .create(&mut fs, &mut drawing(1), TASK)
        .expect("a fresh file");
    fs.put(&temp_path("Scratch"), b"not a name this service composes");
    fs.add_dir(&temp_path("adirectory"), tairix_users::CONFD_UID.0);

    open_temp(&mut fs, 2, true)
        .expect("resolves")
        .create(&mut fs, &mut drawing(1), TASK)
        .expect("a fresh file");
    assert!(
        !fs.exists(&temp_path(&live)),
        "the earlier boot's file went"
    );
    assert!(fs.exists(&temp_path("Scratch")), "and nothing else did");
    assert!(fs.exists(&temp_path("adirectory")));
}
