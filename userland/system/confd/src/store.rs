//! The per-app configuration store: where a store lives, who is allowed to be
//! served out of it, and how a change reaches the volume.
//!
//! Every path this module composes is derived from the caller's
//! kernel-attested identity and from the shared home-shape definition
//! ([`tairix_users`]) — never from anything a caller put on the wire. There is
//! no request shape that names a store, so there is no request shape that
//! reaches another application's data.

use alloc::string::String;
use alloc::vec::Vec;

use tairix_abi::appdata_ipc::APPDATA_SETTINGS_FILE;
use tairix_abi::appinfo::PublisherId;
use tairix_abi::{AppIdentity, Errno};
use tairix_appconf::{Document, MAX_DOCUMENT_LEN};
use tairix_users::{APPDATA_ROOT, CONFD_UID};

use crate::owner::OwnerPin;
use crate::Storage;

/// Name of an app's private configuration document inside its store.
///
/// The one definition lives in the app-data contract: the same name serves the
/// bundle's shipped defaults, so the service and the client cannot disagree
/// about which file the private scope is.
pub const SETTINGS_FILE: &str = APPDATA_SETTINGS_FILE;

/// Name the publish step writes a new document to before renaming it over
/// [`SETTINGS_FILE`].
///
/// A crash between the write and the rename therefore leaves either the old
/// document or the new one, never a torn one. The name is inside the app's own
/// store directory, so two apps can never contend for it, and the rename is
/// within one directory so it can never cross a volume.
const SETTINGS_TEMP_FILE: &str = "settings.conf.new";

/// Name of the ownership-pin record inside an app's store.
///
/// A leading dot cannot collide with a bundle identifier — the identifier
/// grammar forbids one — so this name is unreachable as a store directory.
pub const OWNER_FILE: &str = ".owner";

/// Root of the machine-wide administrator policy layer: the optional,
/// read-only document an image or an installer may ship under
/// `/System/Settings/<bundle-id>/` to change an application's defaults without
/// touching any user's own file.
///
/// It sits *below* the user's own document in precedence, so it sets defaults
/// rather than overriding a choice the user made. `/System` is mounted
/// read-only at runtime, so nothing the service does can write here.
const POLICY_ROOT: &str = "/System/Settings";

/// The directory `/Users` projects, under which every account's home lives.
///
/// The installed-system contract fixes this as the only place user-owned files
/// live, which is what makes scanning it a complete answer to "where is this
/// uid's home?" — and what lets the service resolve a home with no reach into
/// the credential database at all.
const USERS_ROOT: &str = "/Users";

/// The permission bits a per-app store directory carries: owner-only, where
/// the owner is the app-data service.
///
/// The gated root above it already refuses every other principal, so this is
/// defence in depth rather than the boundary itself.
const STORE_DIR_MODE: u32 = 0o700;

/// Why a store could not be opened or a change could not be published.
///
/// Every variant is a refusal that fails closed, and each one is distinct
/// because the audit record and the caller's own reported reason differ: an
/// absent store is normal, an unreachable volume is transient, and a pin
/// mismatch or a foreign root is an attack indication.
#[derive(Copy, Clone, Debug, Eq, PartialEq)]
pub enum StoreError {
    /// The caller is not running a verified bundle, so it has no store at all.
    NoAppIdentity,
    /// No directory under `/Users` is owned by the caller's uid, so the
    /// account has no home the store could live in.
    NoHome,
    /// The gated store root is absent, or is not owned by the app-data
    /// service — so it is not the root the provisioners created and nothing
    /// may be served out of it.
    RootNotOwned,
    /// The store's ownership pin names a different publisher: a developer
    /// other than the one that created this data is claiming the identifier.
    PublisherMismatch,
    /// The store's ownership pin is present but malformed, so it attests
    /// nothing.
    PinMalformed,
    /// The document on the volume is outside the format's bounds, or the
    /// change would put it outside them.
    DocumentRefused,
    /// The volume could not be reached — the encrypted root is not yet
    /// unlocked, or an I/O error.
    Unavailable,
}

impl StoreError {
    /// The typed error a caller receives. Deliberately coarse where being
    /// precise would tell an unauthorised caller something: a caller with no
    /// identity, a foreign root, and a pin mismatch all read as a permission
    /// refusal, and only the audit log distinguishes them.
    #[must_use]
    pub const fn errno(self) -> Errno {
        match self {
            Self::NoAppIdentity
            | Self::RootNotOwned
            | Self::PublisherMismatch
            | Self::PinMalformed => Errno::PermissionDenied,
            Self::NoHome => Errno::NotFound,
            Self::DocumentRefused => Errno::OutOfRange,
            Self::Unavailable => Errno::DeviceOffline,
        }
    }

