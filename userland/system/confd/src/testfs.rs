//! An in-memory [`Storage`] standing in for the store volume, so the whole
//! engine — authorisation, the ownership pin, the layered read, staging, and
//! the atomic publish — is exercised on the host with no filesystem at all.
//!
//! It models exactly what the engine depends on and nothing more: a flat map
//! of paths to bytes, a per-path owning uid, a set of directories, and a
//! switch that makes every operation report an unreachable volume (the
//! not-yet-unlocked encrypted root).

use alloc::collections::{BTreeMap, BTreeSet};
use alloc::string::{String, ToString};
use alloc::vec::Vec;

use tairix_abi::{Errno, ProcId};
use tairix_users::CONFD_UID;

use crate::{DirEntry, NodeInfo, Storage};

/// The uid of the account whose home the fixtures provision.
pub const ACCOUNT_UID: u32 = 1000;

/// That account's home directory.
pub const HOME: &str = "/Users/ada";

/// An in-memory store volume.
pub struct TestFs {
    files: BTreeMap<String, Vec<u8>>,
    dirs: BTreeSet<String>,
    owners: BTreeMap<String, u32>,
    /// When set, every operation reports this error — the unreachable-volume
    /// case an early-boot caller meets.
    offline: Option<Errno>,
    /// Paths whose owner this service may not read.
    hidden: Vec<String>,
    /// Every descriptor delegation minted, in mint order — what a test
    /// inspects instead of a kernel handle table.
    grants: Vec<Grant>,
}

/// One minted descriptor delegation, as the fake records it.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct Grant {
    /// The file the delegation names.
    pub path: String,
    /// Whether it conveys write access.
    pub write: bool,
    /// The extent ceiling it carries.
    pub ceiling: u64,
    /// The process instance it was minted to.
    pub recipient: ProcId,
}

impl TestFs {
    /// An empty volume with `/Users` and the read-only policy root, and no
    /// homes.
    pub fn bare() -> Self {
        let mut fs = Self {
            files: BTreeMap::new(),
            dirs: BTreeSet::new(),
            owners: BTreeMap::new(),
            offline: None,
            hidden: Vec::new(),
            grants: Vec::new(),
        };
        fs.add_dir("/Users", 0);
        fs.add_dir("/System", 0);
        fs.add_dir("/System/Settings", 0);
        fs
    }

    /// A volume carrying one properly provisioned home: the search-only
    /// transit path down to a gated root the app-data service owns.
    pub fn provisioned() -> Self {
        let mut fs = Self::bare();
        fs.add_home(HOME, ACCOUNT_UID);
        fs
    }

    /// Provision `home` for `uid`, with the gated per-app store root beneath
    /// it exactly as the three real provisioners create it.
    pub fn add_home(&mut self, home: &str, uid: u32) {
        self.add_dir(home, uid);
        for parent in tairix_users::APPDATA_ROOT_PARENTS {
            let dir = alloc::format!("{home}/{parent}");
            self.add_dir(&dir, uid);
            self.add_dir(
                &alloc::format!("{dir}/{}", tairix_users::APPDATA_ROOT),
                CONFD_UID.0,
            );
        }
    }

    /// Record a directory owned by `uid` — also how a test plants an
    /// application-owned decoy where a provisioned root should be.
    pub fn add_dir(&mut self, path: &str, uid: u32) {
        self.dirs.insert(path.to_string());
        self.owners.insert(path.to_string(), uid);
    }

    /// Plant `bytes` at `path`, creating the parent directories.
    pub fn put(&mut self, path: &str, bytes: &[u8]) {
        if let Some(cut) = path.rfind('/') {
            let parent = &path[..cut];
            if !parent.is_empty() && !self.dirs.contains(parent) {
                self.add_dir(parent, CONFD_UID.0);
            }
        }
        self.files.insert(path.to_string(), bytes.to_vec());
        self.owners.insert(path.to_string(), CONFD_UID.0);
    }

    /// `true` if `path` names a file or a directory.
    pub fn exists(&self, path: &str) -> bool {
        self.files.contains_key(path) || self.dirs.contains(path)
    }

    /// The text at `path`, if it holds valid UTF-8.
    pub fn read_text(&self, path: &str) -> Option<String> {
        let bytes = self.files.get(path)?;
        core::str::from_utf8(bytes).ok().map(String::from)
    }

    /// Reassign the owner of `path` — how a test plants an application-owned
    /// decoy where the service's own root should be.
    pub fn set_owner(&mut self, path: &str, uid: u32) {
        self.owners.insert(path.to_string(), uid);
    }

    /// Remove `path` and everything beneath it.
    pub fn remove(&mut self, path: &str) {
        let prefix = alloc::format!("{path}/");
        self.files
            .retain(|key, _| key != path && !key.starts_with(&prefix));
        self.dirs
            .retain(|key| key != path && !key.starts_with(&prefix));
        self.owners
            .retain(|key, _| key != path && !key.starts_with(&prefix));
    }

