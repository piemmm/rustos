//! The per-app bulk store: the two scopes of `Library/Apps/<bundle-id>/` an
//! application reaches as descriptors rather than as bytes on the app-data
//! channel — the durable [`BlobStore`] and the per-boot [`TempStore`].
//!
//! A mail index, a thumbnail cache, or a staged download is the wrong shape for
//! a message: the IPC payload ceiling is far below what one holds, so a store
//! that proxied bytes could not serve them at all. The service decides once —
//! is this the caller's own file, does the store belong to this publisher, is
//! the count ceiling reached — and hands back a one-shot descriptor delegation.
//!
//! The delegation is the bound: it carries only the access the operation asked
//! for, and a writable one carries a byte-extent ceiling
//! ([`APPDATA_BULK_FILE_MAX_BYTES`]) the kernel enforces on every write and
//! truncate through it. Admission enforces the other dimension, the file
//! *count*, and **nothing else** — summing sizes at open time and
//! refusing the next open would do nothing about the file a caller already
//! holds open, and a defence a hostile application defeats in one line is worse
//! than none because it reads as an assurance.
//!
//! A blob is durable and the application names it. A temporary file is the
//! service's to name, and nothing here *opens* one, so the only way to hold one
//! is to have just created it. Their lifetime is the boot and the name says
//! which one ([`TempNames`]), so no marker record has to be kept in step with
//! the directory and no boot-time walk of every account's every store is paid.
//! The two share everything but that: one inner scope resolves the directory,
//! proves the gated root, enumerates it, unlinks from it, and mints its
//! delegations, and the two public faces add only the rule that is their own.
//!
//! Bulk data lives under `Library/` and configuration under `Settings/`, and one
//! `.owner` record in the configuration store governs both. So a bulk operation
//! resolves and pins through [`AppStore`] first and reaches this tree only
//! behind that check, and a publisher squatting another developer's identifier
//! is refused before a byte of its data is reachable.
//!
//! Why the ceilings are fixed and why the service must have them are
//! `docs/src/userland/confd.md`.

use alloc::string::String;
use alloc::vec::Vec;

use tairix_abi::appdata_ipc::{
    validate_bulk_name, BlobEntry, BlobMode, BulkQuota, APPDATA_BLOB_MAX_COUNT,
    APPDATA_BULK_FILE_MAX_BYTES, APPDATA_TEMP_MAX_COUNT,
};
use tairix_abi::{BootId, Errno, BOOT_ID_HEX_LEN};
use tairix_users::AppDataTree;

use crate::store::{AppStore, RootCache, StoreError, STORE_DIR_MODE};
use crate::vault::Entropy;
use crate::Storage;

/// Directory name of an application's durable blob store inside its bulk tree.
pub const BLOBS_DIR: &str = "Blobs";

/// Directory name of an application's temporary files inside its bulk tree.
///
/// A sibling of [`BLOBS_DIR`] rather than the bulk root itself, so the reap
/// that empties it after a boot can never catch a blob.
pub const TEMP_DIR: &str = "Temp";

/// Bytes of randomness in the slot half of a temporary file's name.
///
/// Eight is what makes a name unique without the service remembering which it
/// has already handed out. A counter would have to be kept per process and
/// would re-issue a name after a release, so a caller that released the same
/// name twice would delete a *later* file; a drawn slot makes that
/// unrepresentable rather than unlikely.
const TEMP_SLOT_LEN: usize = 8;

/// The naming rule for temporary files: `<boot>-<slot>`, both lowercase hex.
///
/// The boot half is what makes staleness legible with no second record to keep
/// in step: a file's own name says which boot it belongs to, so "this is not
/// mine to serve" is read from the name itself rather than from a marker a torn
/// write could contradict. The slot half is drawn afresh for every file, so two
/// instances of one application never collide and a name a caller kept across a
/// reboot names nothing at all.
///
/// Every character it can produce is a lowercase hex digit or the separator, so
/// a minted name is inside the one store-name grammar by construction and can
/// never be a traversal, a hidden entry, or a case-folding collision.
#[derive(Copy, Clone, Debug, Eq, PartialEq)]
pub struct TempNames {
    boot: [u8; BOOT_ID_HEX_LEN],
}

