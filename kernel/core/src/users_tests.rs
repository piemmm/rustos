//! Behavioural tests for the boot-time users-database load
//! ([`crate::users::load_users_db`]): the success path and every
//! fail-closed refusal, each with its audit record.

use alloc::string::String;
use alloc::vec::Vec;

use rustos_abi::driver::filesystem::{
    DirEntry, FilesystemRead, FilesystemSecurity, NodeId, NodeInfo, NodeKind, NodeSecurity,
};
use rustos_abi::{CapabilityId, DriverError};
use rustos_users::{
    AccountState, Gid, Identity, ParseError, Salt, Uid, UserRecord, UsersDb, MAX_DB_LEN,
    MIN_ITERATIONS, SALT_LEN,
};

use crate::fs::VfsError;
use crate::test_sink::TestSink;
use crate::users::{
    load_users_db, load_users_db_source, LateUsersDb, UsersDbAlreadyInstalled, UsersDbSource,
    UsersLoadError,
};
use rustos_abi::Errno;

const ROOT: u64 = 1;
const SYSTEM: u64 = 2;
const SECURITY: u64 = 3;
const USERS: u64 = 4;

/// Mock root-volume driver: the fixed `/System/Security/Users` tree with
/// a configurable database node, mirroring what rustfs reports for the
/// mkimage-authored root volume.
struct MockRoot {
    /// Bytes of the `Users` node.
    content: Vec<u8>,
    /// Size `node_info` reports for the `Users` node; decoupled from
    /// `content.len()` so the short-read refusal is reachable.
    reported_size: u64,
    /// Whether the `Users` node exists at all.
    present: bool,
    /// Whether the `Users` node is a directory.
    is_dir: bool,
    /// record reported for the `Users` node.
    security: NodeSecurity,
    /// Set when `read_at` touches the `Users` node.
    read_called: bool,
}

impl MockRoot {
    fn with_text(text: &str) -> Self {
        Self {
            content: text.as_bytes().to_vec(),
            reported_size: text.len() as u64,
            present: true,
            is_dir: false,
            // The rustfs default for a created file.
            security: NodeSecurity::new(0o644, 0, 0),
            read_called: false,
        }
    }
}

impl FilesystemRead for MockRoot {
    fn root(&self) -> NodeId {
        NodeId::from_raw(ROOT)
    }

    fn node_info(&mut self, node: NodeId) -> Result<NodeInfo, DriverError> {
        match node.raw() {
            ROOT | SYSTEM | SECURITY => Ok(NodeInfo {
                kind: NodeKind::Directory,
                size: 0,
            }),
            USERS if self.present => Ok(NodeInfo {
                kind: if self.is_dir {
                    NodeKind::Directory
                } else {
                    NodeKind::RegularFile
                },
                size: self.reported_size,
            }),
            _ => Err(DriverError::NotFound),
        }
    }

    fn lookup(&mut self, dir: NodeId, name: &[u8]) -> Result<NodeId, DriverError> {
        match (dir.raw(), name) {
            (ROOT, b"System") => Ok(NodeId::from_raw(SYSTEM)),
            (SYSTEM, b"Security") => Ok(NodeId::from_raw(SECURITY)),
            (SECURITY, b"Users") if self.present => Ok(NodeId::from_raw(USERS)),
            (ROOT | SYSTEM | SECURITY, _) => Err(DriverError::NotFound),
            _ => Err(DriverError::Unsupported),
        }
    }

    fn read_at(&mut self, file: NodeId, offset: u64, buf: &mut [u8]) -> Result<usize, DriverError> {
        if file.raw() != USERS {
            return Err(DriverError::Unsupported);
        }
        self.read_called = true;
        let Ok(start) = usize::try_from(offset) else {
            return Ok(0);
        };
        if start >= self.content.len() {
            return Ok(0);
        }
        let n = core::cmp::min(buf.len(), self.content.len() - start);
        buf[..n].copy_from_slice(&self.content[start..start + n]);
        Ok(n)
    }

