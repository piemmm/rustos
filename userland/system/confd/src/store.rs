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
//! # Three scopes, and why only one of them is layered
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
//!
//! The **sealed** scope ([`VAULT_FILE`]) is the application's secrets,
//! encrypted at rest under a key derived per (account, application)
//! ([`crate::vault`]). It has no layer beneath it for a third reason of its
//! own: a layer an administrator or a bundle could ship would be a secret an
//! application had not written, and a secret an application did not put there
//! is not one it may be made to believe. It is also not a [`ConfigScope`] — no
//! configuration request can name it and no sealed request can name a
//! configuration document, in either direction, because the two families of
//! frame have no field in common that could select the other's file.

use alloc::string::String;
use alloc::vec::Vec;

use tairix_abi::appdata_ipc::{ConfigScope, APPDATA_SETTINGS_FILE};
use tairix_abi::appinfo::PublisherId;
use tairix_abi::{AppIdentity, Errno};
use tairix_appconf::{Document, MAX_DOCUMENT_LEN};
use tairix_users::{APPDATA_ROOT, CONFD_UID};
use zeroize::Zeroize;

use crate::owner::OwnerPin;
use crate::vault::{self, Entropy, MasterSecret, VaultError};
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

/// Name of an app's sealed configuration document inside its store — the
/// scope no other principal may read, and no request shape can name across
/// applications.
///
/// Like [`PUBLIC_FILE`] this name is the service's alone: the sealed scope has
/// no bundle-shipped layer and no machine-wide one, because an administrator
/// must not be able to plant a secret an application would then read as its
/// own.
pub const VAULT_FILE: &str = "secret.vault";