impl TempNames {
    /// The naming rule for the running boot, or [`None`] when the boot has no
    /// identity.
    ///
    /// A boot whose identity is unset had no seeded generator when the kernel
    /// minted it, so this service cannot tell one boot's scratch from another's
    /// — it refuses the scope rather than serving files it could never reclaim.
    #[must_use]
    pub fn of(boot: BootId) -> Option<Self> {
        if boot.is_unset() {
            return None;
        }
        let mut rendered = [0u8; BOOT_ID_HEX_LEN];
        // The rendering answers the empty string rather than panicking if what
        // it wrote were not UTF-8. It cannot be — every byte is a hex digit —
        // but a naming rule built on a rendering that failed would give every
        // boot's files one prefix, so it is refused rather than assumed.
        if boot.write_hex(&mut rendered).is_empty() {
            return None;
        }
        Some(Self { boot: rendered })
    }

    /// Whether `name` names a file of this boot.
    #[must_use]
    pub fn is_live(&self, name: &str) -> bool {
        // The separator is part of the prefix: a name whose boot half merely
        // starts with this boot's belongs to another boot.
        let bytes = name.as_bytes();
        bytes.len() > BOOT_ID_HEX_LEN
            && bytes[..BOOT_ID_HEX_LEN] == self.boot
            && bytes[BOOT_ID_HEX_LEN] == b'-'
    }

    /// The name a file drawn as `slot` carries in this boot.
    #[must_use]
    fn name(&self, slot: [u8; TEMP_SLOT_LEN]) -> String {
        const DIGITS: &[u8; 16] = b"0123456789abcdef";
        let mut name = String::with_capacity(BOOT_ID_HEX_LEN + 1 + TEMP_SLOT_LEN * 2);
        // Both halves are built a hex digit at a time, so no step of this can
        // fail and none of it needs a fallback that would name two files alike.
        name.extend(self.boot.iter().map(|digit| char::from(*digit)));
        name.push('-');
        for byte in slot {
            name.push(char::from(DIGITS[usize::from(byte >> 4)]));
            name.push(char::from(DIGITS[usize::from(byte & 0x0f)]));
        }
        name
    }
}

/// One scope's directory inside an application's bulk tree, resolved and
/// authorised: everything the two public faces have in common.
///
/// Holding one is proof that the caller's app identity was attested, that the
/// configuration store's ownership pin named this publisher, and that the gated
/// bulk root belongs to the app-data service.
struct Scope {
    /// Absolute path of `<home>/Library/Apps/<bundle-id>/<scope>`, with no
    /// trailing separator.
    dir: String,
    /// Whether the directory has been created. A scope that has never held a
    /// file answers an empty listing and a zero usage without touching the
    /// volume, so a read is never the act that creates one.
    present: bool,
}

