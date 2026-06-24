//! Behavioural tests for the boot-time driver-store enumeration
//! ([`crate::driver_store::enumerate_driver_store`]): the nested-store
//! success path, the fail-closed refusals, the validation bounds, and the
//! single audit record.

use alloc::collections::BTreeMap;
use alloc::string::{String, ToString};
use alloc::vec::Vec;

use rustos_abi::driver::filesystem::{
    DirEntry, FilesystemRead, FilesystemSecurity, NodeId, NodeInfo, NodeKind, NodeSecurity,
};
use rustos_abi::driver::DriverError;

use crate::driver_store::{
    enumerate_driver_store, DriverImageError, DriverImageReader, DRIVER_STORE_PATH,
    MAX_DRIVER_IMAGE_LEN, MAX_STORE_DEPTH, MAX_STORE_DRIVERS,
};
use crate::fs::VfsError;
use crate::test_sink::TestSink;
use rustos_abi::Errno;

const ROOT_ID: u64 = 1;

/// One node in the mock root volume's tree.
struct MockNode {
    kind: NodeKind,
    /// Child `(name, node-id)` pairs, in stable enumeration order.
    children: Vec<(String, u64)>,
    /// File contents (empty for directories); also the reported size
    /// unless [`MockNode::reported_size`] overrides it.
    content: Vec<u8>,
    /// An explicit `stat` size overriding `content.len()` (models a
    /// short-read driver whose stated size exceeds its readable bytes).
    reported_size: Option<u64>,
    /// The record the driver reports for the node.
    security: NodeSecurity,
}

impl MockNode {
    fn dir() -> Self {
        Self {
            kind: NodeKind::Directory,
            children: Vec::new(),
            content: Vec::new(),
            reported_size: None,
            // Searchable + readable by the uid-0 boot identity.
            security: NodeSecurity::new(0o755, 0, 0),
        }
    }

    fn file() -> Self {
        Self {
            kind: NodeKind::RegularFile,
            children: Vec::new(),
            content: Vec::new(),
            reported_size: None,
            security: NodeSecurity::new(0o644, 0, 0),
        }
    }
}

/// A mock root-volume driver presenting an arbitrary directory tree,
/// mirroring the structural surface rustfs reports for the
/// mkimage-authored root.
struct MockStore {
    nodes: BTreeMap<u64, MockNode>,
    next: u64,
}

impl MockStore {
    /// A store with the four top-level dirs absent except `/System`
    /// — only the root exists until [`Self::add`] builds paths.
    fn new() -> Self {
        let mut nodes = BTreeMap::new();
        nodes.insert(ROOT_ID, MockNode::dir());
        Self {
            nodes,
            next: ROOT_ID + 1,
        }
    }

    fn alloc(&mut self, node: MockNode) -> u64 {
        let id = self.next;
        self.next += 1;
        self.nodes.insert(id, node);
        id
    }

    /// Resolve `name` under `dir`, returning the child id if present.
    fn child(&self, dir: u64, name: &str) -> Option<u64> {
        self.nodes
            .get(&dir)?
            .children
            .iter()
            .find(|(n, _)| n == name)
            .map(|(_, id)| *id)
    }

    /// Ensure a directory chain exists, returning the deepest dir's id.
    fn ensure_dirs(&mut self, components: &[&str]) -> u64 {
        let mut cur = ROOT_ID;
        for &comp in components {
            cur = if let Some(id) = self.child(cur, comp) {
                id
            } else {
                let id = self.alloc(MockNode::dir());
                self.nodes
                    .get_mut(&cur)
                    .expect("parent exists")
                    .children
                    .push((comp.to_string(), id));
                id
            };
        }
        cur
    }

    /// Add a regular file at an absolute path, creating intermediate dirs.
    fn add_file(&mut self, path: &str) {
        self.add_file_with(path, &[]);
    }

    /// Add a regular file with `content` at an absolute path, returning its
    /// node id so a test can adjust its security record or size.
    fn add_file_with(&mut self, path: &str, content: &[u8]) -> u64 {
        let comps: Vec<&str> = path.trim_start_matches('/').split('/').collect();
        let (name, dirs) = comps.split_last().expect("non-empty path");
        let parent = self.ensure_dirs(dirs);
        let mut node = MockNode::file();
        node.content = content.to_vec();
        let id = self.alloc(node);
        self.nodes
            .get_mut(&parent)
            .expect("parent exists")
            .children
            .push(((*name).to_string(), id));
        id
    }

