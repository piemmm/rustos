//! The per-app configuration store: where a store lives, who is allowed to be
//! served out of it, and how a change reaches the volume.
//!
//! Every path this module composes is derived from the caller's
//! kernel-attested identity and from the shared home-shape definition
//! ([`tairix_users`]) — never from anything a caller put on the wire, with the
//! single exception of the bundle identifier a foreign published read
//! ([`published_document`]) names, which crossed the wire only after the one
//! identifier grammar accepted it.
//!
//! # Two scopes, and why only one of them is layered
//!
//! [`ConfigScope::Private`] is the user's settings for an application, so it
//! reads through the machine-wide policy layer an administrator may ship
//! beneath it: an unset key falls back to the machine's answer rather than to
//! nothing.
//!
//! [`ConfigScope::Public`] is what the application *publishes about itself*
//! for other applications to read, and it is deliberately **one document with
//! no layer beneath it**. Two reasons, and the first is structural: a bundle's
//! own directory is not something this service can name — nothing attested
//! gives it a bundle path — so a bundle-shipped published document could never
//! be read on the foreign path at all, and a layer that only worked for the
//! publishing app itself would mean two applications disagreeing about what a
//! third publishes. The second is the scope's contract: a reader must be able
//! to attribute a published value to the application, and a machine-wide layer
//! beneath it would let an administrator make an application appear to say
//! something it never said.

use alloc::string::String;
use alloc::vec::Vec;

use tairix_abi::appdata_ipc::{ConfigScope, APPDATA_SETTINGS_FILE};
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

/// Name of an app's published configuration document inside its store — the
/// scope any application may read.
///
/// Unlike [`SETTINGS_FILE`] this name is the service's alone: the published
/// scope has no bundle-shipped layer (see the module documentation), so no
/// client ever composes a path to it.
pub const PUBLIC_FILE: &str = "public.conf";

/// Suffix of the sibling name a publish writes a new document to before
/// renaming it over the live one.
///
/// A crash between the write and the rename therefore leaves either the old
/// document or the new one, never a torn one. Deriving the temporary from the
/// live name rather than declaring one per scope is what makes it always a
/// sibling in the app's own store directory: two applications can never
/// contend for it, two scopes can never collide on it, and the rename can
/// never cross a volume.
const TEMP_SUFFIX: &str = ".new";

/// Name of the ownership-pin record inside an app's store.
///
/// A leading dot cannot collide with a bundle identifier — the identifier
/// grammar forbids one — so this name is unreachable as a store directory.
pub const OWNER_FILE: &str = ".owner";