impl Scope {
    /// Resolve and authorise the directory `name` inside the bulk tree of the
    /// application `store` was opened for, on the terms
    /// [`BlobStore::open`] documents.
    fn open<S: Storage + ?Sized>(
        fs: &mut S,
        store: &AppStore,
        name: &str,
        create: bool,
    ) -> Result<Self, StoreError> {
        // The bulk root's ownership is proved here rather than inherited from
        // the configuration root's: they are two directories, and one of them
        // being the service's says nothing about the other.
        let root = RootCache::root_of(fs, store.home(), AppDataTree::Bulk)?;
        let dir = crate::store::join(&crate::store::join(&root, store.bundle_id()), name);
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

    /// The names of the files in this directory that `live` accepts, each with
    /// its length, sorted by name.
    ///
    /// Sorted so a listing is a stable answer rather than whatever order the
    /// volume enumerates in, which is what lets a caller compare two listings
    /// and lets a test assert on one.
    fn listing<S: Storage + ?Sized>(
        &self,
        fs: &mut S,
        live: impl Fn(&str) -> bool,
    ) -> Result<Vec<(String, u64)>, StoreError> {
        let mut listed = Vec::new();
        for name in self.entries(fs)? {
            if !live(&name) {
                continue;
            }
            // A file that vanished between the listing and the stat is one the
            // caller has already deleted, so it is left out rather than
            // reported as a length of zero.
            if let Ok(node) = fs.stat(&crate::store::join(&self.dir, &name)) {
                listed.push((name, node.len));
            }
        }
        listed.sort_unstable();
        Ok(listed)
    }

    /// How many files `live` accepts in this directory.
    ///
    /// Counted without a `stat` each, unlike [`Self::listing`]: admission needs
    /// the count and nothing else.
    fn count<S: Storage + ?Sized>(
        &self,
        fs: &mut S,
        live: impl Fn(&str) -> bool,
    ) -> Result<usize, StoreError> {
        Ok(self
            .entries(fs)?
            .into_iter()
            .filter(|name| live(name))
            .count())
    }

    /// Every entry of this directory that could be a file this service
    /// created.
    ///
    /// The one place the volume is enumerated, so a reap and a listing can
    /// never disagree about what is there — only about which of it is the
    /// application's to see. It is also the one place a name *from* the volume
    /// becomes a path component, so the store-name grammar is applied here:
    /// every path this module composes is therefore built from a name that
    /// cannot traverse, hide, or case-fold, whatever a filesystem driver
    /// enumerates. A directory, and a name outside the grammar, are alike not
    /// files this service created and are left out — neither counted, listed,
    /// nor unlinked.
    fn entries<S: Storage + ?Sized>(&self, fs: &mut S) -> Result<Vec<String>, StoreError> {
        if !self.present {
            return Ok(Vec::new());
        }
        match fs.list_dir(&self.dir) {
            Ok(entries) => Ok(entries
                .into_iter()
                .filter(|entry| !entry.dir && validate_bulk_name(&entry.name).is_ok())
                .map(|entry| entry.name)
                .collect()),
            Err(Errno::NotFound) => Ok(Vec::new()),
            Err(_) => Err(StoreError::Unavailable),
        }
    }

    /// Whether the file `name` is on the volume.
    fn exists<S: Storage + ?Sized>(&self, fs: &mut S, name: &str) -> Result<bool, StoreError> {
        match fs.stat(&crate::store::join(&self.dir, name)) {
            Ok(_) => Ok(true),
            Err(Errno::NotFound) => Ok(false),
            Err(_) => Err(StoreError::Unavailable),
        }
    }

    /// Remove `name`, treating an absent file as removed.
    fn unlink<S: Storage + ?Sized>(&self, fs: &mut S, name: &str) -> Result<(), StoreError> {
        match fs.unlink(&crate::store::join(&self.dir, name)) {
            Ok(()) | Err(Errno::NotFound) => Ok(()),
            Err(_) => Err(StoreError::Unavailable),
        }
    }

    /// Mint the one-shot delegation for `name` to the live task `task`,
    /// carrying the shared extent ceiling on a writable one and no extent at
    /// all on a read.
    fn mint<S: Storage + ?Sized>(
        &self,
        fs: &mut S,
        name: &str,
        write: bool,
        task: u64,
    ) -> Result<u64, StoreError> {
        let ceiling = if write {
            APPDATA_BULK_FILE_MAX_BYTES
        } else {
            0
        };
        fs.grant(&crate::store::join(&self.dir, name), write, ceiling, task)
            .map_err(|err| match err {
                Errno::NotFound => StoreError::BlobNotFound,
                _ => StoreError::Unavailable,
            })
    }
}

/// One application's durable blob store.
pub struct BlobStore(Scope);

impl BlobStore {
    /// Resolve and authorise the blob store of the application `store` was
    /// opened for.
    ///
    /// `create` decides what happens when the application has never held a blob:
    /// a read or a listing passes `false` and gets an absent store, which
    /// answers empty; an open for writing passes `true`, which creates the
    /// directory. So no read pays a write, and a probe cannot bring a store into
    /// existence.
    ///
    /// **An unpinned store holds nothing**, whatever is on the volume, exactly
    /// as an unpinned configuration store reads as an empty document: with no
    /// ownership pin there is no attested owner, so there is nothing this
    /// service may serve. That is what makes the pin's authority cover the bulk
    /// tree structurally — a `create` reaches here only after [`AppStore::open`]
    /// has created or matched the pin, so a file cannot exist in a store whose
    /// owner was never recorded and a later publisher claiming the identifier
    /// cannot inherit one.
    ///
    /// # Errors
    ///
    /// [`StoreError::RootNotOwned`] when the gated bulk root is absent or is not
    /// the app-data service's own, [`StoreError::Unavailable`] when the volume
    /// cannot be reached.
    pub fn open<S: Storage + ?Sized>(
        fs: &mut S,
        store: &AppStore,
        create: bool,
    ) -> Result<Self, StoreError> {
        Scope::open(fs, store, BLOBS_DIR, create).map(Self)
    }