    /// Overwrite the size a node reports without changing its bytes — used
    /// to model a driver whose `stat` size and `read` byte count disagree
    /// (a short read).
    fn set_reported_size(&mut self, id: u64, size: u64) {
        self.nodes.get_mut(&id).expect("node exists").reported_size = Some(size);
    }

    /// Add a directory at an absolute path (creating intermediates),
    /// returning its id so a test can adjust its security record.
    fn add_dir(&mut self, path: &str) -> u64 {
        let comps: Vec<&str> = path.trim_start_matches('/').split('/').collect();
        self.ensure_dirs(&comps)
    }

    fn set_security(&mut self, id: u64, security: NodeSecurity) {
        self.nodes.get_mut(&id).expect("node exists").security = security;
    }
}

impl FilesystemRead for MockStore {
    fn root(&self) -> NodeId {
        NodeId::from_raw(ROOT_ID)
    }

    fn node_info(&mut self, node: NodeId) -> Result<NodeInfo, DriverError> {
        let n = self.nodes.get(&node.raw()).ok_or(DriverError::NotFound)?;
        Ok(NodeInfo {
            kind: n.kind,
            size: n.reported_size.unwrap_or(n.content.len() as u64),
        })
    }

    fn lookup(&mut self, dir: NodeId, name: &[u8]) -> Result<NodeId, DriverError> {
        let name = core::str::from_utf8(name).map_err(|_| DriverError::NotFound)?;
        match self.child(dir.raw(), name) {
            Some(id) => Ok(NodeId::from_raw(id)),
            None => Err(DriverError::NotFound),
        }
    }

    fn read_at(&mut self, file: NodeId, offset: u64, buf: &mut [u8]) -> Result<usize, DriverError> {
        // The enumeration never reads file bytes (the
        // load gate does), but `DriverImageReader::read_image` does, so the
        // mock serves the node's content from `offset`.
        let n = self.nodes.get(&file.raw()).ok_or(DriverError::NotFound)?;
        let Ok(offset) = usize::try_from(offset) else {
            return Ok(0);
        };
        if offset >= n.content.len() {
            return Ok(0);
        }
        let avail = &n.content[offset..];
        let take = avail.len().min(buf.len());
        buf[..take].copy_from_slice(&avail[..take]);
        Ok(take)
    }

    fn read_dir(
        &mut self,
        dir: NodeId,
        index: u64,
        name_out: &mut [u8],
    ) -> Result<Option<DirEntry>, DriverError> {
        let n = self.nodes.get(&dir.raw()).ok_or(DriverError::NotFound)?;
        let Ok(index) = usize::try_from(index) else {
            return Ok(None);
        };
        let Some((name, child_id)) = n.children.get(index) else {
            return Ok(None);
        };
        let bytes = name.as_bytes();
        if bytes.len() > name_out.len() {
            return Err(DriverError::LengthOutOfRange);
        }
        name_out[..bytes.len()].copy_from_slice(bytes);
        let kind = self.nodes.get(child_id).ok_or(DriverError::NotFound)?.kind;
        Ok(Some(DirEntry {
            node: NodeId::from_raw(*child_id),
            kind,
            name_len: bytes.len(),
        }))
    }
}

impl FilesystemSecurity for MockStore {
    fn security(&mut self, node: NodeId) -> Result<NodeSecurity, DriverError> {
        self.nodes
            .get(&node.raw())
            .map(|n| n.security)
            .ok_or(DriverError::NotFound)
    }
}

fn scanned_record(sink: &TestSink) -> (usize, usize) {
    let events = sink.snapshot();
    assert_eq!(events.len(), 1, "exactly one scan record is emitted");
    let event = &events[0];
    assert_eq!(event.id.0, 4042);
    let field = |key: &str| -> usize {
        event
            .fields
            .iter()
            .find(|(k, _)| k == key)
            .and_then(|(_, v)| v.parse().ok())
            .unwrap_or_else(|| panic!("field {key} present and numeric"))
    };
    (field("drivers"), field("skipped"))
}