    fn read_dir(
        &mut self,
        _dir: NodeId,
        _index: u64,
        _name_out: &mut [u8],
    ) -> Result<Option<DirEntry>, DriverError> {
        Ok(None)
    }
}

impl FilesystemSecurity for MockRoot {
    fn security(&mut self, node: NodeId) -> Result<NodeSecurity, DriverError> {
        match node.raw() {
            ROOT | SYSTEM | SECURITY => Ok(NodeSecurity::new(0o755, 0, 0)),
            USERS if self.present => Ok(self.security),
            _ => Err(DriverError::NotFound),
        }
    }
}

/// A valid single-account `users-v1` database, serialised by the same
/// `lib/users` code the loader parses with.
fn valid_db_text() -> String {
    let salt: Salt = [0x11; SALT_LEN];
    let record = UserRecord::with_password(
        Identity {
            username: "ada",
            uid: Uid(1000),
            primary_gid: Gid(1000),
            supplementary_gids: &[],
            display_name: "Ada Lovelace",
            home: "/Users/ada",
            shell: "/System/Apps/elsh.app/Run",
            capabilities: rustos_caps::CapabilitySet::empty(),
            state: AccountState::Active,
        },
        b"correct horse",
        salt,
        MIN_ITERATIONS,
    )
    .expect("valid record");
    UsersDb::new(alloc::vec![record])
        .expect("valid db")
        .serialise()
}

#[test]
fn a_valid_database_loads_and_is_audited() {
    let sink = TestSink::new();
    let mut fs = MockRoot::with_text(&valid_db_text());

    let db = load_users_db(&mut fs, &sink).expect("valid database loads");
    assert_eq!(db.records().len(), 1);
    assert_eq!(db.records()[0].username(), "ada");

    let events = sink.snapshot();
    assert_eq!(events.len(), 1);
    assert_eq!(events[0].id.0, 4040);
    assert!(events[0]
        .fields
        .iter()
        .any(|(k, v)| k == "records" && v == "1"));
}

#[test]
fn the_source_load_holds_the_exact_text_and_authenticates() {
    // `load_users_db_source` shares the read/parse/audit path with
    // `load_users_db` but retains the canonical `users-v1` text so the
    // `users_db_read` syscall serves the exact bytes.
    let sink = TestSink::new();
    let text = valid_db_text();
    let mut fs = MockRoot::with_text(&text);

    let source = load_users_db_source(&mut fs, &sink).expect("valid database loads");

    // The held bytes are byte-for-byte the database text on disk — not a
    // re-serialisation.
    assert_eq!(source.text().expect("held text"), text.as_bytes());

    // The served text re-parses and authenticates the planted account,
    // proving the login path can use it.
    let served_text = source.text().expect("held text");
    let served = core::str::from_utf8(&served_text).expect("utf8");
    let db = UsersDb::parse(served).expect("served text re-parses");
    assert_eq!(db.records().len(), 1);
    db.authenticate("ada", b"correct horse")
        .expect("planted account authenticates");
    assert!(
        db.authenticate("ada", b"wrong").is_err(),
        "a wrong password must be refused"
    );

    // The success audit record matches `load_users_db`'s exactly.
    let events = sink.snapshot();
    assert_eq!(events.len(), 1);
    assert_eq!(events[0].id.0, 4040);
    assert!(events[0]
        .fields
        .iter()
        .any(|(k, v)| k == "records" && v == "1"));
}

#[test]
fn the_source_load_fails_closed_with_no_holder_on_a_missing_database() {
    // A missing database yields no holder and the same `4041` rejection
    // record `load_users_db` emits — login then refuses every attempt
    // rather than inventing accounts.
    let sink = TestSink::new();
    let mut fs = MockRoot::with_text(&valid_db_text());
    fs.present = false;

    let err = load_users_db_source(&mut fs, &sink).expect_err("missing file refused");
    assert_eq!(err, UsersLoadError::Vfs(VfsError::NotFound));

    let events = sink.snapshot();
    assert_eq!(events.len(), 1);
    assert_eq!(events[0].id.0, 4041);
    assert!(events[0]
        .fields
        .iter()
        .any(|(k, v)| k == "cause" && v == "not_found"));
}

