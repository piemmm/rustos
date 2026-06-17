//! VFS-backed [`ImageSource`] adapter for the signed-driver-store scan
//! (`plans/PI.md` P10 Stage 4.HW item 5; `AGENTS.md` §18.3 / §18.6).
//!
//! The user-space store scan (`rustos_drvhost::store::scan_store`) fetches
//! each bundle's bytes through the `drvhost` [`ImageSource`] seam. On a real
//! installation those bytes live on the mounted root volume under
//! `/System/Drivers/`, so the production boot wiring needs an `ImageSource`
//! that reads them through the kernel's root-backed VFS.
//!
//! That read belongs in `kernel/core`, which owns the root-mount builder and
//! the §5.3-checked per-inode delegation
//! ([`rustos_kernel_core::DriverImageReader`]). But the [`ImageSource`] trait
//! lives in `userland/system/drvhost`, and the §17.4 layering forbids
//! `kernel/core` from depending on a userland crate. The bin crate is the one
//! layer that may name `drvhost` (`AGENTS.md` §17.4), so this thin adapter
//! lives here and simply *delegates* to the kernel-core reader — adding no
//! authority of its own.
//!
//! The adapter holds a single [`DriverImageReader`] (its root-backed VFS built
//! once, `AGENTS.md` §2.16) and the root-volume filesystem driver. The driver
//! needs `&mut` access per read, but [`ImageSource::read`] takes `&self`, so
//! the driver is held behind a [`RefCell`]: the scan is single-threaded and
//! pulls one bundle at a time, so the borrow never overlaps. Every capability
//! and §5.3 check stays in the kernel-core reader, which fails closed
//! (`AGENTS.md` §5.4 / §2.9).

use core::cell::RefCell;

use alloc::vec::Vec;

use rustos_abi::driver::filesystem::{FilesystemRead, FilesystemSecurity};
use rustos_abi::Errno;
use rustos_drvhost::ImageSource;
use rustos_kernel_core::{DriverImageError, DriverImageReader, VfsError};

/// An [`ImageSource`] that reads driver-bundle images off the mounted root
/// volume's `/System/Drivers/` store through the kernel's root-backed VFS.
///
/// Construct one with [`VfsImageSource::open`], then hand `&source` to
/// `rustos_drvhost::store::scan_store` alongside the paths
/// [`rustos_kernel_core::enumerate_driver_store`] returned.
pub struct VfsImageSource<'a, F: ?Sized> {
    reader: DriverImageReader,
    fs: RefCell<&'a mut F>,
}

impl<'a, F> VfsImageSource<'a, F>
where
    F: FilesystemRead + FilesystemSecurity + ?Sized,
{
    /// Build an adapter over the mounted root volume's filesystem driver
    /// `fs`, constructing the root-backed VFS once.
    ///
    /// # Errors
    ///
    /// The [`VfsError`] from [`DriverImageReader::open`] if the private root
    /// mount cannot be built.
    pub fn open(fs: &'a mut F) -> Result<Self, VfsError> {
        Ok(Self {
            reader: DriverImageReader::open()?,
            fs: RefCell::new(fs),
        })
    }
}