#[test]
fn a_nested_store_enumerates_every_regular_file_in_order() {
    let mut fs = MockStore::new();
    // The chain drivers, organised `<class>[/<vendor>]/<driver>`.
    fs.add_file("/System/Drivers/bus_usb");
    fs.add_file("/System/Drivers/pcie/brcm/bcm2711");
    fs.add_file("/System/Drivers/usb_kbd");

    let sink = TestSink::new();
    let drivers = enumerate_driver_store(&mut fs, DRIVER_STORE_PATH, &sink);

    assert_eq!(
        drivers,
        alloc::vec![
            "/System/Drivers/bus_usb".to_string(),
            "/System/Drivers/pcie/brcm/bcm2711".to_string(),
            "/System/Drivers/usb_kbd".to_string(),
        ]
    );
    assert_eq!(scanned_record(&sink), (3, 0));
}

#[test]
fn a_missing_store_is_not_an_error_and_yields_nothing() {
    // `/System` exists but `/System/Drivers` does not — a driverless
    // install autoloads nothing, never an error.
    let mut fs = MockStore::new();
    fs.add_dir("/System");

    let sink = TestSink::new();
    let drivers = enumerate_driver_store(&mut fs, DRIVER_STORE_PATH, &sink);

    assert!(drivers.is_empty());
    // The absent store root is the legitimate "no drivers" case: not a
    // skipped entry.
    assert_eq!(scanned_record(&sink), (0, 0));
}

#[test]
fn an_entirely_absent_system_tree_yields_nothing() {
    // Not even `/System` exists; the store-root listing simply fails and
    // the scan is empty.
    let mut fs = MockStore::new();

    let sink = TestSink::new();
    let drivers = enumerate_driver_store(&mut fs, DRIVER_STORE_PATH, &sink);

    assert!(drivers.is_empty());
    assert_eq!(scanned_record(&sink), (0, 0));
}

#[test]
fn an_unreadable_subdirectory_is_skipped_and_the_walk_continues() {
    let mut fs = MockStore::new();
    fs.add_file("/System/Drivers/bus_usb");
    let private = fs.add_dir("/System/Drivers/private");
    fs.add_file("/System/Drivers/private/secret");
    // The subdir is owned by another user with no group/other access: the
    // uid-0 boot identity cannot list it (no bypass).
    fs.set_security(private, NodeSecurity::new(0o700, 7, 7));

    let sink = TestSink::new();
    let drivers = enumerate_driver_store(&mut fs, DRIVER_STORE_PATH, &sink);

    // The readable driver is found; the unreadable subtree is skipped.
    assert_eq!(drivers, alloc::vec!["/System/Drivers/bus_usb".to_string()]);
    assert_eq!(scanned_record(&sink), (1, 1));
}

#[test]
fn a_node_below_the_depth_bound_is_refused() {
    let mut fs = MockStore::new();
    // Build a directory chain `MAX_STORE_DEPTH + 1` levels below the store,
    // with a regular file at the bottom that the bound must exclude.
    let mut path = String::from("/System/Drivers");
    for level in 0..=MAX_STORE_DEPTH {
        path.push_str("/d");
        path.push_str(&level.to_string());
    }
    path.push_str("/too_deep");
    fs.add_file(&path);
    // A reachable driver at depth 0 to prove the walk still produced output.
    fs.add_file("/System/Drivers/shallow");

    let sink = TestSink::new();
    let drivers = enumerate_driver_store(&mut fs, DRIVER_STORE_PATH, &sink);

    assert_eq!(drivers, alloc::vec!["/System/Drivers/shallow".to_string()]);
    let (found, skipped) = scanned_record(&sink);
    assert_eq!(found, 1);
    assert!(skipped >= 1, "the over-deep directory is refused");
}

