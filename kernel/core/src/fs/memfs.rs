//! A shared in-memory `FilesystemRead` + `FilesystemWrite` +
//! `FilesystemSecurity` driver for the `fs` module's tests.
//!
//! `RwMockFs` is a small allocation-backed filesystem implementing the same
//! `(dir, name)` mutation model the ABI defines, standing in for a
//! block-backed `drivers/filesystem/*` driver (`kernel/core` may not depend
//! on `drivers/*`). It is the one definition shared by the delegation tests
//! (`delegate_tests.rs`) and the mounted-service tests (`mounted_tests.rs`),
//! so neither carries its own copy.
//!
//! By default a created node is owned by [`ADMIN_UID`]/[`ADMIN_GID`] mode
//! `0o755` (the delegation tests vary only the node they care about); a test
//! that needs a different creator uses [`RwMockFs::with_create_owner`].

use alloc::collections::BTreeMap;
use alloc::string::{String, ToString};
use alloc::vec::Vec;

use rustos_abi::driver::filesystem::{
    DirEntry, FilesystemRead, FilesystemSecurity, FilesystemWrite, NodeId, NodeInfo, NodeKind,
    NodeSecurity,
};
use rustos_abi::driver::DriverError;

/// Default owner uid baked into a freshly created node's security record.
pub(crate) const ADMIN_UID: u32 = 1;
/// Default owning gid baked into a freshly created node's security record.
pub(crate) const ADMIN_GID: u32 = 1;

/// One node of the in-memory tree.
enum RwNode {
    /// A directory: child name → node index.
    Dir(BTreeMap<String, usize>),
    /// A regular file: its byte contents.
    File(Vec<u8>),
}

/// An in-memory read/write filesystem with a per-node security record.
pub(crate) struct RwMockFs {
    nodes: Vec<RwNode>,
    sec: Vec<NodeSecurity>,
    /// Security applied to a node created through [`FilesystemWrite::create`].
    create_uid: u32,
    create_gid: u32,
    create_mode: u32,
}

impl RwMockFs {
    /// A fresh filesystem with an empty, admin-owned, world-traversable root.
    pub(crate) fn new() -> Self {
        Self {
            nodes: alloc::vec![RwNode::Dir(BTreeMap::new())],
            // Root is admin-owned and world-traversable by default, so a test
            // can vary just the node it cares about.
            sec: alloc::vec![NodeSecurity::new(0o755, ADMIN_UID, ADMIN_GID)],
            create_uid: ADMIN_UID,
            create_gid: ADMIN_GID,
            create_mode: 0o755,
        }
    }

    /// Set the owner/mode a node created through the write surface receives,
    /// so a test can create files the resolving principal then owns.
    pub(crate) fn with_create_owner(mut self, uid: u32, gid: u32, mode: u32) -> Self {
        self.create_uid = uid;
        self.create_gid = gid;
        self.create_mode = mode;
        self
    }

    /// Overwrite the root directory's security record.
    pub(crate) fn set_root_security(&mut self, sec: NodeSecurity) {
        self.sec[0] = sec;
    }

    fn index(node: NodeId) -> Result<usize, DriverError> {
        let raw = node.raw();
        if raw == 0 {
            return Err(DriverError::NotFound);
        }
        usize::try_from(raw - 1).map_err(|_| DriverError::NotFound)
    }

    fn child_index(&self, dir: NodeId, name: &[u8]) -> Result<Option<usize>, DriverError> {
        let idx = Self::index(dir)?;
        let RwNode::Dir(children) = self.nodes.get(idx).ok_or(DriverError::NotFound)? else {
            return Err(DriverError::Unsupported);
        };
        let needle = core::str::from_utf8(name).map_err(|_| DriverError::NotFound)?;
        for (k, &v) in children {
            if k.eq_ignore_ascii_case(needle) {
                return Ok(Some(v));
            }
        }
        Ok(None)
    }

    /// Remove whatever entry in directory `dir_idx` maps to `child_idx`.
    fn unlink_index(&mut self, dir_idx: usize, child_idx: usize) {
        if let RwNode::Dir(children) = &mut self.nodes[dir_idx] {
            let key = children
                .iter()
                .find(|(_, &v)| v == child_idx)
                .map(|(k, _)| k.clone());
            if let Some(key) = key {
                children.remove(&key);
            }
        }
    }

