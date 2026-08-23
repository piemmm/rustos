//! The per-app blob store: bulk data an application reaches as a descriptor,
//! not as bytes on the app-data channel.
//!
//! # Why a descriptor and not a payload
//!
//! A mail index, a search index, or a thumbnail cache is the wrong shape for a
//! message: the IPC payload ceiling is far below what one holds, so a store
//! that proxied bytes could not serve them at all. So the service makes the
//! policy decision once — is this the caller's own blob, does the store belong
//! to this publisher, is the count ceiling reached — and hands back a one-shot
//! descriptor delegation. The application then reads, writes, truncates, and
//! memory-maps the file directly against the kernel VFS at full speed, and the
//! service never touches a byte of payload.
//!
//! # What bounds direct access
//!
//! The delegation is the bound. It carries only the access the caller's mode
//! asked for, and a writable one carries a byte-extent ceiling the kernel
//! enforces on every write and truncate through it, so an application cannot
//! grow a blob past [`APPDATA_BLOB_MAX_BYTES`] however it uses the descriptor.
//! Admission enforces the other dimension, the blob *count*, and nothing else:
//! summing sizes at open time and refusing the next open would do nothing
//! about the blob a caller already holds open, and a defence a hostile
//! application defeats in one line is worse than none because it reads as an
//! assurance.
//!
//! Why the store must bound this at all rather than leaning on a filesystem
//! quota: the gated tree is owned by the app-data service precisely so the
//! account's own shell cannot reach it, so every byte written to a blob is
//! charged to the *service's* uid. No per-user filesystem quota, present or
//! future, would ever see it.
//!
//! # One pin, both trees
//!
//! Blobs live under `Library/`, configuration under `Settings/`, and one
//! `.owner` record in the configuration store governs both: it attests who
//! owns the *application's data*, not who owns one file. A blob operation
//! therefore resolves and pins through [`AppStore`] first and reaches the bulk
//! tree only behind that check, so a publisher squatting another developer's
//! identifier is refused before a byte of its blobs is reachable.

use alloc::string::String;
use alloc::vec::Vec;

use tairix_abi::appdata_ipc::{
    validate_blob_name, BlobEntry, BlobMode, BlobQuota, APPDATA_BLOB_MAX_BYTES,
    APPDATA_BLOB_MAX_COUNT,
};
use tairix_abi::Errno;
use tairix_users::AppDataTree;

use crate::store::{AppStore, RootCache, StoreError, STORE_DIR_MODE};
use crate::Storage;

/// Directory name of an application's blob store inside its bulk tree.
///
/// A sibling of the `Cache/` and `Temp/` scopes rather than the bulk root
/// itself, so those can be reaped or evicted on their own policy without a
/// blob ever being caught by one.
pub const BLOBS_DIR: &str = "Blobs";

/// One application's blob store, resolved and authorised.
///
/// Holding one is proof that the caller's app identity was attested, that the
/// configuration store's ownership pin named this publisher, and that the
/// gated bulk root belongs to the app-data service.
pub struct BlobStore {
    /// Absolute path of `<home>/Library/Apps/<bundle-id>/Blobs`, with no
    /// trailing separator.
    dir: String,
    /// Whether the directory has been created. A store that has never held a
    /// blob answers an empty listing and a zero quota without touching the
    /// volume, so a read is never the act that creates one.
    present: bool,
}