    /// Mint a one-shot descriptor delegation for the blob `name`, to the live
    /// task `task`.
    ///
    /// A read-only open of a blob the application does not hold answers
    /// [`StoreError::BlobNotFound`]; a read-write open creates it, which is what
    /// makes creation the mode's business rather than a separate flag.
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
        validate_bulk_name(name).map_err(|_| StoreError::StoreNameRefused)?;
        if !self.0.present {
            return Err(StoreError::BlobNotFound);
        }
        if !self.0.exists(fs, name)? {
            if !mode.is_write() {
                return Err(StoreError::BlobNotFound);
            }
            if self.0.count(fs, |_| true)? >= APPDATA_BLOB_MAX_COUNT {
                return Err(StoreError::BlobLimit);
            }
        }
        self.0.mint(fs, name, mode.is_write(), task)
    }

    /// Delete the blob `name`.
    ///
    /// Deleting one the application does not hold removes nothing and is not an
    /// error, so a refusal never reveals which blobs exist and a delete cannot
    /// bring a store into existence.
    ///
    /// # Errors
    ///
    /// [`StoreError::StoreNameRefused`] for a name outside the grammar,
    /// [`StoreError::Unavailable`] for an unreachable volume.
    pub fn delete<S: Storage + ?Sized>(&self, fs: &mut S, name: &str) -> Result<(), StoreError> {
        validate_bulk_name(name).map_err(|_| StoreError::StoreNameRefused)?;
        self.0.unlink(fs, name)
    }

    /// Every blob the application holds, with its length, sorted by name.
    ///
    /// # Errors
    ///
    /// [`StoreError::Unavailable`] for an unreachable volume.
    pub fn listing<S: Storage + ?Sized>(
        &self,
        fs: &mut S,
    ) -> Result<Vec<(String, u64)>, StoreError> {
        // Every name the enumeration answers is already one an application
        // could have asked for, so the durable scope adds no rule of its own.
        self.0.listing(fs, |_| true)
    }

    /// How many blobs the application holds and their total length.
    ///
    /// # Errors
    ///
    /// As [`Self::listing`].
    pub fn usage<S: Storage + ?Sized>(&self, fs: &mut S) -> Result<(u64, u64), StoreError> {
        Ok(usage_of(&self.listing(fs)?))
    }
}

/// One application's temporary files, for one boot.
pub struct TempStore {
    scope: Scope,
    names: TempNames,
}

impl TempStore {
    /// Resolve and authorise them, on [`BlobStore::open`]'s terms, under the
    /// naming rule of the running boot.
    ///
    /// # Errors
    ///
    /// As [`BlobStore::open`].
    pub fn open<S: Storage + ?Sized>(
        fs: &mut S,
        store: &AppStore,
        names: TempNames,
        create: bool,
    ) -> Result<Self, StoreError> {
        Ok(Self {
            scope: Scope::open(fs, store, TEMP_DIR, create)?,
            names,
        })
    }

    /// Create a fresh temporary file and mint a read-write delegation for it to
    /// the live task `task`, answering the handle and the name it was given.
    ///
    /// Reclaiming an earlier boot's leavings happens here, before admission, so
    /// a boot's scratch is charged to that boot alone and an application that
    /// filled the scope before a reboot is not refused after one. This is the
    /// one operation that pays the sweep, because it is the one that needs the
    /// room; every other answer simply does not see a stale file.
    ///
    /// # Errors
    ///
    /// [`StoreError::TempLimit`] when the application already holds
    /// [`APPDATA_TEMP_MAX_COUNT`] of them, [`StoreError::TempUnavailable`] when
    /// the generator could not draw a name or answered one already taken,
    /// [`StoreError::Unavailable`] for an unreachable volume, a directory that
    /// could not be created, or a delegation the kernel refused.
    pub fn create<S: Storage + ?Sized, E: Entropy + ?Sized>(
        &self,
        fs: &mut S,
        entropy: &mut E,
        task: u64,
    ) -> Result<(u64, String), StoreError> {
        if !self.scope.present {
            // A create always opens the scope with `create`, so the directory
            // is there unless it could not be made; composing a path into one
            // that is not is never the next step.
            return Err(StoreError::Unavailable);
        }
        if self.sweep(fs)? >= APPDATA_TEMP_MAX_COUNT {
            return Err(StoreError::TempLimit);
        }
        let mut slot = [0u8; TEMP_SLOT_LEN];
        entropy
            .fill(&mut slot)
            .map_err(|_| StoreError::TempUnavailable)?;
        let name = self.names.name(slot);
        // A drawn slot cannot collide with a live name in any run this machine
        // will see, so a name already taken is not a collision to retry past —
        // it is a generator that is not delivering the randomness it claimed,
        // and going on would hand the caller another instance's open scratch.
        if self.scope.exists(fs, &name)? {
            return Err(StoreError::TempUnavailable);
        }
        let handle = self.scope.mint(fs, &name, true, task)?;
        Ok((handle, name))
    }