/// Name of the per-account app-data master-secret record, in the gated store
/// root.
///
/// It lives beside the per-app store directories rather than inside one
/// because it is the *account's* key material, from which every application's
/// vault key is derived. A leading dot cannot collide with a bundle
/// identifier — the identifier grammar forbids one — so the name is
/// unreachable as a store directory. It sits under `Settings/` rather than
/// `Library/` because `Library/` holds the evictable and boot-reaped scopes,
/// and key material may be in neither.
pub const MASTER_FILE: &str = ".vault-master";

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
    /// A sealed-scope operation was refused. Carried as the sealed scope's own
    /// vocabulary rather than flattened in here, so the key hierarchy's
    /// failures stay defined beside the key hierarchy.
    Vault(VaultError),
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
            // A sealed-scope refusal is about the caller's *own* data, so it
            // is reported precisely: an application can tell "your saved
            // secrets are damaged" from "storage is unreachable" and say so,
            // and no other principal learns anything either way.
            Self::Vault(VaultError::MasterSecretRefused | VaultError::VaultMalformed) => {
                Errno::BadMagic
            }
            Self::Vault(VaultError::VaultUnsealFailed) => Errno::SignatureInvalid,
            Self::Vault(VaultError::EntropyUnavailable) => Errno::EntropyNotReady,
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
            | Self::Unavailable
            // The sealed scope has no foreign path at all, so a vault refusal
            // is always the caller's own store's.
            | Self::Vault(_) => false,
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
            Self::Vault(err) => err.reason(),
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
    /// The account this store belongs to, as the kernel attested it. Held
    /// rather than passed per call so the master-secret record can only ever
    /// be read for the account whose root was resolved.
    uid: u32,
    /// The application this store belongs to, as the kernel attested it. Held
    /// for the same reason: the sealed scope's key is derived from it, and a
    /// key derived from an identity a call site supplied separately could
    /// disagree with the directory the store resolved to.
    identity: AppIdentity,
    /// Absolute path of `<home>/Settings/Apps`, the gated root the store was
    /// resolved under — where the account's master-secret record lives.
    root: String,
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
            uid,
            identity: *identity,
            policy_dir: join(POLICY_ROOT, identity.bundle_id()),
            root,
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
        replace_atomically(fs, &self.dir, scope_file(scope), text.as_bytes())
    }

    /// Read the app's own sealed document, or an empty one when it has sealed
    /// nothing.
    ///
    /// An unpinned store answers empty without reading anything, exactly as
    /// [`Self::document`] does: with no ownership pin there is no attested
    /// owner, so there is no document this store may be read out of. A sealed
    /// document that *is* there and cannot be opened is a refusal, never an
    /// empty answer.
    ///
    /// # Errors
    ///
    /// [`StoreError::Vault`] for a sealed document that is not a well-formed
    /// record, one whose authentication fails, or an account whose master
    /// secret is missing while a sealed document exists;
    /// [`StoreError::Unavailable`] for an unreachable volume.
    pub fn vault<S: Storage + ?Sized>(&self, fs: &mut S) -> Result<Document, StoreError> {
        let Some(sealed) = self.sealed_bytes(fs)? else {
            return Ok(Document::new());
        };
        let master = self
            .master(fs)?
            .ok_or(StoreError::Vault(VaultError::MasterSecretRefused))?;
        vault::open_document(&master.app_key(&self.identity), &sealed).map_err(StoreError::Vault)
    }

    /// Apply one change to the app's sealed document and publish it, as a
    /// single read-modify-seal-publish.
    ///
    /// `value` is the new value, or [`None`] for a removal. This is the whole
    /// sealed write: there is no staging session, so plaintext secret material
    /// exists here only for the span of one request — and because the service
    /// serves requests one at a time, two processes of one application sealing
    /// different secrets cannot lose each other's.
    ///
    /// Nothing is written when nothing changes: a value the sealed document
    /// already carries, and a removal of a key it does not, both return
    /// success having touched neither the document nor the account's key
    /// material. That is what stops a removal from bringing a store, a master
    /// secret, or a sealed document into existence.
    ///
    /// # Errors
    ///
    /// As [`Self::vault`], plus [`StoreError::Vault`] with
    /// [`VaultError::EntropyUnavailable`] when the nonce or a first master
    /// secret cannot be drawn, and [`StoreError::DocumentRefused`] for a change
    /// the format will not hold — in practice a document already at its setting
    /// or line bound, since the key and the value were validated against the
    /// same engine before this was called.
    pub fn seal_change<S: Storage + ?Sized, E: Entropy + ?Sized>(
        &self,
        fs: &mut S,
        entropy: &mut E,
        key: &str,
        value: Option<&str>,
    ) -> Result<(), StoreError> {
        let sealed = self.sealed_bytes(fs)?;
        // Removing a key from a vault that does not exist removes nothing, so
        // it must not create one — nor an account master secret, which a probe
        // could otherwise make the service draw.
        if sealed.is_none() && value.is_none() {
            return Ok(());
        }
        let master = match sealed {
            // A sealed document with no key to open it is a refusal, never a
            // fresh start: drawing a new master here would silently make the
            // existing vault unreadable for ever.
            Some(_) => self
                .master(fs)?
                .ok_or(StoreError::Vault(VaultError::MasterSecretRefused))?,
            None => self.master_or_draw(fs, entropy)?,
        };
        let app_key = master.app_key(&self.identity);
        let mut document = match &sealed {
            Some(bytes) => vault::open_document(&app_key, bytes).map_err(StoreError::Vault)?,
            None => Document::new(),
        };
        if let Some(value) = value {
            if document.get(key) == Some(value) {
                return Ok(());
            }
            document
                .set(key, value)
                .map_err(|_| StoreError::DocumentRefused)?;
        } else {
            if document.get(key).is_none() {
                return Ok(());
            }
            document.unset(key);
        }
        let record =
            vault::seal_document(&app_key, entropy, &document).map_err(StoreError::Vault)?;
        replace_atomically(fs, &self.dir, VAULT_FILE, &record)
    }

    /// The app's sealed document as it sits on the volume, or [`None`] when it
    /// has none.
    fn sealed_bytes<S: Storage + ?Sized>(&self, fs: &mut S) -> Result<Option<Vec<u8>>, StoreError> {
        if !self.pinned {
            return Ok(None);
        }
        match fs.read(&join(&self.dir, VAULT_FILE)) {
            Ok(bytes) => Ok(Some(bytes)),
            Err(Errno::NotFound) => Ok(None),
            Err(_) => Err(StoreError::Unavailable),
        }
    }

    /// The account's app-data master secret, or [`None`] when the account has
    /// none yet.
    ///
    /// Read afresh on every sealed operation and wiped when it goes out of
    /// scope. The service deliberately caches no master secret: a vault write
    /// is a rare, human-driven act, and holding an account's key material in
    /// the service's heap for the life of the machine would buy a file read it
    /// does not need.
    ///
    /// # Errors
    ///
    /// [`StoreError::Vault`] with [`VaultError::MasterSecretRefused`] for a
    /// record that is present but attests nothing, and
    /// [`StoreError::Unavailable`] for an unreachable volume.
    fn master<S: Storage + ?Sized>(&self, fs: &mut S) -> Result<Option<MasterSecret>, StoreError> {
        let mut bytes = match fs.read(&join(&self.root, MASTER_FILE)) {
            Ok(bytes) => bytes,
            Err(Errno::NotFound) => return Ok(None),
            Err(_) => return Err(StoreError::Unavailable),
        };
        let master = MasterSecret::decode(&bytes, self.uid);
        bytes.zeroize();
        master
            .map(Some)
            .ok_or(StoreError::Vault(VaultError::MasterSecretRefused))
    }

    /// The account's master secret, drawing and recording one when the account
    /// has none.
    ///
    /// The record is replaced atomically like any other: a torn write would
    /// leave a record that attests nothing, and because a malformed record is
    /// never replaced that would strand the account's vaults for good.
    fn master_or_draw<S: Storage + ?Sized, E: Entropy + ?Sized>(
        &self,
        fs: &mut S,
        entropy: &mut E,
    ) -> Result<MasterSecret, StoreError> {
        if let Some(master) = self.master(fs)? {
            return Ok(master);
        }
        let master = MasterSecret::draw(entropy).map_err(StoreError::Vault)?;
        let mut record = master.encode(self.uid);
        let written = replace_atomically(fs, &self.root, MASTER_FILE, &record);
        record.zeroize();
        written?;
        Ok(master)
    }
}

/// Replace `dir/file` with `bytes`, atomically.
///
/// The bytes are written whole to a sibling temporary name and then renamed
/// over the live name, so a crash mid-write leaves either the old contents or
/// the new ones. A failed rename leaves the old contents live and the temporary
/// behind; the next write overwrites the temporary, so a retry converges
/// without a repair pass.
///
/// Deriving the temporary from the live name rather than declaring one per file
/// is what keeps it always a sibling in the same directory: two applications
/// can never contend for it, two scopes can never collide on it, and the rename
/// can never cross a volume.
fn replace_atomically<S: Storage + ?Sized>(
    fs: &mut S,
    dir: &str,
    file: &str,
    bytes: &[u8],
) -> Result<(), StoreError> {
    let mut temp_name = String::from(file);
    temp_name.push_str(TEMP_SUFFIX);
    let temp = join(dir, &temp_name);
    let live = join(dir, file);
    fs.write(&temp, bytes)
        .map_err(|_| StoreError::Unavailable)?;
    fs.rename(&temp, &live).map_err(|_| StoreError::Unavailable)
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