impl BlobStore {
    /// Resolve and authorise the blob store of the application `store` was
    /// opened for.
    ///
    /// `create` decides what happens when the application has never held a
    /// blob: a read or a listing passes `false` and gets an absent store,
    /// which answers empty; an open for writing passes `true`, which creates
    /// the directory. So no read pays a write, and a probe cannot bring a
    /// store into existence.
    ///
    /// **An unpinned store has no blobs**, whatever is on the volume, exactly
    /// as an unpinned configuration store reads as an empty document: with no
    /// ownership pin there is no attested owner, so there is nothing this
    /// service may serve. That is what makes the pin's authority cover the
    /// bulk tree structurally — a `create` reaches here only after
    /// [`AppStore::open`] has created or matched the pin, so a blob cannot
    /// exist in a store whose owner was never recorded and a later publisher
    /// claiming the identifier cannot inherit one.
    ///
    /// # Errors
    ///
    /// [`StoreError::RootNotOwned`] when the gated bulk root is absent or is
    /// not the app-data service's own, [`StoreError::Unavailable`] when the
    /// volume cannot be reached.
    pub fn open<S: Storage + ?Sized>(
        fs: &mut S,
        store: &AppStore,
        create: bool,
    ) -> Result<Self, StoreError> {
        // The bulk root's ownership is proved here rather than inherited from
        // the configuration root's: they are two directories, and one of them
        // being the service's says nothing about the other.
        let root = RootCache::root_of(fs, store.home(), AppDataTree::Bulk)?;
        let dir = crate::store::join(&crate::store::join(&root, store.bundle_id()), BLOBS_DIR);
        if !store.is_pinned() {
            return Ok(Self {
                dir,
                present: false,
            });
        }
        let present = match fs.stat(&dir) {
            Ok(_) => true,
            Err(Errno::NotFound) if !create => false,
            Err(Errno::NotFound) => {
                create_dirs(fs, &dir)?;
                true
            }
            Err(_) => return Err(StoreError::Unavailable),
        };
        Ok(Self { dir, present })
    }

    /// Mint a one-shot descriptor delegation for the blob `name`, to the live
    /// task `task`.
    ///
    /// A read-only open of a blob the application does not hold answers
    /// [`StoreError::BlobNotFound`]; a read-write open creates it, which is
    /// what makes creation the mode's business rather than a separate flag.
    /// The delegation a write conveys carries an extent ceiling of
    /// [`APPDATA_BLOB_MAX_BYTES`], enforced by the kernel.
    ///
    /// # Errors
    ///
    /// [`StoreError::BlobNotFound`] for a read of a blob that does not exist,
    /// [`StoreError::BlobLimit`] when creating one would pass
    /// [`APPDATA_BLOB_MAX_COUNT`], [`StoreError::Unavailable`] for an
    /// unreachable volume or a delegation the kernel refused.
    pub fn grant<S: Storage + ?Sized>(
        &self,
        fs: &mut S,
        name: &str,
        mode: BlobMode,
        task: u64,
    ) -> Result<u64, StoreError> {
        // The name arrived inside the store-name grammar at decode; re-stating
        // it here is what makes a path this module composes safe on its own
        // terms rather than on a caller's discipline.
        validate_blob_name(name).map_err(|_| StoreError::BlobNameRefused)?;
        if !self.present {
            return Err(StoreError::BlobNotFound);
        }
        let path = crate::store::join(&self.dir, name);
        let existing = match fs.stat(&path) {
            Ok(_) => true,
            Err(Errno::NotFound) => false,
            Err(_) => return Err(StoreError::Unavailable),
        };
        if !existing {
            if !mode.is_write() {
                return Err(StoreError::BlobNotFound);
            }
            // Admission is the count and nothing else: a new blob is refused
            // when the store is full, and the extent ceiling below bounds
            // every blob's bytes without a sum this service could be raced on.
            if self.names(fs)?.len() >= APPDATA_BLOB_MAX_COUNT {
                return Err(StoreError::BlobLimit);
            }
        }
        let ceiling = if mode.is_write() {
            APPDATA_BLOB_MAX_BYTES
        } else {
            0
        };
        fs.grant(&path, mode.is_write(), ceiling, task)
            .map_err(|err| match err {
                Errno::NotFound => StoreError::BlobNotFound,
                _ => StoreError::Unavailable,
            })
    }

    /// Delete the blob `name`.
    ///
    /// Deleting one the application does not hold removes nothing and is not
    /// an error, so a refusal never reveals which blobs exist and a delete
    /// cannot bring a store into existence.
    ///
    /// # Errors
    ///
    /// [`StoreError::BlobNameRefused`] for a name outside the grammar,
    /// [`StoreError::Unavailable`] for an unreachable volume.
    pub fn delete<S: Storage + ?Sized>(&self, fs: &mut S, name: &str) -> Result<(), StoreError> {
        validate_blob_name(name).map_err(|_| StoreError::BlobNameRefused)?;
        if !self.present {
            return Ok(());
        }
        match fs.unlink(&crate::store::join(&self.dir, name)) {
            Ok(()) | Err(Errno::NotFound) => Ok(()),
            Err(_) => Err(StoreError::Unavailable),
        }
    }

