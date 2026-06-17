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

use crate::driver_store::{enumerate_driver_store, MAX_STORE_DEPTH, MAX_STORE_DRIVERS};
use crate::test_sink::TestSink;

const ROOT_ID: u64 = 1;

/// One node in the mock root volume's tree.
struct MockNode {
    kind: NodeKind,
    /// Child `(name, node-id)` pairs, in stable enumeration order.
    children: Vec<(String, u64)>,
    /// The §5.3 record the driver reports for the node.
    security: NodeSecurity,
}

impl MockNode {
    fn dir() -> Self {
        Self {
            kind: NodeKind::Directory,
            children: Vec::new(),
            // Searchable + readable by the uid-0 boot identity.
            security: NodeSecurity::new(0o755, 0, 0),
        }
    }

    fn file() -> Self {
        Self {
            kind: NodeKind::RegularFile,
            children: Vec::new(),
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
    /// A store with the four §16.1 top-level dirs absent except `/System`
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
        let comps: Vec<&str> = path.trim_start_matches('/').split('/').collect();
        let (name, dirs) = comps.split_last().expect("non-empty path");
        let parent = self.ensure_dirs(dirs);
        let id = self.alloc(MockNode::file());
        self.nodes
            .get_mut(&parent)
            .expect("parent exists")
            .children
            .push(((*name).to_string(), id));
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
            size: 0,
        })
    }

    fn lookup(&mut self, dir: NodeId, name: &[u8]) -> Result<NodeId, DriverError> {
        let name = core::str::from_utf8(name).map_err(|_| DriverError::NotFound)?;
        match self.child(dir.raw(), name) {
            Some(id) => Ok(NodeId::from_raw(id)),
            None => Err(DriverError::NotFound),
        }
    }

    fn read_at(
        &mut self,
        _file: NodeId,
        _offset: u64,
        _buf: &mut [u8],
    ) -> Result<usize, DriverError> {
        // The enumeration never reads file bytes (`AGENTS.md` §18.6 — the
        // load gate does); this is unreachable from the walk.
        Err(DriverError::Unsupported)
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
    // The chain drivers, organised `<class>[/<vendor>]/<driver>` (§16.2).
    fs.add_file("/System/Drivers/bus_usb");
    fs.add_file("/System/Drivers/pcie/brcm/bcm2711");
    fs.add_file("/System/Drivers/usb_kbd");

    let sink = TestSink::new();
    let drivers = enumerate_driver_store(&mut fs, &sink);

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
    // install autoloads nothing (`AGENTS.md` §18.4), never an error.
    let mut fs = MockStore::new();
    fs.add_dir("/System");

    let sink = TestSink::new();
    let drivers = enumerate_driver_store(&mut fs, &sink);

    assert!(drivers.is_empty());
    // The absent store root is the legitimate "no drivers" case: not a
    // skipped entry.
    assert_eq!(scanned_record(&sink), (0, 0));
}

#[test]
fn an_entirely_absent_system_tree_yields_nothing() {
    // Not even `/System` exists; the store-root listing simply fails and
    // the scan is empty (`AGENTS.md` §18.4).
    let mut fs = MockStore::new();

    let sink = TestSink::new();
    let drivers = enumerate_driver_store(&mut fs, &sink);

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
    // uid-0 boot identity cannot list it (no §5.1 bypass).
    fs.set_security(private, NodeSecurity::new(0o700, 7, 7));

    let sink = TestSink::new();
    let drivers = enumerate_driver_store(&mut fs, &sink);

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
    let drivers = enumerate_driver_store(&mut fs, &sink);

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
    let drivers = enumerate_driver_store(&mut fs, &sink);

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
    let drivers = enumerate_driver_store(&mut fs, &sink);

    assert!(drivers.is_empty());
    assert_eq!(scanned_record(&sink), (0, 0));
}