#[test]
fn the_driver_count_is_bounded() {
    let mut fs = MockStore::new();
    let total = MAX_STORE_DRIVERS + 5;
    for i in 0..total {
        fs.add_file(&alloc::format!("/System/Drivers/drv{i:04}"));
    }

    let sink = TestSink::new();
    let drivers = enumerate_driver_store(&mut fs, DRIVER_STORE_PATH, &sink);

    assert_eq!(drivers.len(), MAX_STORE_DRIVERS);
    let (found, skipped) = scanned_record(&sink);
    assert_eq!(found, MAX_STORE_DRIVERS);
    assert_eq!(skipped, total - MAX_STORE_DRIVERS);
}

#[test]
fn an_empty_store_directory_yields_nothing() {
    let mut fs = MockStore::new();
    // `/System/Drivers` exists but is empty.
    fs.add_dir("/System/Drivers");

    let sink = TestSink::new();
    let drivers = enumerate_driver_store(&mut fs, DRIVER_STORE_PATH, &sink);

    assert!(drivers.is_empty());
    assert_eq!(scanned_record(&sink), (0, 0));
}

// --- DriverImageReader -------------------------------------------------

#[test]
fn read_image_returns_a_bundle_byte_for_byte() {
    let mut fs = MockStore::new();
    let bytes = b"\x7fELF-ish driver bundle";
    fs.add_file_with("/System/Drivers/usb_kbd", bytes);

    let reader = DriverImageReader::open().expect("root mount builds");
    let mut buf = Vec::new();
    reader
        .read_image(
            &mut fs,
            DRIVER_STORE_PATH,
            "/System/Drivers/usb_kbd",
            &mut buf,
        )
        .expect("a readable in-store file");

    assert_eq!(buf.as_slice(), bytes.as_slice());
}

#[test]
fn read_image_appends_rather_than_overwrites() {
    // The `ImageSource` contract appends to a (pre-cleared, pre-sized)
    // buffer; prove a non-empty prefix is preserved.
    let mut fs = MockStore::new();
    fs.add_file_with("/System/Drivers/bus_usb", b"BODY");

    let reader = DriverImageReader::open().expect("root mount builds");
    let mut buf = alloc::vec![0xAAu8, 0xBB];
    reader
        .read_image(
            &mut fs,
            DRIVER_STORE_PATH,
            "/System/Drivers/bus_usb",
            &mut buf,
        )
        .expect("a readable in-store file");

    assert_eq!(buf, alloc::vec![0xAA, 0xBB, b'B', b'O', b'D', b'Y']);
}

#[test]
fn read_image_reads_an_empty_bundle_as_zero_bytes() {
    // An empty file is read as zero bytes (the load gate rejects it as
    // truncated later — not the reader's job).
    let mut fs = MockStore::new();
    fs.add_file_with("/System/Drivers/empty", &[]);

    let reader = DriverImageReader::open().expect("root mount builds");
    let mut buf = Vec::new();
    reader
        .read_image(
            &mut fs,
            DRIVER_STORE_PATH,
            "/System/Drivers/empty",
            &mut buf,
        )
        .expect("an empty in-store file reads cleanly");

    assert!(buf.is_empty());
}

#[test]
fn read_image_refuses_a_path_outside_the_store() {
    let mut fs = MockStore::new();
    fs.add_file_with("/System/Security/Users", b"secret");

    let reader = DriverImageReader::open().expect("root mount builds");
    let mut buf = Vec::new();
    // A path outside `/System/Drivers/` is refused before any fs access: the reader only ever reads driver bundles.
    assert_eq!(
        reader.read_image(
            &mut fs,
            DRIVER_STORE_PATH,
            "/System/Security/Users",
            &mut buf
        ),
        Err(DriverImageError::OutsideStore)
    );
    assert!(buf.is_empty());
}

#[test]
fn read_image_refuses_the_store_directory_itself() {
    let mut fs = MockStore::new();
    fs.add_dir("/System/Drivers");

    let reader = DriverImageReader::open().expect("root mount builds");
    let mut buf = Vec::new();
    // The store directory is not strictly *below* the store.
    assert_eq!(
        reader.read_image(&mut fs, DRIVER_STORE_PATH, "/System/Drivers", &mut buf),
        Err(DriverImageError::OutsideStore)
    );
    // A sibling whose name merely shares the prefix is also refused.
    assert_eq!(
        reader.read_image(
            &mut fs,
            DRIVER_STORE_PATH,
            "/System/DriversExtra/x",
            &mut buf
        ),
        Err(DriverImageError::OutsideStore)
    );
}