    /// A stable one-line reason for the audit record.
    #[must_use]
    pub const fn reason(self) -> &'static str {
        match self {
            Self::NoAppIdentity => "caller runs no verified bundle, so it has no app-data store",
            Self::NoHome => "no home directory under /Users is owned by the caller",
            Self::RootNotOwned => "the app-data store root is not owned by the app-data service",
            Self::PublisherMismatch => "the store belongs to a different publisher",
            Self::PinMalformed => "the store's ownership pin is malformed",
            Self::DocumentRefused => "the configuration document is outside the format's bounds",
            Self::Unavailable => "the store volume is not reachable",
        }
    }
}

/// One app's store, resolved and authorised: the directory its documents live
/// in, ready for a read or a publish.
///
/// Holding one is proof that the caller's app identity was attested, that the
/// gated root belongs to the app-data service, and that the ownership pin
/// either named this publisher or was created for it.
pub struct AppStore {
    /// Absolute path of `<home>/Settings/Apps/<bundle-id>`, with no trailing
    /// separator.
    dir: String,
    /// Absolute path of `/System/Settings/<bundle-id>`, the policy layer.
    policy_dir: String,
    /// Whether an ownership pin was found — i.e. whether this app has ever
    /// written anything.
    ///
    /// An unpinned store has no user layer to read, and this is what makes
    /// that structural rather than a matter of a file happening to be absent:
    /// [`Self::document`] answers empty without touching the volume, so no
    /// document whose owner was never attested can be read at all.
    pinned: bool,
}

/// The gated store roots resolved so far, one per account.
///
/// Without it every settings read would re-list `/Users` and stat each entry to
/// find the caller's home — a directory scan on the hot path, growing with the
/// number of accounts. The cache holds only the resolved **path**: the
/// ownership that authorises it is re-checked on every use ([`Self::resolve`]),
/// so an administrator who reassigns a home (the one act that could invalidate
/// a resolution, and it needs `CAP_FS_CHOWN`) cannot make a stale entry serve
/// the wrong account's store. Only successful resolutions are remembered, so an
/// account created after the service started still resolves.
pub struct RootCache {
    roots: Vec<(u32, String)>,
}

impl RootCache {
    /// An empty cache.
    #[must_use]
    pub const fn new() -> Self {
        Self { roots: Vec::new() }
    }

    /// The gated store root of the account `uid` owns.
    ///
    /// # Errors
    ///
    /// [`StoreError::NoHome`] when no directory under `/Users` is owned by
    /// `uid`, [`StoreError::RootNotOwned`] when the root is absent or is not
    /// the app-data service's own, [`StoreError::Unavailable`] when the volume
    /// cannot be reached.
    pub fn resolve<S: Storage + ?Sized>(
        &mut self,
        fs: &mut S,
        uid: u32,
    ) -> Result<String, StoreError> {
        if let Some(index) = self.roots.iter().position(|(known, _)| *known == uid) {
            let home = home_of_root(&self.roots[index].1);
            match fs.owner_of(home) {
                Ok(owner) if owner == uid => return Ok(self.roots[index].1.clone()),
                Ok(_) | Err(Errno::NotFound) => {
                    // The home was reassigned or removed; the remembered path
                    // is no longer this account's, so forget it and look again.
                    self.roots.remove(index);
                }
                Err(_) => return Err(StoreError::Unavailable),
            }
        }
        let root = gated_root(fs, uid)?;
        self.roots.push((uid, root.clone()));
        Ok(root)
    }

    /// Number of remembered roots. Test and diagnostic surface.
    #[must_use]
    pub fn len(&self) -> usize {
        self.roots.len()
    }

    /// Whether nothing has been resolved yet.
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.roots.is_empty()
    }
}

impl Default for RootCache {
    fn default() -> Self {
        Self::new()
    }
}

/// The home directory of a gated store root — the root path with its fixed
/// `<parent>/Apps` tail removed.
///
/// The tail is composed by [`gated_root`] from the shared home-shape
/// definition, so trimming exactly two components is the inverse of that
/// composition and cannot be steered by anything a caller supplies.
fn home_of_root(root: &str) -> &str {
    let mut cut = root;
    for _ in 0..2 {
        cut = match cut.rfind('/') {
            Some(index) => &cut[..index],
            None => return cut,
        };
    }
    cut
}