    /// Whether `target_idx` is `root_idx` or anywhere within its subtree,
    /// used to refuse moving a directory into its own descendants.
    fn is_in_subtree(&self, root_idx: usize, target_idx: usize) -> bool {
        if root_idx == target_idx {
            return true;
        }
        if let Some(RwNode::Dir(children)) = self.nodes.get(root_idx) {
            for &child in children.values() {
                if self.is_in_subtree(child, target_idx) {
                    return true;
                }
            }
        }
        false
    }
}

impl FilesystemRead for RwMockFs {
    fn root(&self) -> NodeId {
        NodeId::from_raw(1)
    }

    fn node_info(&mut self, node: NodeId) -> Result<NodeInfo, DriverError> {
        let idx = Self::index(node)?;
        match self.nodes.get(idx).ok_or(DriverError::NotFound)? {
            RwNode::Dir(_) => Ok(NodeInfo {
                kind: NodeKind::Directory,
                size: 0,
            }),
            RwNode::File(data) => Ok(NodeInfo {
                kind: NodeKind::RegularFile,
                size: data.len() as u64,
            }),
        }
    }

    fn lookup(&mut self, dir: NodeId, name: &[u8]) -> Result<NodeId, DriverError> {
        match self.child_index(dir, name)? {
            Some(i) => Ok(NodeId::from_raw(i as u64 + 1)),
            None => Err(DriverError::NotFound),
        }
    }

    fn read_at(&mut self, file: NodeId, offset: u64, buf: &mut [u8]) -> Result<usize, DriverError> {
        let idx = Self::index(file)?;
        let RwNode::File(data) = self.nodes.get(idx).ok_or(DriverError::NotFound)? else {
            return Err(DriverError::Unsupported);
        };
        let Ok(start) = usize::try_from(offset) else {
            return Ok(0);
        };
        if start >= data.len() {
            return Ok(0);
        }
        let n = core::cmp::min(buf.len(), data.len() - start);
        buf[..n].copy_from_slice(&data[start..start + n]);
        Ok(n)
    }

    fn read_dir(
        &mut self,
        dir: NodeId,
        index: u64,
        name_out: &mut [u8],
    ) -> Result<Option<DirEntry>, DriverError> {
        let idx = Self::index(dir)?;
        let RwNode::Dir(children) = self.nodes.get(idx).ok_or(DriverError::NotFound)? else {
            return Err(DriverError::Unsupported);
        };
        let Ok(i) = usize::try_from(index) else {
            return Ok(None);
        };
        let Some((name, &child)) = children.iter().nth(i) else {
            return Ok(None);
        };
        if name_out.len() < name.len() {
            return Err(DriverError::BufferTooSmall);
        }
        name_out[..name.len()].copy_from_slice(name.as_bytes());
        let kind = match self.nodes[child] {
            RwNode::Dir(_) => NodeKind::Directory,
            RwNode::File(_) => NodeKind::RegularFile,
        };
        Ok(Some(DirEntry {
            node: NodeId::from_raw(child as u64 + 1),
            kind,
            name_len: name.len(),
        }))
    }
}

impl FilesystemWrite for RwMockFs {
    fn create(&mut self, dir: NodeId, name: &[u8], kind: NodeKind) -> Result<NodeId, DriverError> {
        if self.child_index(dir, name)?.is_some() {
            return Err(DriverError::Busy);
        }
        let name = core::str::from_utf8(name)
            .map_err(|_| DriverError::LengthOutOfRange)?
            .to_string();
        let node = match kind {
            NodeKind::Directory => RwNode::Dir(BTreeMap::new()),
            NodeKind::RegularFile => RwNode::File(Vec::new()),
        };
        let new_index = self.nodes.len();
        self.nodes.push(node);
        self.sec.push(NodeSecurity::new(
            self.create_mode,
            self.create_uid,
            self.create_gid,
        ));
        let dir_idx = Self::index(dir)?;
        if let RwNode::Dir(children) = &mut self.nodes[dir_idx] {
            children.insert(name, new_index);
        }
        Ok(NodeId::from_raw(new_index as u64 + 1))
    }