/// Root of the machine-wide administrator policy layer: the optional,
/// read-only document an image or an installer may ship under
/// `/System/Settings/<bundle-id>/` to change an application's private defaults
/// without touching any user's own file.
///
/// It sits *below* the user's own document in precedence, so it sets defaults
/// rather than overriding a choice the user made. `/System` is mounted
/// read-only at runtime, so nothing the service does can write here. It layers
/// the private scope alone — see the module documentation for why the
/// published scope has no layer beneath it.
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

    /// Whether this refusal is a defect of the *store being read* rather than
    /// of the caller, its account, or the volume.
    ///
    /// A foreign published read answers the empty document for these, audited:
    /// another application's broken pin or over-long document is nothing the
    /// reader can act on, and reporting it would make the read an oracle for
    /// the state of a store the caller has no business knowing about. The
    /// caller-side and volume refusals are reported as themselves, because
    /// those the caller *can* act on.
    #[must_use]
    pub const fn is_target_defect(self) -> bool {
        match self {
            Self::PinMalformed | Self::DocumentRefused => true,
            Self::NoAppIdentity
            | Self::NoHome
            | Self::RootNotOwned
            | Self::PublisherMismatch
            | Self::Unavailable => false,
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

/// One app's own store, resolved and authorised: the directory its documents
/// live in, ready for a read or a publish in either scope.
///
/// Holding one is proof that the caller's app identity was attested, that the
/// gated root belongs to the app-data service, and that the ownership pin
/// either named this publisher or was created for it.
pub struct AppStore {
    /// Absolute path of `<home>/Settings/Apps/<bundle-id>`, with no trailing
    /// separator.
    dir: String,
    /// Absolute path of `/System/Settings/<bundle-id>`, the private scope's
    /// policy layer.
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
    ///
    /// One pin governs the whole store rather than one per scope: the pin
    /// records who owns the *data*, and both scopes are the same app's.
    #[must_use]
    pub const fn is_pinned(&self) -> bool {
        self.pinned
    }

    /// Read the app's own committed document for `scope`, or an empty one when
    /// it has never been written.
    ///
    /// An unpinned store answers empty without reading anything: with no pin
    /// there is no attested owner, so there is no document this store may be
    /// read out of.
    ///
    /// # Errors
    ///
    /// [`StoreError::DocumentRefused`] for a document outside the format's
    /// bounds, [`StoreError::Unavailable`] for an unreachable volume.
    pub fn document<S: Storage + ?Sized>(
        &self,
        fs: &mut S,
        scope: ConfigScope,
    ) -> Result<Document, StoreError> {
        if !self.pinned {
            return Ok(Document::new());
        }
        read_document(fs, &join(&self.dir, scope_file(scope)))
    }

    /// Read the machine-wide policy document — the layer beneath the app's own
    /// private scope — or an empty one when the machine ships none.
    ///
    /// # Errors
    ///
    /// As [`Self::document`].
    pub fn policy_document<S: Storage + ?Sized>(&self, fs: &mut S) -> Result<Document, StoreError> {
        read_document(fs, &join(&self.policy_dir, SETTINGS_FILE))
    }

    /// The document a caller is served for `scope`.
    ///
    /// For [`ConfigScope::Private`] that is the machine-wide policy layer with
    /// the app's own settings applied over it; for [`ConfigScope::Public`] it
    /// is the app's own published document alone, because that scope has no
    /// layer beneath it (see the module documentation).
    ///
    /// The result is *canonical* — one line per setting, no comments, no
    /// duplicates — because a merge of two documents is one, and because the
    /// caller parses it rather than editing it. The app's own file keeps its
    /// comments and its ordering; only this view of it is normalised.
    ///
    /// # Errors
    ///
    /// [`StoreError::DocumentRefused`] when the layers together exceed the
    /// format's setting or line bound, else as [`Self::document`].
    pub fn merged_document<S: Storage + ?Sized>(
        &self,
        fs: &mut S,
        scope: ConfigScope,
    ) -> Result<Document, StoreError> {
        let own = self.document(fs, scope)?;
        let ConfigScope::Private = scope else {
            return Ok(own);
        };
        let policy = self.policy_document(fs)?;
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

    /// Publish `document` as the app's own `scope` document, atomically.
    ///
    /// The rendered text is written whole to a sibling temporary name and then
    /// renamed over the live document, so a crash mid-publish leaves either
    /// the old document or the new one. A rendered document that would exceed
    /// the format's own byte bound is refused rather than written.
    ///
    /// One scope at a time, because one rename replaces one name: a publish
    /// that claimed to replace both documents at once would be claiming an
    /// atomicity no filesystem offers.
    ///
    /// # Errors
    ///
    /// [`StoreError::DocumentRefused`] for a rendering past
    /// [`MAX_DOCUMENT_LEN`], [`StoreError::Unavailable`] for a failed write or
    /// rename.
    pub fn publish<S: Storage + ?Sized>(
        &self,
        fs: &mut S,
        scope: ConfigScope,
        document: &Document,
    ) -> Result<(), StoreError> {
        let text = document.render();
        if text.len() > MAX_DOCUMENT_LEN {
            return Err(StoreError::DocumentRefused);
        }
        let file = scope_file(scope);
        let mut temp_name = String::from(file);
        temp_name.push_str(TEMP_SUFFIX);
        let temp = join(&self.dir, &temp_name);
        let live = join(&self.dir, file);
        fs.write(&temp, text.as_bytes())
            .map_err(|_| StoreError::Unavailable)?;
        // A failed rename leaves the old document live and the temporary
        // behind; the next publish overwrites the temporary, so a retry
        // converges without a repair pass.
        fs.rename(&temp, &live).map_err(|_| StoreError::Unavailable)
    }
}

/// The document the application `bundle_id` names publishes, inside the store
/// of the account `uid` owns, or an empty document when it publishes nothing.
///
/// A foreign read is **one read and no handle**. There is deliberately no value
/// standing for "another application's store": a type with a directory in it
/// would be one a later call site could publish through, and the cheapest way
/// to guarantee that a foreign store is read-only and public-only is for no
/// such value to exist. Stores are per-user, so this never crosses an account
/// either — it reads what the *calling* account's copy of that application
/// publishes.
///
/// `bundle_id` is the one value here that came off the wire, and it arrives
/// having already passed the single identifier grammar
/// ([`tairix_abi::validate_bundle_id`], applied by the request decoder) — so it
/// is a single path component that cannot traverse, hide, or case-fold into
/// another application's name.
///
/// No pin *comparison* happens: a foreign reader is not the owner, so there is
/// nothing to compare against. The pin is still required to be **present and
/// well formed**, which is what attests that the directory is a store this
/// service created: the gated root is owned by the service and mode `0700`, so
/// nothing else can create a child in it, and a decodable pin inside such a
/// child can only have been written here.
///
/// The answer is the *committed* document — what every other application sees,
/// never a staged edit of the publisher's. A publisher that wants to know what
/// it has staged reads its own [`ConfigScope::Public`] scope.
///
/// # Errors
///
/// [`StoreError::NoHome`], [`StoreError::RootNotOwned`], or
/// [`StoreError::Unavailable`] for the caller's own account and volume;
/// [`StoreError::PinMalformed`] for a target whose ownership record attests
/// nothing, and [`StoreError::DocumentRefused`] for one whose published
/// document is outside the format's bounds. The last two are the target's
/// defects rather than the reader's, and the dispatcher answers them as an
/// empty document ([`StoreError::is_target_defect`]).
pub fn published_document<S: Storage + ?Sized>(
    fs: &mut S,
    roots: &mut RootCache,
    uid: u32,
    bundle_id: &str,
) -> Result<Document, StoreError> {
    let dir = join(&roots.resolve(fs, uid)?, bundle_id);
    match fs.read(&join(&dir, OWNER_FILE)) {
        Ok(bytes) => {
            OwnerPin::decode(&bytes).ok_or(StoreError::PinMalformed)?;
        }
        // The named application has no store in this account: it has never run
        // here, or has never written. Either way it publishes nothing, which is
        // the same answer as a store with an empty published document — so a
        // read is never an oracle for anything but what an application chose to
        // publish.
        Err(Errno::NotFound) => return Ok(Document::new()),
        Err(_) => return Err(StoreError::Unavailable),
    }
    read_document(fs, &join(&dir, PUBLIC_FILE))
}

/// The document file name `scope` lives in.
///
/// The one mapping from a scope to a name, so no call site can compose a path
/// to the wrong document.
const fn scope_file(scope: ConfigScope) -> &'static str {
    match scope {
        ConfigScope::Private => SETTINGS_FILE,
        ConfigScope::Public => PUBLIC_FILE,
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