impl<F> ImageSource for VfsImageSource<'_, F>
where
    F: FilesystemRead + FilesystemSecurity + ?Sized,
{
    /// Read the bundle at `path`, appending its bytes to `buf`
    /// (the [`ImageSource`] contract). Delegates to
    /// [`DriverImageReader::read_image`]; the precise refusal is mapped to
    /// the stable [`Errno`] the scan records as the bundle's skip reason.
    fn read(&self, path: &str, buf: &mut Vec<u8>) -> Result<(), Errno> {
        let mut fs = self.fs.borrow_mut();
        self.reader
            .read_image(&mut **fs, path, buf)
            .map_err(DriverImageError::to_errno)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    use alloc::collections::BTreeMap;
    use alloc::string::{String, ToString};
    use alloc::vec;

    use rustos_abi::driver::filesystem::{DirEntry, NodeId, NodeInfo, NodeKind, NodeSecurity};
    use rustos_abi::driver::DriverError;

    const ROOT_ID: u64 = 1;

    /// One node of a minimal mock root volume: a directory tree plus file
    /// contents, enough to drive the root-backed VFS delegation path.
    struct Node {
        kind: NodeKind,
        children: Vec<(String, u64)>,
        content: Vec<u8>,
        security: NodeSecurity,
    }

    /// A minimal mock root-volume driver serving an in-memory tree, the
    /// surface `rustos_kernel_core::DriverImageReader` reads through.
    struct MockFs {
        nodes: BTreeMap<u64, Node>,
        next: u64,
    }

    impl MockFs {
        fn new() -> Self {
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

        fn child(&self, dir: u64, name: &str) -> Option<u64> {
            self.nodes
                .get(&dir)?
                .children
                .iter()
                .find(|(n, _)| n == name)
                .map(|(_, id)| *id)
        }

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

        fn add_file(&mut self, path: &str, content: &[u8]) {
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

    impl FilesystemRead for MockFs {
        fn root(&self) -> NodeId {
            NodeId::from_raw(ROOT_ID)
        }

        fn node_info(&mut self, node: NodeId) -> Result<NodeInfo, DriverError> {
            let n = self.nodes.get(&node.raw()).ok_or(DriverError::NotFound)?;
            Ok(NodeInfo {
                kind: n.kind,
                size: n.content.len() as u64,
            })
        }

        fn lookup(&mut self, dir: NodeId, name: &[u8]) -> Result<NodeId, DriverError> {
            let name = core::str::from_utf8(name).map_err(|_| DriverError::NotFound)?;
            self.child(dir.raw(), name)
                .map(NodeId::from_raw)
                .ok_or(DriverError::NotFound)
        }

        fn read_at(
            &mut self,
            file: NodeId,
            offset: u64,
            buf: &mut [u8],
        ) -> Result<usize, DriverError> {
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

    impl FilesystemSecurity for MockFs {
        fn security(&mut self, node: NodeId) -> Result<NodeSecurity, DriverError> {
            self.nodes
                .get(&node.raw())
                .map(|n| n.security)
                .ok_or(DriverError::NotFound)
        }
    }

    #[test]
    fn read_delegates_to_the_reader_and_appends() {
        let mut fs = MockFs::new();
        fs.add_file("/System/Drivers/usb_kbd", b"BUNDLE");
        let source = VfsImageSource::open(&mut fs).expect("root mount builds");

        // The scan pre-clears and reuses one buffer across bundles; prove a
        // non-empty prefix is preserved (the append contract).
        let mut buf = vec![0x01u8];
        source
            .read("/System/Drivers/usb_kbd", &mut buf)
            .expect("a readable in-store bundle");
        assert_eq!(buf, vec![0x01, b'B', b'U', b'N', b'D', b'L', b'E']);
    }

    #[test]
    fn read_serves_multiple_bundles_through_the_one_borrowed_driver() {
        let mut fs = MockFs::new();
        fs.add_file("/System/Drivers/a", b"AAA");
        fs.add_file("/System/Drivers/b", b"BB");
        let source = VfsImageSource::open(&mut fs).expect("root mount builds");

        let mut buf = Vec::new();
        source.read("/System/Drivers/a", &mut buf).expect("a");
        assert_eq!(buf, b"AAA");
        buf.clear();
        source.read("/System/Drivers/b", &mut buf).expect("b");
        assert_eq!(buf, b"BB");
    }

    #[test]
    fn a_missing_bundle_maps_to_not_found() {
        let mut fs = MockFs::new();
        fs.add_file("/System/Drivers/present", b"x");
        let source = VfsImageSource::open(&mut fs).expect("root mount builds");

        let mut buf = Vec::new();
        assert_eq!(
            source.read("/System/Drivers/absent", &mut buf),
            Err(Errno::NotFound)
        );
        assert!(buf.is_empty());
    }

    #[test]
    fn a_path_outside_the_store_is_denied() {
        let mut fs = MockFs::new();
        fs.add_file("/System/Security/Users", b"secret");
        let source = VfsImageSource::open(&mut fs).expect("root mount builds");

        let mut buf = Vec::new();
        assert_eq!(
            source.read("/System/Security/Users", &mut buf),
            Err(Errno::PermissionDenied)
        );
        assert!(buf.is_empty());
    }
}