impl AppStore {
    /// Resolve and authorise the store of the app `identity` running as `uid`.
    ///
    /// `create` decides what happens when the app has never written anything:
    /// a read passes `false` and gets an **unpinned** store — the machine-wide
    /// policy layer is still readable, so an administrator's default applies
    /// from an app's very first launch, and no write is paid for a read. A
    /// publish passes `true`, which creates the directory and its ownership
    /// pin.
    ///
    /// # Errors
    ///
    /// Every [`StoreError`]: the caller has no attested app identity, the
    /// account has no home, the gated root is not the service's own, the pin
    /// names another publisher or is malformed, or the volume is unreachable.
    pub fn open<S: Storage + ?Sized>(
        fs: &mut S,
        roots: &mut RootCache,
        uid: u32,
        identity: &AppIdentity,
        create: bool,
    ) -> Result<Self, StoreError> {
        let root = roots.resolve(fs, uid)?;
        let dir = join(&root, identity.bundle_id());
        let pin_path = join(&dir, OWNER_FILE);
        let pinned = match fs.read(&pin_path) {
            Ok(bytes) => {
                let pin = OwnerPin::decode(&bytes).ok_or(StoreError::PinMalformed)?;
                if pin.publisher() != identity.publisher() {
                    return Err(StoreError::PublisherMismatch);
                }
                true
            }
            Err(Errno::NotFound) if !create => false,
            Err(Errno::NotFound) => {
                create_store(fs, &dir, &pin_path, identity.publisher())?;
                true
            }
            Err(_) => return Err(StoreError::Unavailable),
        };
        Ok(Self {
            policy_dir: join(POLICY_ROOT, identity.bundle_id()),
            dir,
            pinned,
        })
    }

    /// Whether this app has ever written to its store — i.e. whether an
    /// ownership pin attests who owns it.
    #[must_use]
    pub const fn is_pinned(&self) -> bool {
        self.pinned
    }

    /// Read the app's own committed configuration document, or an empty one
    /// when it has never been written.
    ///
    /// An unpinned store answers empty without reading anything: with no pin
    /// there is no attested owner, so there is no document this store may be
    /// read out of.
    ///
    /// # Errors
    ///
    /// [`StoreError::DocumentRefused`] for a document outside the format's
    /// bounds, [`StoreError::Unavailable`] for an unreachable volume.
    pub fn document<S: Storage + ?Sized>(&self, fs: &mut S) -> Result<Document, StoreError> {
        if !self.pinned {
            return Ok(Document::new());
        }
        read_document(fs, &join(&self.dir, SETTINGS_FILE))
    }

    /// Read the machine-wide policy document — the layer beneath the app's own
    /// — or an empty one when the machine ships none.
    ///
    /// # Errors
    ///
    /// As [`Self::document`].
    pub fn policy_document<S: Storage + ?Sized>(&self, fs: &mut S) -> Result<Document, StoreError> {
        read_document(fs, &join(&self.policy_dir, SETTINGS_FILE))
    }

    /// The document a caller is served: the machine-wide policy layer with the
    /// app's own settings applied over it.
    ///
    /// The result is *canonical* — one line per setting, no comments, no
    /// duplicates — because it is two documents made one, and because the
    /// caller parses it rather than editing it. The app's own file keeps its
    /// comments and its ordering; only this view of it is normalised.
    ///
    /// # Errors
    ///
    /// [`StoreError::DocumentRefused`] when the two layers together exceed the
    /// format's setting or line bound, else as [`Self::document`].
    pub fn merged_document<S: Storage + ?Sized>(&self, fs: &mut S) -> Result<Document, StoreError> {
        let policy = self.policy_document(fs)?;
        let own = self.document(fs)?;
        let mut merged = Document::new();
        // Policy first, the app's own second: a key in both ends up with the
        // user's value, which is what makes an override an override.
        for setting in policy.settings().chain(own.settings()) {
            merged
                .set(setting.key, setting.value)
                .map_err(|_| StoreError::DocumentRefused)?;
        }
        Ok(merged)
    }

    /// Publish `document` as the app's own configuration, atomically.
    ///
    /// The rendered text is written whole to a sibling temporary name and then
    /// renamed over the live document, so a crash mid-publish leaves either
    /// the old document or the new one. A rendered document that would exceed
    /// the format's own byte bound is refused rather than written.
    ///
    /// # Errors
    ///
    /// [`StoreError::DocumentRefused`] for a rendering past
    /// [`MAX_DOCUMENT_LEN`], [`StoreError::Unavailable`] for a failed write or
    /// rename.
    pub fn publish<S: Storage + ?Sized>(
        &self,
        fs: &mut S,
        document: &Document,
    ) -> Result<(), StoreError> {
        let text = document.render();
        if text.len() > MAX_DOCUMENT_LEN {
            return Err(StoreError::DocumentRefused);
        }
        let temp = join(&self.dir, SETTINGS_TEMP_FILE);
        let live = join(&self.dir, SETTINGS_FILE);
        fs.write(&temp, text.as_bytes())
            .map_err(|_| StoreError::Unavailable)?;
        // A failed rename leaves the old document live and the temporary
        // behind; the next publish overwrites the temporary, so a retry
        // converges without a repair pass.
        fs.rename(&temp, &live).map_err(|_| StoreError::Unavailable)
    }
}