    /// Every blob the application holds, with its length, sorted by name.
    ///
    /// Sorted so a listing is a stable answer rather than whatever order the
    /// volume enumerates in, which is what lets a caller compare two listings
    /// and lets a test assert on one.
    ///
    /// # Errors
    ///
    /// [`StoreError::Unavailable`] for an unreachable volume.
    pub fn listing<S: Storage + ?Sized>(
        &self,
        fs: &mut S,
    ) -> Result<Vec<(String, u64)>, StoreError> {
        let mut listed = Vec::new();
        for name in self.names(fs)? {
            // A blob that vanished between the listing and the stat is one the
            // caller has already deleted, so it is left out rather than
            // reported as a length of zero.
            if let Ok(node) = fs.stat(&crate::store::join(&self.dir, &name)) {
                listed.push((name, node.len));
            }
        }
        listed.sort_unstable();
        Ok(listed)
    }

    /// The application's blob usage and the ceilings it is bounded by.
    ///
    /// # Errors
    ///
    /// As [`Self::listing`].
    pub fn quota<S: Storage + ?Sized>(&self, fs: &mut S) -> Result<BlobQuota, StoreError> {
        let listed = self.listing(fs)?;
        Ok(BlobQuota {
            blobs: listed.len() as u64,
            bytes: listed.iter().map(|(_, len)| *len).sum(),
            blob_max: APPDATA_BLOB_MAX_COUNT as u64,
            blob_bytes_max: APPDATA_BLOB_MAX_BYTES,
        })
    }

    /// The names of the blobs the application holds.
    ///
    /// A directory entry that is itself a directory, or whose name is outside
    /// the blob-name grammar, is not a blob this service created and is left
    /// out — so a listing reports only names a caller could have asked for,
    /// and admission counts only those too.
    fn names<S: Storage + ?Sized>(&self, fs: &mut S) -> Result<Vec<String>, StoreError> {
        if !self.present {
            return Ok(Vec::new());
        }
        match fs.list_dir(&self.dir) {
            Ok(entries) => Ok(entries
                .into_iter()
                .filter(|entry| !entry.dir && validate_blob_name(&entry.name).is_ok())
                .map(|entry| entry.name)
                .collect()),
            Err(Errno::NotFound) => Ok(Vec::new()),
            Err(_) => Err(StoreError::Unavailable),
        }
    }
}

/// Render `listing` as the fixed-width entry sequence the wire carries.
///
/// # Errors
///
/// [`StoreError::BlobNameRefused`] for a name the wire's own grammar refuses —
/// unreachable, because [`BlobStore::listing`] filters to that grammar, but
/// fail closed rather than trust the filter.
pub fn render_listing(listing: &[(String, u64)]) -> Result<Vec<u8>, StoreError> {
    let mut out = alloc::vec![0u8; listing.len() * tairix_abi::appdata_ipc::APPDATA_BLOB_ENTRY_LEN];
    for (slot, (name, len)) in out
        .chunks_mut(tairix_abi::appdata_ipc::APPDATA_BLOB_ENTRY_LEN)
        .zip(listing)
    {
        tairix_abi::appdata_ipc::encode_blob_entry(&BlobEntry { name, len: *len }, slot)
            .map_err(|_| StoreError::BlobNameRefused)?;
    }
    Ok(out)
}

/// Create `dir` and the per-app directory above it, in that order.
///
/// The bulk tree's per-app directory is created here rather than with the
/// configuration store's, because an application that never holds a blob
/// should leave nothing behind in `Library/`.
fn create_dirs<S: Storage + ?Sized>(fs: &mut S, dir: &str) -> Result<(), StoreError> {
    let parent = dir
        .rfind('/')
        .map(|cut| &dir[..cut])
        .ok_or(StoreError::Unavailable)?;
    for path in [parent, dir] {
        match fs.mkdir(path, STORE_DIR_MODE) {
            // An existing directory is the interrupted tail of an earlier
            // create, or the parent a sibling scope already made: finish
            // rather than refusing for ever.
            Ok(()) | Err(Errno::AlreadyExists) => {}
            Err(_) => return Err(StoreError::Unavailable),
        }
    }
    Ok(())
}

#[cfg(test)]
#[path = "blob_tests.rs"]
mod tests;