    fn write_at(
        &mut self,
        dir: NodeId,
        name: &[u8],
        offset: u64,
        data: &[u8],
    ) -> Result<usize, DriverError> {
        let child = self.child_index(dir, name)?.ok_or(DriverError::NotFound)?;
        let RwNode::File(body) = &mut self.nodes[child] else {
            return Err(DriverError::Unsupported);
        };
        let start = usize::try_from(offset).map_err(|_| DriverError::LengthOutOfRange)?;
        let end = start
            .checked_add(data.len())
            .ok_or(DriverError::LengthOutOfRange)?;
        if body.len() < end {
            body.resize(end, 0);
        }
        body[start..end].copy_from_slice(data);
        Ok(data.len())
    }

    fn truncate(&mut self, dir: NodeId, name: &[u8], size: u64) -> Result<(), DriverError> {
        let child = self.child_index(dir, name)?.ok_or(DriverError::NotFound)?;
        let RwNode::File(body) = &mut self.nodes[child] else {
            return Err(DriverError::Unsupported);
        };
        let new = usize::try_from(size).map_err(|_| DriverError::LengthOutOfRange)?;
        body.resize(new, 0);
        Ok(())
    }

    fn remove(&mut self, dir: NodeId, name: &[u8]) -> Result<(), DriverError> {
        let child = self.child_index(dir, name)?.ok_or(DriverError::NotFound)?;
        if let RwNode::Dir(children) = &self.nodes[child] {
            if !children.is_empty() {
                return Err(DriverError::Busy);
            }
        }
        let dir_idx = Self::index(dir)?;
        self.unlink_index(dir_idx, child);
        Ok(())
    }

    fn rename(
        &mut self,
        src_dir: NodeId,
        src_name: &[u8],
        dst_dir: NodeId,
        dst_name: &[u8],
    ) -> Result<(), DriverError> {
        let src_idx = self
            .child_index(src_dir, src_name)?
            .ok_or(DriverError::NotFound)?;
        let dst_dir_idx = Self::index(dst_dir)?;
        if !matches!(self.nodes.get(dst_dir_idx), Some(RwNode::Dir(_))) {
            return Err(DriverError::Unsupported);
        }
        let dst_key = core::str::from_utf8(dst_name)
            .map_err(|_| DriverError::LengthOutOfRange)?
            .to_string();
        let moving_dir = matches!(self.nodes[src_idx], RwNode::Dir(_));

        // Refuse moving a directory into itself or its own subtree.
        if moving_dir && self.is_in_subtree(src_idx, dst_dir_idx) {
            return Err(DriverError::Busy);
        }

        // Replace an existing destination of a compatible kind.
        if let Some(dst_idx) = self.child_index(dst_dir, dst_name)? {
            if dst_idx == src_idx {
                return Ok(());
            }
            let dst_is_dir = matches!(self.nodes[dst_idx], RwNode::Dir(_));
            if dst_is_dir != moving_dir {
                return Err(DriverError::Unsupported);
            }
            if let RwNode::Dir(children) = &self.nodes[dst_idx] {
                if !children.is_empty() {
                    return Err(DriverError::Busy);
                }
            }
            self.unlink_index(dst_dir_idx, dst_idx);
        }

        // Detach the source name and attach the node under the new name.
        let src_dir_idx = Self::index(src_dir)?;
        self.unlink_index(src_dir_idx, src_idx);
        if let RwNode::Dir(children) = &mut self.nodes[dst_dir_idx] {
            children.insert(dst_key, src_idx);
        }
        Ok(())
    }

    fn flush(&mut self) -> Result<(), DriverError> {
        Ok(())
    }
}

impl FilesystemSecurity for RwMockFs {
    fn security(&mut self, node: NodeId) -> Result<NodeSecurity, DriverError> {
        let idx = Self::index(node)?;
        self.sec.get(idx).copied().ok_or(DriverError::NotFound)
    }
}
