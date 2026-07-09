//! Shared host-test fixtures for the boot-path filesystem readers.
//!
//! [`MockRootFs`] is a minimal in-memory root-volume filesystem driver
//! serving a directory tree plus file contents — exactly the
//! [`FilesystemRead`] + [`FilesystemSecurity`] surface the kernel's
//! root-backed VFS delegation walks ([`rustos_kernel_core::DriverImageReader`],
//! [`rustos_kernel_core::enumerate_driver_store`]). Both the
//! [`crate::system_files`] service tests and the
//! [`crate::driver_autoload`] composition tests delegate through it, so it is
//! defined once here rather than copy-pasted into each module.

use alloc::collections::BTreeMap;
use alloc::string::{String, ToString};
use alloc::vec::Vec;

use rustos_abi::driver::filesystem::{
    DirEntry, FilesystemRead, FilesystemSecurity, NodeId, NodeInfo, NodeKind, NodeSecurity,
};
use rustos_abi::driver::DriverError;

/// Fixed node id of the mock volume's root directory.
const ROOT_ID: u64 = 1;

/// One node of the in-memory tree: a directory (with named children) or a
/// regular file (with byte contents), plus its security record.
struct Node {
    kind: NodeKind,
    children: Vec<(String, u64)>,
    content: Vec<u8>,
    security: NodeSecurity,
}

/// A minimal in-memory root-volume filesystem driver for host tests.
///
/// Build one with [`MockRootFs::new`], populate it with
/// [`MockRootFs::add_file`] (intermediate directories are created on
/// demand), then hand `&mut fs` to any reader that consumes a
/// [`FilesystemRead`] + [`FilesystemSecurity`] root volume.
pub struct MockRootFs {
    nodes: BTreeMap<u64, Node>,
    next: u64,
}

impl MockRootFs {
    /// A fresh volume holding only an empty, world-searchable root
    /// directory (`0o755`, uid/gid 0).
    pub fn new() -> Self {
        let mut nodes = BTreeMap::new();
        nodes.insert(
            ROOT_ID,
            Node {
                kind: NodeKind::Directory,
                children: Vec::new(),
                content: Vec::new(),
                security: NodeSecurity::new(0o755, 0, 0),
            },
        );
        Self {
            nodes,
            next: ROOT_ID + 1,
        }
    }

    /// Look up the child node id of `name` directly under directory `dir`.
    fn child(&self, dir: u64, name: &str) -> Option<u64> {
        self.nodes
            .get(&dir)?
            .children
            .iter()
            .find(|(n, _)| n == name)
            .map(|(_, id)| *id)
    }

    /// Ensure every directory in `comps` exists under the root, creating
    /// the missing levels, and return the id of the deepest one.
    fn ensure_dirs(&mut self, comps: &[&str]) -> u64 {
        let mut cur = ROOT_ID;
        for &c in comps {
            cur = if let Some(id) = self.child(cur, c) {
                id
            } else {
                let id = self.next;
                self.next += 1;
                self.nodes.insert(
                    id,
                    Node {
                        kind: NodeKind::Directory,
                        children: Vec::new(),
                        content: Vec::new(),
                        security: NodeSecurity::new(0o755, 0, 0),
                    },
                );
                self.nodes
                    .get_mut(&cur)
                    .expect("parent")
                    .children
                    .push((c.to_string(), id));
                id
            };
        }
        cur
    }

    /// Add a regular file at the absolute `path` with `content`, creating
    /// intermediate directories (`0o644`, uid/gid 0).
    pub fn add_file(&mut self, path: &str, content: &[u8]) {
        let comps: Vec<&str> = path.trim_start_matches('/').split('/').collect();
        let (name, dirs) = comps.split_last().expect("non-empty path");
        let parent = self.ensure_dirs(dirs);
        let id = self.next;
        self.next += 1;
        self.nodes.insert(
            id,
            Node {
                kind: NodeKind::RegularFile,
                children: Vec::new(),
                content: content.to_vec(),
                security: NodeSecurity::new(0o644, 0, 0),
            },
        );
        self.nodes
            .get_mut(&parent)
            .expect("parent")
            .children
            .push(((*name).to_string(), id));
    }
}

impl FilesystemRead for MockRootFs {
    fn root(&self) -> NodeId {
        NodeId::from_raw(ROOT_ID)
    }

    fn node_info(&mut self, node: NodeId) -> Result<NodeInfo, DriverError> {
        let n = self.nodes.get(&node.raw()).ok_or(DriverError::NotFound)?;
        Ok(NodeInfo {
            kind: n.kind,
            size: n.content.len() as u64,
            allocated: n.content.len() as u64,
        })
    }

    fn lookup(&mut self, dir: NodeId, name: &[u8]) -> Result<NodeId, DriverError> {
        let name = core::str::from_utf8(name).map_err(|_| DriverError::NotFound)?;
        self.child(dir.raw(), name)
            .map(NodeId::from_raw)
            .ok_or(DriverError::NotFound)
    }

    fn read_at(&mut self, file: NodeId, offset: u64, buf: &mut [u8]) -> Result<usize, DriverError> {
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
        cursor: u64,
        name_out: &mut [u8],
    ) -> Result<Option<DirEntry>, DriverError> {
        let n = self.nodes.get(&dir.raw()).ok_or(DriverError::NotFound)?;
        let Ok(index) = usize::try_from(cursor) else {
            return Ok(None);
        };
        let Some((name, child_id)) = n.children.get(index) else {
            return Ok(None);
        };
        let bytes = name.as_bytes();
        if bytes.len() > name_out.len() {
            return Err(DriverError::LengthOutOfRange);
        }
        let name_len = bytes.len();
        name_out[..name_len].copy_from_slice(bytes);
        let child_id = *child_id;
        let info = self.node_info(NodeId::from_raw(child_id))?;
        Ok(Some(DirEntry {
            node: NodeId::from_raw(child_id),
            info,
            modified: rustos_abi::time::Time64::UNIX_EPOCH,
            name_len,
            next_cursor: cursor + 1,
        }))
    }
}

impl FilesystemSecurity for MockRootFs {
    fn security(&mut self, node: NodeId) -> Result<NodeSecurity, DriverError> {
        self.nodes
            .get(&node.raw())
            .map(|n| n.security)
            .ok_or(DriverError::NotFound)
    }

    fn set_security(&mut self, node: NodeId, security: NodeSecurity) -> Result<(), DriverError> {
        match self.nodes.get_mut(&node.raw()) {
            Some(stored) => {
                stored.security = security;
                Ok(())
            }
            None => Err(DriverError::NotFound),
        }
    }
}