    /// Make `path` report [`Errno::PermissionDenied`] to a stat — a foreign
    /// home the app-data service may not look at.
    pub fn hide(&mut self, path: &str) {
        self.hidden.push(path.to_string());
    }

    /// Make every subsequent operation report `err`.
    pub fn fail_all(&mut self, err: Errno) {
        self.offline = Some(err);
    }

    /// Every delegation minted so far, in mint order.
    pub fn grants(&self) -> &[Grant] {
        &self.grants
    }

    /// The unreachable-volume error, if one is armed.
    fn offline(&self) -> Result<(), Errno> {
        match self.offline {
            Some(err) => Err(err),
            None => Ok(()),
        }
    }
}

impl Storage for TestFs {
    fn read(&mut self, path: &str) -> Result<Vec<u8>, Errno> {
        self.offline()?;
        self.files.get(path).cloned().ok_or(Errno::NotFound)
    }

    fn write(&mut self, path: &str, bytes: &[u8]) -> Result<(), Errno> {
        self.offline()?;
        // A write into a directory that does not exist would be a path the
        // engine composed wrongly; surface it rather than inventing the
        // parent, which the real filesystem would never do.
        let cut = path.rfind('/').ok_or(Errno::NotFound)?;
        if !self.dirs.contains(&path[..cut]) {
            return Err(Errno::NotFound);
        }
        self.files.insert(path.to_string(), bytes.to_vec());
        self.owners.insert(path.to_string(), CONFD_UID.0);
        Ok(())
    }

    fn rename(&mut self, src: &str, dst: &str) -> Result<(), Errno> {
        self.offline()?;
        let bytes = self.files.remove(src).ok_or(Errno::NotFound)?;
        self.files.insert(dst.to_string(), bytes);
        Ok(())
    }

    fn mkdir(&mut self, path: &str, _mode: u32) -> Result<(), Errno> {
        self.offline()?;
        if self.exists(path) {
            return Err(Errno::AlreadyExists);
        }
        let cut = path.rfind('/').ok_or(Errno::NotFound)?;
        if !self.dirs.contains(&path[..cut]) {
            return Err(Errno::NotFound);
        }
        self.add_dir(path, CONFD_UID.0);
        Ok(())
    }

    fn unlink(&mut self, path: &str) -> Result<(), Errno> {
        self.offline()?;
        self.files.remove(path).ok_or(Errno::NotFound)?;
        self.owners.remove(path);
        Ok(())
    }

    fn stat(&mut self, path: &str) -> Result<NodeInfo, Errno> {
        self.offline()?;
        if self.hidden.iter().any(|hidden| hidden == path) {
            return Err(Errno::PermissionDenied);
        }
        let uid = self.owners.get(path).copied().ok_or(Errno::NotFound)?;
        Ok(NodeInfo {
            uid,
            len: self
                .files
                .get(path)
                .map_or(0, |bytes| bytes.len().try_into().unwrap_or(u64::MAX)),
        })
    }

    fn list_dir(&mut self, path: &str) -> Result<Vec<DirEntry>, Errno> {
        self.offline()?;
        if !self.dirs.contains(path) {
            return Err(Errno::NotFound);
        }
        let prefix = alloc::format!("{path}/");
        let names = |keys: &mut dyn Iterator<Item = &String>, dir: bool| -> Vec<DirEntry> {
            keys.filter_map(|key| key.strip_prefix(&prefix))
                .filter(|rest| !rest.contains('/'))
                .map(|rest| DirEntry {
                    name: String::from(rest),
                    dir,
                })
                .collect()
        };
        let mut entries = names(&mut self.dirs.iter(), true);
        entries.extend(names(&mut self.files.keys(), false));
        Ok(entries)
    }

    fn grant(
        &mut self,
        path: &str,
        write: bool,
        ceiling: u64,
        recipient: ProcId,
    ) -> Result<u64, Errno> {
        self.offline()?;
        if !self.files.contains_key(path) {
            if !write {
                return Err(Errno::NotFound);
            }
            let cut = path.rfind('/').ok_or(Errno::NotFound)?;
            if !self.dirs.contains(&path[..cut]) {
                return Err(Errno::NotFound);
            }
            self.files.insert(path.to_string(), Vec::new());
            self.owners.insert(path.to_string(), CONFD_UID.0);
        }
        self.grants.push(Grant {
            path: path.to_string(),
            write,
            ceiling,
            recipient,
        });
        // Handle 0 is the reserved invalid value, exactly as the kernel's mint
        // treats it, so a test that reads a handle back proves it was minted.
        Ok(self.grants.len().try_into().unwrap_or(u64::MAX))
    }
}