/// Read and parse the document at `path`, treating an absent file as an empty
/// document — an app that has never saved anything is not an error.
fn read_document<S: Storage + ?Sized>(fs: &mut S, path: &str) -> Result<Document, StoreError> {
    let bytes = match fs.read(path) {
        Ok(bytes) => bytes,
        Err(Errno::NotFound) => return Ok(Document::new()),
        Err(_) => return Err(StoreError::Unavailable),
    };
    let text = core::str::from_utf8(&bytes).map_err(|_| StoreError::DocumentRefused)?;
    Document::parse(text).map_err(|_| StoreError::DocumentRefused)
}

/// Create an app's store directory and its ownership pin, in that order.
///
/// The pin is written last, so a store directory that carries one is a store
/// whose owner is recorded; the directory is stamped owner-only under the
/// service's own identity.
fn create_store<S: Storage + ?Sized>(
    fs: &mut S,
    dir: &str,
    pin_path: &str,
    publisher: PublisherId,
) -> Result<(), StoreError> {
    match fs.mkdir(dir, STORE_DIR_MODE) {
        // An existing directory with no pin is the interrupted tail of an
        // earlier create: finish it rather than refusing forever.
        Ok(()) | Err(Errno::AlreadyExists) => {}
        Err(_) => return Err(StoreError::Unavailable),
    }
    fs.write(pin_path, &OwnerPin::new(publisher).encode())
        .map_err(|_| StoreError::Unavailable)
}

/// Resolve the gated per-app store root of the account `uid` owns, proving on
/// the way that it is the root the provisioners created.
///
/// Two checks, both necessary. The home must be **owned by the caller's uid**,
/// which is what makes the resolution a uid→home answer rather than a guess —
/// and it needs no reach into the credential database, so this service holds
/// none. The gated root must be **owned by the app-data service**, because the
/// store's parent directory is writable by the account: an application could
/// otherwise plant a world-traversable directory of that name and have this
/// service serve forged settings out of it. A capability gate alone would not
/// catch that — this service holds the capability either way.
fn gated_root<S: Storage + ?Sized>(fs: &mut S, uid: u32) -> Result<String, StoreError> {
    let home = home_of(fs, uid)?;
    let root = join(&join(&home, crate::APPDATA_PARENT), APPDATA_ROOT);
    match fs.owner_of(&root) {
        Ok(CONFD_UID_RAW) => Ok(root),
        Ok(_) | Err(Errno::NotFound) => Err(StoreError::RootNotOwned),
        Err(_) => Err(StoreError::Unavailable),
    }
}

/// The app-data service's own uid, as a pattern-matchable constant.
const CONFD_UID_RAW: u32 = CONFD_UID.0;

/// The absolute path of the home directory `uid` owns.
///
/// `/Users` is the only place user-owned files live, so walking it and
/// matching the owning uid is a complete answer — and one that reads the truth
/// on the volume rather than a record that could disagree with it.
fn home_of<S: Storage + ?Sized>(fs: &mut S, uid: u32) -> Result<String, StoreError> {
    let names = fs.list_dir(USERS_ROOT).map_err(|err| match err {
        Errno::NotFound => StoreError::NoHome,
        _ => StoreError::Unavailable,
    })?;
    for name in names {
        let candidate = join(USERS_ROOT, &name);
        match fs.owner_of(&candidate) {
            Ok(owner) if owner == uid => return Ok(candidate),
            // Someone else's home, a name that vanished between the listing
            // and the check, or one this service may not stat: none of them is
            // this uid's home, and one broken home must not deny every
            // account.
            Ok(_) | Err(Errno::NotFound | Errno::PermissionDenied) => {}
            // The volume itself went away mid-scan, so the answer is unknown
            // rather than negative — say so instead of reporting no home.
            Err(_) => return Err(StoreError::Unavailable),
        }
    }
    Err(StoreError::NoHome)
}

/// `parent/child`, with exactly one separator.
fn join(parent: &str, child: &str) -> String {
    let mut path = String::with_capacity(parent.len() + 1 + child.len());
    path.push_str(parent.trim_end_matches('/'));
    path.push('/');
    path.push_str(child);
    path
}

#[cfg(test)]
#[path = "store_tests.rs"]
pub(crate) mod tests;