#[test]
fn read_image_reports_a_missing_bundle_as_not_found() {
    let mut fs = MockStore::new();
    fs.add_dir("/System/Drivers");

    let reader = DriverImageReader::open().expect("root mount builds");
    let mut buf = Vec::new();
    assert_eq!(
        reader.read_image(
            &mut fs,
            DRIVER_STORE_PATH,
            "/System/Drivers/absent",
            &mut buf
        ),
        Err(DriverImageError::Vfs(VfsError::NotFound))
    );
    assert!(buf.is_empty());
}

#[test]
fn read_image_refuses_a_directory() {
    let mut fs = MockStore::new();
    fs.add_dir("/System/Drivers/display");

    let reader = DriverImageReader::open().expect("root mount builds");
    let mut buf = Vec::new();
    assert_eq!(
        reader.read_image(
            &mut fs,
            DRIVER_STORE_PATH,
            "/System/Drivers/display",
            &mut buf
        ),
        Err(DriverImageError::NotAFile)
    );
}

#[test]
fn read_image_refuses_an_oversized_bundle_before_reading() {
    let mut fs = MockStore::new();
    // A file whose *stated* size exceeds the validation bound: refused
    // before a single byte (and before any large allocation).
    let id = fs.add_file_with("/System/Drivers/huge", b"small actual body");
    fs.set_reported_size(id, MAX_DRIVER_IMAGE_LEN as u64 + 1);

    let reader = DriverImageReader::open().expect("root mount builds");
    let mut buf = Vec::new();
    assert_eq!(
        reader.read_image(&mut fs, DRIVER_STORE_PATH, "/System/Drivers/huge", &mut buf),
        Err(DriverImageError::TooLarge)
    );
    assert!(buf.is_empty());
}

#[test]
fn read_image_refuses_a_bundle_the_boot_identity_may_not_read() {
    let mut fs = MockStore::new();
    let id = fs.add_file_with("/System/Drivers/guarded", b"body");
    // Owned by another user, no group/other read: the uid-0 boot identity
    // is refused (no bypass).
    fs.set_security(id, NodeSecurity::new(0o600, 7, 7));

    let reader = DriverImageReader::open().expect("root mount builds");
    let mut buf = Vec::new();
    assert_eq!(
        reader.read_image(
            &mut fs,
            DRIVER_STORE_PATH,
            "/System/Drivers/guarded",
            &mut buf
        ),
        Err(DriverImageError::Vfs(VfsError::PermissionDenied))
    );
    assert!(buf.is_empty());
}

#[test]
fn read_image_unwinds_the_buffer_on_a_short_read() {
    let mut fs = MockStore::new();
    // `stat` claims more bytes than `read_at` can serve (a short read);
    // the partial bytes are discarded and `buf` is left at entry length.
    let id = fs.add_file_with("/System/Drivers/torn", b"only four");
    fs.set_reported_size(id, 9999);

    let reader = DriverImageReader::open().expect("root mount builds");
    let mut buf = alloc::vec![0x11u8];
    assert_eq!(
        reader.read_image(&mut fs, DRIVER_STORE_PATH, "/System/Drivers/torn", &mut buf),
        Err(DriverImageError::ShortRead)
    );
    // The prefix survives; nothing partial is left behind.
    assert_eq!(buf, alloc::vec![0x11u8]);
}

#[test]
fn driver_image_error_errno_mapping_is_stable() {
    assert_eq!(
        DriverImageError::OutsideStore.to_errno(),
        Errno::PermissionDenied
    );
    assert_eq!(DriverImageError::NotAFile.to_errno(), Errno::OutOfRange);
    assert_eq!(
        DriverImageError::TooLarge.to_errno(),
        Errno::LengthOutOfRange
    );
    assert_eq!(
        DriverImageError::ShortRead.to_errno(),
        Errno::NotImplemented
    );
    assert_eq!(
        DriverImageError::Vfs(VfsError::NotFound).to_errno(),
        Errno::NotFound
    );
    assert_eq!(
        DriverImageError::Vfs(VfsError::PermissionDenied).to_errno(),
        Errno::PermissionDenied
    );
}