    /// Delete the temporary file `name`.
    ///
    /// Releasing one the application does not hold removes nothing and is not
    /// an error, so a refusal never reveals which files exist and a release
    /// cannot bring a store into existence.
    ///
    /// # Errors
    ///
    /// As [`BlobStore::delete`].
    pub fn release<S: Storage + ?Sized>(&self, fs: &mut S, name: &str) -> Result<(), StoreError> {
        validate_bulk_name(name).map_err(|_| StoreError::StoreNameRefused)?;
        self.scope.unlink(fs, name)
    }

    /// How many temporary files of *this* boot the application holds, and their
    /// total length.
    ///
    /// # Errors
    ///
    /// [`StoreError::Unavailable`] for an unreachable volume.
    pub fn usage<S: Storage + ?Sized>(&self, fs: &mut S) -> Result<(u64, u64), StoreError> {
        Ok(usage_of(
            &self.scope.listing(fs, |name| self.names.is_live(name))?,
        ))
    }

    /// Delete every file here the running boot does not own, and answer how
    /// many of the application's own remain.
    ///
    /// Reclaiming and counting are one walk because they are one question —
    /// what is in this directory that is mine — and asking the volume twice on
    /// the create path would enumerate it for each half of the answer.
    fn sweep<S: Storage + ?Sized>(&self, fs: &mut S) -> Result<usize, StoreError> {
        let mut live = 0;
        for entry in self.scope.entries(fs)? {
            if self.names.is_live(&entry) {
                live += 1;
            } else {
                self.scope.unlink(fs, &entry)?;
            }
        }
        Ok(live)
    }
}

/// The count and total length a listing represents.
fn usage_of(listed: &[(String, u64)]) -> (u64, u64) {
    (
        listed.len() as u64,
        listed.iter().map(|(_, len)| *len).sum(),
    )
}

/// The quota reply for an application holding `blobs` and `temps`, each a
/// count and a total length.
///
/// Assembled in one place so the ceilings a caller is told about and the ones
/// admission enforces cannot drift apart.
#[must_use]
pub fn quota(blobs: (u64, u64), temps: (u64, u64)) -> BulkQuota {
    BulkQuota {
        blobs: blobs.0,
        blob_bytes: blobs.1,
        temps: temps.0,
        temp_bytes: temps.1,
        blob_max: APPDATA_BLOB_MAX_COUNT as u64,
        temp_max: APPDATA_TEMP_MAX_COUNT as u64,
        file_bytes_max: APPDATA_BULK_FILE_MAX_BYTES,
    }
}

/// Render `listing` as the fixed-width entry sequence the wire carries.
///
/// # Errors
///
/// [`StoreError::StoreNameRefused`] for a name the wire's own grammar refuses —
/// unreachable, because [`BlobStore::listing`] filters to that grammar, but
/// fail closed rather than trust the filter.
pub fn render_listing(listing: &[(String, u64)]) -> Result<Vec<u8>, StoreError> {
    let mut out = alloc::vec![0u8; listing.len() * tairix_abi::appdata_ipc::APPDATA_BLOB_ENTRY_LEN];
    for (slot, (name, len)) in out
        .chunks_mut(tairix_abi::appdata_ipc::APPDATA_BLOB_ENTRY_LEN)
        .zip(listing)
    {
        tairix_abi::appdata_ipc::encode_blob_entry(&BlobEntry { name, len: *len }, slot)
            .map_err(|_| StoreError::StoreNameRefused)?;
    }
    Ok(out)
}

/// Create `dir` and the per-app directory above it, in that order.
///
/// The bulk tree's per-app directory is created here rather than with the
/// configuration store's, because an application that never holds bulk data
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
#[path = "bulk_tests.rs"]
mod tests;