#[test]
fn the_source_load_fails_closed_on_an_invalid_database() {
    // A structurally invalid database is refused by the shared parser and
    // produces no holder.
    let sink = TestSink::new();
    let mut fs = MockRoot::with_text("not-the-users-header\n");

    let err = load_users_db_source(&mut fs, &sink).expect_err("bad header refused");
    assert_eq!(err, UsersLoadError::Parse(ParseError::Header));

    let events = sink.snapshot();
    assert_eq!(events[0].id.0, 4041);
    assert!(events[0]
        .fields
        .iter()
        .any(|(k, v)| k == "cause" && v == "parse_rejected"));
}

#[test]
fn a_missing_database_is_not_found_and_audited() {
    let sink = TestSink::new();
    let mut fs = MockRoot::with_text(&valid_db_text());
    fs.present = false;

    let err = load_users_db(&mut fs, &sink).expect_err("missing file refused");
    assert_eq!(err, UsersLoadError::Vfs(VfsError::NotFound));

    let events = sink.snapshot();
    assert_eq!(events.len(), 1);
    assert_eq!(events[0].id.0, 4041);
    assert!(events[0]
        .fields
        .iter()
        .any(|(k, v)| k == "cause" && v == "not_found"));
}

#[test]
fn a_directory_at_the_database_path_is_refused() {
    let sink = TestSink::new();
    let mut fs = MockRoot::with_text(&valid_db_text());
    fs.is_dir = true;

    let err = load_users_db(&mut fs, &sink).expect_err("directory refused");
    assert_eq!(err, UsersLoadError::NotAFile);
}

#[test]
fn an_oversize_database_is_refused_before_any_byte_is_read() {
    let sink = TestSink::new();
    let mut fs = MockRoot::with_text(&valid_db_text());
    fs.reported_size = (MAX_DB_LEN as u64) + 1;

    let err = load_users_db(&mut fs, &sink).expect_err("oversize refused");
    assert_eq!(err, UsersLoadError::TooLarge);
    assert!(!fs.read_called, "no byte may be read past the size bound");
}

#[test]
fn a_short_read_is_refused() {
    let sink = TestSink::new();
    let mut fs = MockRoot::with_text(&valid_db_text());
    fs.reported_size += 8; // The driver yields fewer bytes than stat said.

    let err = load_users_db(&mut fs, &sink).expect_err("truncated read refused");
    assert_eq!(err, UsersLoadError::ShortRead);
}

#[test]
fn non_utf8_bytes_are_refused() {
    let sink = TestSink::new();
    let mut fs = MockRoot::with_text("rustos-users-v1\n");
    fs.content[0] = 0xFF;

    let err = load_users_db(&mut fs, &sink).expect_err("non-UTF-8 refused");
    assert_eq!(err, UsersLoadError::NotUtf8);
}

#[test]
fn an_invalid_database_is_refused_by_the_parser() {
    let sink = TestSink::new();
    let mut fs = MockRoot::with_text("not-the-users-header\n");

    let err = load_users_db(&mut fs, &sink).expect_err("bad header refused");
    assert_eq!(err, UsersLoadError::Parse(ParseError::Header));

    let events = sink.snapshot();
    assert_eq!(events[0].id.0, 4041);
    assert!(events[0]
        .fields
        .iter()
        .any(|(k, v)| k == "cause" && v == "parse_rejected"));
}

#[test]
fn a_database_unreadable_by_its_stored_record_is_refused() {
    let sink = TestSink::new();
    let mut fs = MockRoot::with_text(&valid_db_text());
    // Owned by uid 7, no group/other read: the kernel's uid-0 bootstrap
    // identity holds no bypass.
    fs.security = NodeSecurity::new(0o600, 7, 7);

    let err = load_users_db(&mut fs, &sink).expect_err("unreadable record refused");
    assert_eq!(err, UsersLoadError::Vfs(VfsError::PermissionDenied));
}

