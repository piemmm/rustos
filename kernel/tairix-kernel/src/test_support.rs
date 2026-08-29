//! Shared host-test fixtures for the boot-path filesystem readers and the
//! mounted-volume paths above them.
//!
//! [`RamBlock`] is the `Vec`-backed 512-byte-block device every host test
//! that needs a real on-disk image formats over — the runtime volume
//! attach/detach scenarios and the write-back flusher alike — so there is one
//! definition rather than a copy per test module.
//!
//! [`MockRootFs`] is a minimal in-memory root-volume filesystem driver
//! serving a directory tree plus file contents — exactly the
//! [`FilesystemRead`] + [`FilesystemSecurity`] surface the kernel's
//! root-backed VFS delegation walks ([`tairix_kernel_core::DriverImageReader`],
//! [`tairix_kernel_core::enumerate_driver_store`]). Both the
//! [`crate::system_files`] service tests and the
//! [`crate::driver_autoload`] composition tests delegate through it, so it is
//! defined once here rather than copy-pasted into each module.

use alloc::collections::BTreeMap;
use alloc::string::{String, ToString};
use alloc::vec::Vec;

use alloc::sync::Arc;

use tairix_abi::blkio::BlkDeviceClass;
use tairix_abi::driver::block::{Block, BlockGeometry};
use tairix_abi::driver::filesystem::{
    DirEntry, FilesystemRead, FilesystemSecurity, NodeId, NodeInfo, NodeKind, NodeSecurity,
    NodeTimes,
};
use tairix_abi::driver::DriverError;
use tairix_sync::SpinLock;

/// Block size every [`RamBlock`] fixture serves.
pub const BLOCK_SIZE: usize = 512;

/// [`BLOCK_SIZE`] as the geometry reply's field type.
pub const BLOCK_SIZE_U32: u32 = 512;

/// A `Vec`-backed 512-byte-block device.
///
/// It declares the removable medium: the scenarios that use it attach a
/// stick, and the write-back window a mounted volume takes is a property of
/// that class, so a test that reports one can only have learned it here.
pub struct RamBlock {
    /// The image bytes, so a test can seed or inspect them directly.
    pub data: Vec<u8>,
}

impl RamBlock {
    /// A zeroed device of `sectors` [`BLOCK_SIZE`]-byte blocks.
    #[must_use]
    pub fn new(sectors: u64) -> Self {
        Self {
            data: alloc::vec![0u8; usize::try_from(sectors).expect("fits") * BLOCK_SIZE],
        }
    }
}

/// A handle onto one shared [`RamBlock`], so a test can read the image back
/// while a live driver still holds the device.
///
/// Pure forwarding over the one device definition: the point is the shared
/// ownership, not a second block backing.
pub struct SharedRamBlock(Arc<SpinLock<RamBlock>>);

impl SharedRamBlock {
    /// A zeroed device of `sectors` blocks, and a second handle on the same
    /// image for the test to read.
    #[must_use]
    pub fn new(sectors: u64) -> (Self, Arc<SpinLock<RamBlock>>) {
        let shared = Arc::new(SpinLock::new(RamBlock::new(sectors)));
        (Self(Arc::clone(&shared)), shared)
    }
}

impl Block for SharedRamBlock {
    fn device_class(&self) -> BlkDeviceClass {
        self.0.lock().device_class()
    }

    fn geometry(&self) -> Result<BlockGeometry, DriverError> {
        self.0.lock().geometry()
    }

    fn read_blocks(&mut self, lba: u64, buf: &mut [u8]) -> Result<(), DriverError> {
        self.0.lock().read_blocks(lba, buf)
    }

    fn write_blocks(&mut self, lba: u64, buf: &[u8]) -> Result<(), DriverError> {
        self.0.lock().write_blocks(lba, buf)
    }

    fn flush(&mut self) -> Result<(), DriverError> {
        self.0.lock().flush()
    }
}

impl Block for RamBlock {
    fn device_class(&self) -> BlkDeviceClass {
        BlkDeviceClass::Removable
    }

    fn geometry(&self) -> Result<BlockGeometry, DriverError> {
        Ok(BlockGeometry {
            block_size: BLOCK_SIZE_U32,
            block_count: (self.data.len() / BLOCK_SIZE) as u64,
        })
    }

    fn read_blocks(&mut self, lba: u64, buf: &mut [u8]) -> Result<(), DriverError> {
        let start = usize::try_from(lba).map_err(|_| DriverError::LengthOutOfRange)? * BLOCK_SIZE;
        let end = start
            .checked_add(buf.len())
            .filter(|&end| {
                end <= self.data.len() && !buf.is_empty() && buf.len().is_multiple_of(BLOCK_SIZE)
            })
            .ok_or(DriverError::LengthOutOfRange)?;
        buf.copy_from_slice(&self.data[start..end]);
        Ok(())
    }

    fn write_blocks(&mut self, lba: u64, buf: &[u8]) -> Result<(), DriverError> {
        let start = usize::try_from(lba).map_err(|_| DriverError::LengthOutOfRange)? * BLOCK_SIZE;
        let end = start
            .checked_add(buf.len())
            .filter(|&end| {
                end <= self.data.len() && !buf.is_empty() && buf.len().is_multiple_of(BLOCK_SIZE)
            })
            .ok_or(DriverError::LengthOutOfRange)?;
        self.data[start..end].copy_from_slice(buf);
        Ok(())
    }

    fn flush(&mut self) -> Result<(), DriverError> {
        Ok(())
    }
}

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
            nlink: 1,
            size: n.content.len() as u64,
            allocated: n.content.len() as u64,
            times: NodeTimes::default(),
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