#[test]
fn a_capability_gated_database_is_refused_for_the_capability_less_boot_read() {
    let sink = TestSink::new();
    let mut fs = MockRoot::with_text(&valid_db_text());
    let mut sec = NodeSecurity::new(0o644, 0, 0);
    sec.required_cap = Some(CapabilityId::AUDIT_READ);
    fs.security = sec;

    let err = load_users_db(&mut fs, &sink).expect_err("capability gate refused");
    assert_eq!(err, UsersLoadError::Vfs(VfsError::PermissionDenied));

    let events = sink.snapshot();
    assert_eq!(events[0].id.0, 4041);
    assert!(events[0]
        .fields
        .iter()
        .any(|(k, v)| k == "cause" && v == "permission_denied"));
}

/// Build a `HeldUsersDbSource` over the planted valid database, sharing
/// the real load path so the held bytes are exactly what a boot read
/// produces.
fn held_source() -> crate::users::HeldUsersDbSource {
    let sink = TestSink::new();
    let mut fs = MockRoot::with_text(&valid_db_text());
    load_users_db_source(&mut fs, &sink).expect("valid database loads")
}

#[test]
fn the_late_users_db_is_pending_until_the_unlock_resolves() {
    // While the unlock is still running the cell is neither installed nor
    // resolved: `users_db_read` (which calls `text()`) returns the
    // live-but-not-ready `WouldBlock` so `login` waits without prompting,
    // leaving the console to the passphrase prompt (`plans/PI.md` P11).
    let late = LateUsersDb::new();
    assert!(!late.is_installed());
    assert!(!late.is_resolved());
    assert_eq!(late.text(), Err(Errno::WouldBlock));
}

#[test]
fn a_resolved_empty_late_users_db_fails_closed_not_implemented() {
    // Once the unlock gives up (or there is no root to unlock) the cell is
    // resolved with no database installed: `text()` flips from the pending
    // `WouldBlock` to the inert `NotImplemented`, identical to
    // `NULL_USERS_DB`, so `login` stops waiting and runs its fail-closed
    // deny-all prompt.
    let late = LateUsersDb::new();
    late.resolve();
    assert!(!late.is_installed());
    assert!(late.is_resolved());
    assert_eq!(late.text(), Err(Errno::NotImplemented));
    // Idempotent: a second resolve does not change the outcome.
    late.resolve();
    assert_eq!(late.text(), Err(Errno::NotImplemented));
}

#[test]
fn an_installed_database_wins_over_a_later_resolve() {
    // A successful unlock installs the database, and a `resolve` that the
    // shared release path also fires afterwards must not hide it: the held
    // text keeps serving so `login` authenticates against it.
    let late = LateUsersDb::new();
    let text = valid_db_text();
    late.install(held_source()).expect("first install succeeds");
    assert!(late.is_resolved());
    late.resolve();
    assert_eq!(late.text().expect("served text"), text.as_bytes());
}

#[test]
fn the_late_users_db_serves_the_installed_text() {
    // Once the unlock step publishes the loaded database the next read
    // serves its exact bytes, so login can authenticate against it.
    let late = LateUsersDb::new();
    let text = valid_db_text();
    late.install(held_source()).expect("first install succeeds");
    assert!(late.is_installed());
    assert_eq!(late.text().expect("served text"), text.as_bytes());

    let served_text = late.text().expect("served text");
    let served = core::str::from_utf8(&served_text).expect("utf8");
    let db = UsersDb::parse(served).expect("served text re-parses");
    db.authenticate("ada", b"correct horse")
        .expect("planted account authenticates against the served database");
}

#[test]
fn the_late_users_db_is_set_once_and_refuses_replacement() {
    // The credential database is immutable after the first install: a
    // second install is refused and the originally-served bytes are
    // unchanged, so no later code path can swap the live database.
    let late = LateUsersDb::new();
    let original = valid_db_text();
    late.install(held_source()).expect("first install succeeds");

    assert_eq!(
        late.install(held_source()),
        Err(UsersDbAlreadyInstalled),
        "a second install must be refused"
    );
    // The originally-installed database still serves; the rejected
    // duplicate was dropped (and its bytes zeroed) inside `install`.
    assert_eq!(late.text().expect("served text"), original.as_bytes());
}
