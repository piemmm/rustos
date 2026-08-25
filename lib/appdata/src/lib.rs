//! The app-data client (`tairix-appdata`): how an application reaches its own
//! settings, and the only way it can.
//!
//! ```ignore
//! let mut settings = Settings::open(&mut RtHost, OWN_WORD);
//! let size = settings.u32("font.size")?.unwrap_or(14);
//! settings.set_u32("font.size", 16)?;
//! settings.commit()?;                       // one atomic publish
//! ```
//!
//! Nothing here takes a store path or a user, and nothing but
//! [`read_published`] takes a bundle identifier: the app-data service derives
//! all of those from the identity the kernel attests for the calling task. So
//! an application cannot reach outside its own scope by construction rather
//! than by a check some caller might forget, and this library has no
//! privileged surface to misuse.
//!
//! One entry point per scope: [`Settings::open`] for the **private** document,
//! [`Settings::open_published`] for what the application publishes about
//! itself and [`read_published`] for another application's, [`Vault`] for its
//! **sealed** secrets, and [`blobs`] and [`temp`] for the durable and per-boot
//! **bulk** scopes, which are descriptors rather than documents so their bytes
//! never cross the channel ([`bulk_quota`] reports both).
//!
//! Four contracts a caller depends on:
//!
//! - [`Settings::open`] costs the one round trip and never fails: a store the
//!   service cannot serve leaves the bundle's shipped defaults standing and
//!   [`Settings::store_refusal`] says why. [`Vault::open`] does fail, because
//!   "I could not read your secrets" is not "you have none".
//! - Every read after the open is a lookup in memory, and every
//!   [`Settings::set`] is memory too until [`Settings::commit`] publishes the
//!   changed keys as one atomic document replacement. A handle that is never
//!   committed changes nothing on the volume.
//! - A commit ends by re-reading the store, so the handle reflects what the
//!   service holds — which matters after a [`Settings::unset`], where the
//!   effective value comes back from a layer below.
//! - The sealed scope has no staging and no commit: the service seals each
//!   write before it replies.
//!
//! Only the private scope is layered, and layer 1 of it is **this library's**:
//! the bundle's own `DefaultSettings/settings.conf`, which needs a bundle path
//! nothing attested gives the service. That is the one thing
//! [`Settings::open`]'s command word selects. The machine-wide policy layer
//! and the user's own document arrive already merged, in one call.
//!
//! The crate is `no_std` (with `alloc`) and performs no I/O of its own: every
//! syscall sits behind the [`AppDataHost`] seam, so the whole client is
//! exercised on the host. The `rt` feature supplies the real seam.
//!
//! The scope walkthrough and the design behind it are
//! `docs/src/lib/appdata.md`.

#![no_std]
#![forbid(unsafe_code)]
#![deny(missing_docs)]

extern crate alloc;

use alloc::collections::BTreeSet;
use alloc::string::String;
use alloc::vec::Vec;

use tairix_abi::appdata_ipc::{
    decode_document_reply, decode_quota_reply, AppDataRequest, BulkQuota, ConfigDocument,
    ConfigScope, APPDATA_DOCUMENT_HEADER_LEN, APPDATA_DOCUMENT_MAX, APPDATA_HEADER_LEN,
    APPDATA_MAX_REQUEST, APPDATA_QUOTA_REPLY_LEN, APPDATA_SETTINGS_FILE,
};
use tairix_abi::appinfo::{BundleEntry, BUNDLE_ID_MAX};
use tairix_abi::reply::{decode_status_reply, STATUS_REPLY_LEN};
use tairix_abi::Errno;
use tairix_appconf::{
    as_bool, as_i64, as_permille, as_u32, bool_text, ConfError, Document, Lookup, Setting,
};
use zeroize::Zeroize;

#[cfg(any(test, feature = "test-util"))]
pub mod fake;
#[cfg(feature = "rt")]
mod rt;

#[cfg(feature = "rt")]
pub use rt::RtHost;

/// Document bytes a first read asks the service for.
///
/// A realistic store is a few dozen short settings, so this covers one in a
/// single call. It is deliberately far below the format's 64 KiB ceiling: a
/// larger store costs one extra round trip, where sizing every application's
/// start-up buffer to the ceiling would cost all of them the allocation.
const READ_PROBE: usize = 4096;

/// How many times a read may be re-issued at the length the service declared.
///
/// The second attempt asks for exactly the length the service named, so it
/// succeeds unless another process of the same application published a
/// *larger* document in between. One spare attempt past that covers a race no
/// real writer produces, and the bound is what makes the loop terminate rather
/// than chase a writer for ever.
const READ_ATTEMPTS: usize = 3;

/// The syscalls this client needs, behind one seam.
///
/// They are the *whole* of its contact with the world: one call to the
/// app-data service, one file read, and the ordered directories the calling
/// program's own bundle may be found in. Keeping all three behind a trait is
/// what makes the layered read, the capacity negotiation, and the commit
/// host-testable without a running service or a volume.
pub trait AppDataHost {
    /// Issue one call to the app-data endpoint, returning the reply length
    /// written into `reply`.
    ///
    /// # Errors
    ///
    /// Any [`Errno`] the transport surfaces — [`Errno::NotFound`] for an
    /// endpoint nothing has bound (the service is not running), or the
    /// service's own typed refusal.
    fn call(&mut self, request: &[u8], reply: &mut [u8]) -> Result<usize, Errno>;

    /// Read the whole file at `path`, refusing one longer than `cap`.
    ///
    /// # Errors
    ///
    /// [`Errno::NotFound`] when the path does not exist — the ordinary state
    /// of a bundle that ships no defaults — and any other [`Errno`] for a
    /// failed read.
    fn read_file(&mut self, path: &str, cap: usize) -> Result<Vec<u8>, Errno>;

    /// The candidate bundle directories the command word `word` names, in the
    /// order the system resolves them.
    ///
    /// This is the *shared* resolution order — the one the shell launches
    /// through and `man` reads a bundle's help from — so a program's shipped
    /// defaults and its help can never come from different bundles. It is the
    /// host's method because the order reads the session's `HOME` and `PATH`,
    /// which only the running process can see.
    fn bundle_candidates(&mut self, word: &str) -> Vec<String>;
}

/// A borrowed host is a host, so a caller that must keep its own can lend it
/// to a value that would otherwise take ownership — a store adapter that
/// holds the seam, say, whose owner still has to inspect the service
/// afterwards.
impl<H: AppDataHost + ?Sized> AppDataHost for &mut H {
    fn call(&mut self, request: &[u8], reply: &mut [u8]) -> Result<usize, Errno> {
        (**self).call(request, reply)
    }

    fn read_file(&mut self, path: &str, cap: usize) -> Result<Vec<u8>, Errno> {
        (**self).read_file(path, cap)
    }

    fn bundle_candidates(&mut self, word: &str) -> Vec<String> {
        (**self).bundle_candidates(word)
    }
}

/// One scope of the calling application's own store: the layers it reads
/// through, and the edits it has not published yet.
pub struct Settings<'h> {
    host: &'h mut dyn AppDataHost,
    /// Which of the application's own documents this handle acts on. Every
    /// request it sends carries it, so a handle on one scope can neither read
    /// nor write the other.
    scope: ConfigScope,
    /// Layer 1 — the defaults the bundle ships. Read once and never written,
    /// and empty for the published scope, which has no layer beneath it.
    defaults: Document,
    /// Layers 2 and 3 as the service last served them, with this handle's own
    /// unpublished edits applied over them.
    store: Document,
    /// The keys this handle has edited since its last commit, in the order it
    /// touched them. The *value* is not held here: it is read back out of
    /// [`Self::store`] at commit time, so there is exactly one place a staged
    /// value can live and no way for the two to disagree.
    dirty: Vec<String>,
    /// Why the store layers are absent, when they are.
    store_refusal: Option<Errno>,
    /// Why the bundle's shipped defaults are absent, when the bundle ships
    /// some and they could not be read.
    defaults_refusal: Option<Errno>,
}

impl<'h> Settings<'h> {
    /// Open the calling application's own settings.
    ///
    /// This never fails. A store the service cannot serve — no service yet, an
    /// unlocked volume still to come, a caller running no signed bundle —
    /// leaves the bundle's shipped defaults standing and records the reason in
    /// [`Self::store_refusal`], so an application always runs and can always
    /// say why its settings are the shipped ones.
    ///
    /// `own_word` is the calling program's own command word — the name its
    /// bundle is installed under, which the program knows about itself. It
    /// selects nothing but layer 1: the store itself is keyed on the
    /// kernel-attested app identity and names nothing the caller supplies.
    pub fn open(host: &'h mut dyn AppDataHost, own_word: &str) -> Self {
        let (defaults, defaults_refusal) = read_defaults(host, own_word);
        Self::opened(host, ConfigScope::Private, defaults, defaults_refusal)
    }

    /// Open the calling application's own **published** scope — what it says
    /// about itself for other applications to read through [`read_published`].
    ///
    /// As [`Self::open`], this never fails: a store the service cannot serve
    /// leaves the handle empty and records the reason in
    /// [`Self::store_refusal`], so an application always runs.
    ///
    /// It takes no command word, because the published scope has **no
    /// bundle-shipped layer**. That is structural rather than a simplification:
    /// the service cannot name a bundle's directory, so a shipped published
    /// document could never be read on the foreign path — and a layer that
    /// only worked for the publishing application itself would mean two
    /// applications disagreeing about what a third publishes. What an
    /// application publishes is therefore exactly what it wrote.
    pub fn open_published(host: &'h mut dyn AppDataHost) -> Self {
        Self::opened(host, ConfigScope::Public, Document::new(), None)
    }

    /// Read `scope` and build the handle over it.
    fn opened(
        host: &'h mut dyn AppDataHost,
        scope: ConfigScope,
        defaults: Document,
        defaults_refusal: Option<Errno>,
    ) -> Self {
        let (store, store_refusal) = match read_store(host, scope) {
            Ok(document) => (document, None),
            Err(err) => (Document::new(), Some(err)),
        };
        Self {
            host,
            scope,
            defaults,
            store,
            dirty: Vec::new(),
            store_refusal,
            defaults_refusal,
        }
    }

    /// Which of the application's own documents this handle acts on.
    #[must_use]
    pub const fn scope(&self) -> ConfigScope {
        self.scope
    }

    /// Why the store layers are absent, or [`None`] when the service served
    /// them.
    ///
    /// A caller reports this rather than running silently on defaults the user
    /// did not choose.
    #[must_use]
    pub const fn store_refusal(&self) -> Option<Errno> {
        self.store_refusal
    }

    /// Why the bundle's shipped defaults are absent, or [`None`] when the
    /// bundle ships none or they were read.
    ///
    /// A bundle that ships no `DefaultSettings/settings.conf` is the ordinary
    /// case and is **not** reported here; this names a defaults document that
    /// exists and could not be used, which is a packaging defect worth saying
    /// out loud.
    #[must_use]
    pub const fn defaults_refusal(&self) -> Option<Errno> {
        self.defaults_refusal
    }

    /// The value of `key` from the highest layer that sets it, or [`None`] if
    /// no layer does.
    #[must_use]
    pub fn get(&self, key: &str) -> Option<&str> {
        self.store.get(key).or_else(|| self.defaults.get(key))
    }

    /// Every setting the layers effectively carry, each key once, in the
    /// order the layer that answers it writes it.
    ///
    /// A registry with a **closed** key set never needs this: it reads the
    /// keys it knows and leaves the rest alone. One with an *open* namespace
    /// — a catalog, a recent-file list, a per-host preference set — does, and
    /// the client can answer it with no call at all, because it already holds
    /// the document and parsed it. That is the same reason the wire carries
    /// no listing operation (`plans/APPDATA.md` §3.6): a paged listing can be
    /// spliced out of two snapshots and a whole one cannot.
    ///
    /// A key the store layer sets shadows the same key in the bundle's
    /// shipped defaults, exactly as [`Self::get`] answers it, and a key the
    /// document happens to repeat appears once with the value that wins. The
    /// answer is a whole snapshot rather than an iterator borrowing the
    /// handle, so a caller can act on it — publishing an edit, say — while
    /// still holding it.
    #[must_use]
    pub fn settings(&self) -> Vec<Setting<'_>> {
        // Ordered rather than scanned: a document may carry `MAX_SETTINGS`
        // keys, and a linear membership test per key would make listing one
        // quadratic in a bound a hostile store controls.
        let mut seen: BTreeSet<&str> = BTreeSet::new();
        let mut listed: Vec<Setting<'_>> = Vec::new();
        for setting in self.store.settings().chain(self.defaults.settings()) {
            if !seen.insert(setting.key) {
                continue;
            }
            // The value comes from `get`, not from this line: `Document::get`
            // answers with the *last* setting of a repeated key, so taking the
            // first-seen line's value would answer with the wrong one.
            let Some(value) = self.get(setting.key) else {
                continue;
            };
            listed.push(Setting {
                key: setting.key,
                value,
            });
        }
        listed
    }

    /// The boolean value of `key`.
    ///
    /// # Errors
    ///
    /// [`ConfError::ValueMalformed`] when the setting is present but is not a
    /// boolean, so a caller can report a broken value instead of silently
    /// substituting a default.
    pub fn bool(&self, key: &str) -> Result<Option<bool>, ConfError> {
        self.get(key).map(as_bool).transpose()
    }

    /// The unsigned value of `key`.
    ///
    /// # Errors
    ///
    /// As [`Self::bool`].
    pub fn u32(&self, key: &str) -> Result<Option<u32>, ConfError> {
        self.get(key).map(as_u32).transpose()
    }

    /// The signed value of `key`.
    ///
    /// # Errors
    ///
    /// As [`Self::bool`].
    pub fn i64(&self, key: &str) -> Result<Option<i64>, ConfError> {
        self.get(key).map(as_i64).transpose()
    }

    /// The permille fraction `key` names.
    ///
    /// # Errors
    ///
    /// As [`Self::bool`].
    pub fn permille(&self, key: &str) -> Result<Option<u32>, ConfError> {
        self.get(key).map(as_permille).transpose()
    }

    /// Set `key` to `value`, to be published by [`Self::commit`].
    ///
    /// A value the layers already answer with is **not** staged: an
    /// application that saves a setting it did not change must not rewrite the
    /// user's document, and a value that already comes from the policy or
    /// defaults layer must not be copied up into it. That is what keeps a
    /// user's own file holding only what the user actually changed.
    ///
    /// # Errors
    ///
    /// [`ConfError::KeyInvalid`] or [`ConfError::ValueInvalid`] for a key or
    /// value outside the grammar, and [`ConfError::TooManySettings`] or
    /// [`ConfError::TooManyLines`] at the format's bounds. The refusal happens
    /// here, against the same engine the service applies, so a commit cannot
    /// fail on an edit that was accepted earlier.
    pub fn set(&mut self, key: &str, value: &str) -> Result<(), ConfError> {
        if self.get(key) == Some(value) {
            return Ok(());
        }
        self.store.set(key, value)?;
        self.mark(key);
        Ok(())
    }

    /// Set `key` to a boolean, rendered canonically.
    ///
    /// # Errors
    ///
    /// As [`Self::set`].
    pub fn set_bool(&mut self, key: &str, value: bool) -> Result<(), ConfError> {
        self.set(key, bool_text(value))
    }

    /// Set `key` to an unsigned decimal.
    ///
    /// # Errors
    ///
    /// As [`Self::set`].
    pub fn set_u32(&mut self, key: &str, value: u32) -> Result<(), ConfError> {
        self.set(key, &text_of(value))
    }

    /// Set `key` to a signed decimal.
    ///
    /// # Errors
    ///
    /// As [`Self::set`].
    pub fn set_i64(&mut self, key: &str, value: i64) -> Result<(), ConfError> {
        self.set(key, &text_of(value))
    }

    /// Set `key` to a permille fraction.
    ///
    /// # Errors
    ///
    /// [`ConfError::ValueMalformed`] for a fraction past
    /// [`PERMILLE_FULL`](tairix_appconf::PERMILLE_FULL), else as
    /// [`Self::set`].
    pub fn set_permille(&mut self, key: &str, value: u32) -> Result<(), ConfError> {
        if value > tairix_appconf::PERMILLE_FULL {
            return Err(ConfError::ValueMalformed);
        }
        self.set_u32(key, value)
    }

    /// Remove `key` from the store's layers, to be published by
    /// [`Self::commit`].
    ///
    /// For the private scope the effective value then comes from the bundle's
    /// shipped defaults — or, once the commit has re-read the store, from the
    /// machine's policy layer if that sets it, because that layer is the
    /// service's to answer for. The published scope has no layer beneath it,
    /// so an unset there simply stops publishing the key. A key no store layer
    /// carries is already absent, so removing it stages nothing.
    pub fn unset(&mut self, key: &str) {
        if self.store.get(key).is_none() {
            return;
        }
        self.store.unset(key);
        self.mark(key);
    }

    /// Whether this handle holds edits that have not been published.
    #[must_use]
    pub fn is_dirty(&self) -> bool {
        !self.dirty.is_empty()
    }

    /// Publish every edit made since the last commit, as one atomic document
    /// replacement, and re-read the store.
    ///
    /// A commit with nothing to publish does nothing at all — no call, no
    /// rewrite of the user's document, no cost to its timestamp.
    ///
    /// # Errors
    ///
    /// The service's own typed refusal, or the transport's: [`Errno::NotFound`]
    /// when nothing has bound the endpoint, [`Errno::PermissionDenied`] for a
    /// caller with no attested app identity or a store another publisher owns,
    /// [`Errno::DeviceOffline`] for a volume that cannot be reached. A failed
    /// commit leaves the edits staged, so a caller may retry.
    pub fn commit(&mut self) -> Result<(), Errno> {
        if self.dirty.is_empty() {
            return Ok(());
        }
        // Taken out so the loop can borrow the host mutably; put back on any
        // failure, because a commit that did not land must leave the edits
        // where a retry can find them.
        let dirty = core::mem::take(&mut self.dirty);
        match self.publish(&dirty) {
            Ok(()) => self.reload(),
            Err(err) => {
                self.dirty = dirty;
                Err(err)
            }
        }
    }

    /// Discard any unpublished edits and re-read the store layers.
    ///
    /// A reload is a fresh view, not a merge: edits this handle never published
    /// are dropped, which is the contract a handle that is simply never
    /// committed already has. [`Self::commit`] ends in one, so an application
    /// that saves its settings goes on reading what the service actually holds
    /// — including the lower layer an [`Self::unset`] uncovered.
    ///
    /// # Errors
    ///
    /// As [`Self::commit`]. A failed reload leaves the handle's previous view
    /// and its edits standing, and records the reason in
    /// [`Self::store_refusal`].
    pub fn reload(&mut self) -> Result<(), Errno> {
        match read_store(self.host, self.scope) {
            Ok(document) => {
                self.store = document;
                self.dirty.clear();
                self.store_refusal = None;
                Ok(())
            }
            Err(err) => {
                self.store_refusal = Some(err);
                Err(err)
            }
        }
    }

    /// Stage every dirty key with the service and publish them.
    fn publish(&mut self, dirty: &[String]) -> Result<(), Errno> {
        let mut frame = [0u8; APPDATA_MAX_REQUEST];
        let mut reply = [0u8; STATUS_REPLY_LEN];
        for key in dirty {
            // Read the staged value out before the call: the store stays the
            // one place it lives, and the borrow ends here rather than
            // spanning the request.
            let staged = self.store.get(key).map(String::from);
            let request = match &staged {
                Some(value) => AppDataRequest::ConfigSet {
                    scope: self.scope,
                    key,
                    value,
                },
                None => AppDataRequest::ConfigUnset {
                    scope: self.scope,
                    key,
                },
            };
            self.status(&request, &mut frame, &mut reply)?;
        }
        self.status(
            &AppDataRequest::ConfigCommit { scope: self.scope },
            &mut frame,
            &mut reply,
        )
    }

    /// Issue one request whose reply is the shared status frame.
    fn status(
        &mut self,
        request: &AppDataRequest<'_>,
        frame: &mut [u8],
        reply: &mut [u8],
    ) -> Result<(), Errno> {
        let len = request.encode(frame)?;
        let got = self.host.call(&frame[..len], reply)?;
        decode_status_reply(&reply[..got])
    }

    /// Record `key` as edited, once however many times it is touched.
    fn mark(&mut self, key: &str) {
        if !self.dirty.iter().any(|seen| seen == key) {
            self.dirty.push(String::from(key));
        }
    }
}

impl Lookup for Settings<'_> {
    /// The layered read, offered as the one question a closed registry asks
    /// of whatever document holds it — so an application writes its registry
    /// once and reads it from its own store or from another application's
    /// published document with the same loader.
    fn get(&self, key: &str) -> Option<&str> {
        Self::get(self, key)
    }
}

/// `value` in its decimal spelling.
fn text_of(value: impl core::fmt::Display) -> String {
    let mut text = String::new();
    let _ = core::fmt::Write::write_fmt(&mut text, format_args!("{value}"));
    text
}

/// Read the bundle's shipped defaults, and why they are absent when they are.
///
/// The candidates are walked in the one shared resolution order and the first
/// defaults document found wins. A bundle that ships none is the ordinary
/// case: the layer is simply empty and nothing is reported. A document that
/// exists but cannot be read or parsed is a packaging defect, so the layer is
/// empty *and* the reason is carried back for the application to report.
fn read_defaults(host: &mut dyn AppDataHost, own_word: &str) -> (Document, Option<Errno>) {
    for bundle in host.bundle_candidates(own_word) {
        let mut path = String::from(bundle.trim_end_matches('/'));
        path.push('/');
        path.push_str(BundleEntry::DefaultSettings.as_str());
        path.push('/');
        path.push_str(APPDATA_SETTINGS_FILE);
        let bytes = match host.read_file(&path, APPDATA_DOCUMENT_MAX) {
            Ok(bytes) => bytes,
            // Not an installed bundle of this word, or one shipping no
            // defaults: keep looking rather than deciding the layer is broken.
            Err(Errno::NotFound) => continue,
            Err(err) => return (Document::new(), Some(err)),
        };
        let Ok(text) = core::str::from_utf8(&bytes) else {
            return (Document::new(), Some(Errno::OutOfRange));
        };
        return match Document::parse(text) {
            Ok(document) => (document, None),
            Err(_) => (Document::new(), Some(Errno::OutOfRange)),
        };
    }
    (Document::new(), None)
}

/// Read the caller's own `scope`, negotiating a buffer big enough for the whole
/// document.
fn read_store(host: &mut dyn AppDataHost, scope: ConfigScope) -> Result<Document, Errno> {
    negotiate(host, |capacity| AppDataRequest::ConfigRead {
        scope,
        capacity,
    })
}

/// The calling application's own **sealed** scope: its secrets, as the app-data
/// service last sealed them.
///
/// # It is not layered, not staged, and not shared
///
/// The sealed scope has no bundle-shipped defaults and no machine-wide policy
/// layer, because a secret an application did not write is not one it may be
/// made to believe. It has no staging either: [`Self::set`] and [`Self::unset`]
/// are each applied by the service before it replies, so there is no commit and
/// nothing an application can leave unsaved. And it has no foreign counterpart
/// at all — no request shape reaches another application's secrets.
///
/// # It fails rather than reading empty
///
/// Unlike [`Settings::open`] this can fail, and deliberately so: "I could not
/// read your secrets" is not "you have none". An application that cannot open
/// its vault must say so rather than behave as though the user had never saved
/// a password.
///
/// The handle holds nothing but the opened document, and the format engine
/// wipes a document's every line when it goes out of scope — so the plaintext
/// is gone when the handle is, with no `Drop` of its own to get wrong.
pub struct Vault<'h> {
    host: &'h mut dyn AppDataHost,
    document: Document,
}

impl<'h> Vault<'h> {
    /// Read the calling application's sealed document.
    ///
    /// One call, whatever the vault holds. It takes no command word and no
    /// identifier: the service derives the store, the account, and the key from
    /// the identity the kernel attests for this task.
    ///
    /// # Errors
    ///
    /// The service's own typed refusal, or the transport's:
    /// [`Errno::NotFound`] when nothing has bound the endpoint,
    /// [`Errno::PermissionDenied`] for a caller running no signed bundle or a
    /// store another publisher owns, [`Errno::DeviceOffline`] for a volume that
    /// cannot be reached, [`Errno::SignatureInvalid`] for a sealed document
    /// that fails authentication, and [`Errno::BadMagic`] for one — or for the
    /// account's key material — that is damaged. The last two mean the secrets
    /// are unreadable, which is never reported as an empty vault.
    pub fn open(host: &'h mut dyn AppDataHost) -> Result<Self, Errno> {
        let document = negotiate(host, |capacity| AppDataRequest::VaultRead { capacity })?;
        Ok(Self { host, document })
    }

    /// The secret `key` names, or [`None`] if the vault does not hold one.
    ///
    /// A local lookup: opening did the one round trip, so an application that
    /// reads three secrets issues no further calls.
    #[must_use]
    pub fn get(&self, key: &str) -> Option<&str> {
        self.document.get(key)
    }

    /// Whether the vault holds a secret for `key`.
    #[must_use]
    pub fn has(&self, key: &str) -> bool {
        self.document.get(key).is_some()
    }

    /// Seal `key = value`, immediately.
    ///
    /// This is one call the service applies before it replies — there is no
    /// commit, and nothing is left unsaved. A value the vault already holds
    /// costs no call at all.
    ///
    /// # Errors
    ///
    /// As [`Self::open`], plus [`Errno::OutOfRange`] for a key or value outside
    /// the format's grammar. A refused write leaves the vault — and this
    /// handle — as they were.
    pub fn set(&mut self, key: &str, value: &str) -> Result<(), Errno> {
        if self.document.get(key) == Some(value) {
            return Ok(());
        }
        self.write(&AppDataRequest::VaultSet { key, value })
    }

    /// Remove the secret `key` names, immediately.
    ///
    /// The sealed scope has no layer beneath it, so a removal leaves the key
    /// absent rather than uncovering something else. Removing a key the vault
    /// does not hold costs no call.
    ///
    /// # Errors
    ///
    /// As [`Self::set`].
    pub fn unset(&mut self, key: &str) -> Result<(), Errno> {
        if self.document.get(key).is_none() {
            return Ok(());
        }
        self.write(&AppDataRequest::VaultUnset { key })
    }

    /// Apply one sealed write and re-read what the service then holds.
    ///
    /// The re-read is not a courtesy: the service is the only thing that knows
    /// what the sealed document says once the write has landed, and applying
    /// the change to this handle's own copy instead would be this library
    /// *guessing* — a guess that is wrong the moment another instance of the
    /// application has sealed something of its own. The same rule
    /// [`Settings::commit`] follows, for the same reason.
    ///
    /// # Errors
    ///
    /// As [`Self::set`]. A write that is refused leaves the handle untouched; a
    /// write that lands and is then followed by a failed re-read reports the
    /// re-read's error, and the handle keeps its previous view — a retry of the
    /// same write is harmless, because sealing a value the vault already holds
    /// costs nothing.
    fn write(&mut self, request: &AppDataRequest<'_>) -> Result<(), Errno> {
        let mut frame = [0u8; APPDATA_MAX_REQUEST];
        let mut reply = [0u8; STATUS_REPLY_LEN];
        let len = request.encode(&mut frame)?;
        let got = self.host.call(&frame[..len], &mut reply);
        // The request frame carried the secret; it is this library's buffer and
        // it goes no further.
        frame[..len].zeroize();
        decode_status_reply(&reply[..got?])?;
        self.reload()
    }

    /// Re-read the sealed document.
    ///
    /// # Errors
    ///
    /// As [`Self::open`]. A failed reload leaves this handle's previous view
    /// standing.
    pub fn reload(&mut self) -> Result<(), Errno> {
        self.document = negotiate(self.host, |capacity| AppDataRequest::VaultRead { capacity })?;
        Ok(())
    }
}

/// Read the document the application `bundle_id` names **publishes**.
///
/// This is the one call in the library that names another application, and it
/// can reach nothing but that application's published document: the request
/// carries no scope, so there is no shape by which a caller could ask for
/// another application's private settings.
///
/// An application that publishes nothing, has never run for this account, or
/// whose store the service cannot attest all answer the **empty document** —
/// deliberately indistinguishable, so a caller learns nothing but what an
/// application chose to publish. The answer is the publisher's *committed*
/// document, never its unsaved edits.
///
/// # Errors
///
/// The service's own typed refusal, or the transport's: [`Errno::NotFound`]
/// when nothing has bound the endpoint, [`Errno::PermissionDenied`] for a
/// caller with no attested app identity, [`Errno::DeviceOffline`] for a volume
/// that cannot be reached, [`Errno::OutOfRange`] for an identifier outside the
/// bundle-identifier grammar.
pub fn read_published(host: &mut dyn AppDataHost, bundle_id: &str) -> Result<Document, Errno> {
    negotiate(host, |capacity| AppDataRequest::PublicRead {
        bundle_id,
        capacity,
    })
}

/// Issue the read `request` builds, negotiating a buffer big enough for the
/// whole document.
///
/// The first call asks for [`READ_PROBE`] bytes, which covers a realistic
/// store; a larger one comes back as the length it needs and is asked for
/// again at exactly that size. Every answer is a whole document, so no
/// document is ever assembled out of two snapshots.
fn negotiate<'a>(
    host: &mut dyn AppDataHost,
    request: impl Fn(u32) -> AppDataRequest<'a>,
) -> Result<Document, Errno> {
    let mut capacity = READ_PROBE;
    for _ in 0..READ_ATTEMPTS {
        // A read carries a scope or an application identifier, never a
        // setting, so the frame is bounded by the widest of those rather than
        // by the widest request in the protocol — a `ConfigSet`'s kilobyte of
        // value has no business on a start-up read's stack.
        let mut frame = [0u8; APPDATA_HEADER_LEN + BUNDLE_ID_MAX];
        let asked = request(u32::try_from(capacity).map_err(|_| Errno::LengthOutOfRange)?);
        let len = asked.encode(&mut frame)?;
        let frame_len = STATUS_REPLY_LEN + APPDATA_DOCUMENT_HEADER_LEN + capacity;
        let mut reply = Vec::new();
        reply
            .try_reserve_exact(frame_len)
            .map_err(|_| Errno::OutOfMemory)?;
        reply.resize(frame_len, 0);
        match attempt(host, &frame[..len], &mut reply)? {
            Answer::Whole(document) => return Ok(document),
            Answer::NeedsCapacity(needed) => capacity = needed,
        }
    }
    // A writer that grew the document under every attempt: say so rather than
    // chase it, and never answer with a document that is not whole.
    Err(Errno::Busy)
}

/// What one read attempt answered.
///
/// The whole document is carried **owned** rather than borrowed out of `reply`,
/// which is what lets [`attempt`] wipe the buffer before it returns.
enum Answer {
    /// The document the service served, parsed.
    Whole(Document),
    /// The document did not fit; this is the length to ask again with.
    NeedsCapacity(usize),
}

/// Issue one read of `request` into `reply` and answer what came back, wiping
/// `reply` before returning.
///
/// The sealed scope's document is an application's secrets in the clear, and
/// the userland heap does not re-zero freed bytes — so a transport buffer left
/// holding one would hand it to whatever allocates that memory next. Wiping
/// here, on the success and the refusal path alike, is what keeps the plaintext
/// to the parsed document the caller can drop, and what makes the wipe cover a
/// path added later.
///
/// The whole of `reply` is wiped rather than the length the host reported: the
/// host was handed all of it, so its report bounds what may be *read* and says
/// nothing about where it wrote.
fn attempt(host: &mut dyn AppDataHost, request: &[u8], reply: &mut [u8]) -> Result<Answer, Errno> {
    let read = host.call(request, reply);
    let answered = read.and_then(|got| {
        let seen = reply.get(..got).ok_or(Errno::OutOfRange)?;
        match decode_document_reply(seen)? {
            ConfigDocument::Whole(text) => Document::parse(text)
                .map(Answer::Whole)
                .map_err(|_| Errno::OutOfRange),
            ConfigDocument::NeedsCapacity(needed) => Ok(Answer::NeedsCapacity(needed)),
        }
    });
    reply.zeroize();
    answered
}

/// The **bulk** scope: an application's blobs, reached as descriptors rather
/// than as bytes on the app-data channel.
///
/// # Why a descriptor
///
/// A mail index, a search index, or a thumbnail cache is the wrong shape for a
/// message, and the IPC payload ceiling is far below what one holds — so the
/// service makes the policy decision once at open and hands back a one-shot
/// `fd_grant` handle. Redeeming it (`File::from_delegation`) installs a real
/// descriptor whose reads, writes, truncations, and mappings go straight to
/// the kernel VFS under the service's captured authority, so the application
/// needs no filesystem capability of its own and the service never touches a
/// byte of payload.
///
/// # What bounds it
///
/// The delegation is the bound: it conveys only the access the mode asked for,
/// and a writable one carries an extent ceiling the kernel enforces on every
/// write and truncate through the descriptor. So an application cannot grow a
/// blob past
/// [`APPDATA_BULK_FILE_MAX_BYTES`](tairix_abi::appdata_ipc::APPDATA_BULK_FILE_MAX_BYTES)
/// however it uses what it was given, and
/// [`APPDATA_BLOB_MAX_COUNT`](tairix_abi::appdata_ipc::APPDATA_BLOB_MAX_COUNT)
/// bounds how many blobs it may hold at all. [`bulk_quota`] reports usage
/// against both, so an application that reaches one says which rather than
/// surfacing an errno.
///
/// # No application names another
///
/// As with every other scope, nothing here takes a bundle identifier: the
/// service derives whose blobs these are from the identity the kernel attests
/// for the calling task. There is no foreign counterpart at all — no request
/// shape reaches another application's blobs.
pub mod blobs {
    use super::{AppDataHost, APPDATA_MAX_REQUEST};
    use alloc::string::String;
    use alloc::vec::Vec;
    use tairix_abi::appdata_ipc::{
        decode_blob_list_reply, decode_grant_reply, AppDataRequest, BlobListing, BlobMode,
        APPDATA_BLOB_LIST_HEADER_LEN, APPDATA_BLOB_LIST_MAX, APPDATA_GRANT_REPLY_LEN,
        APPDATA_HEADER_LEN, APPDATA_NAME_MAX,
    };
    use tairix_abi::reply::STATUS_REPLY_LEN;
    use tairix_abi::Errno;

    /// Open the calling application's blob `name`, returning the one-shot
    /// grant handle to redeem it with.
    ///
    /// A handle rather than an owned descriptor, because installing one is a
    /// syscall and this crate is I/O-free by design so the whole client stays
    /// host-testable. `tairix_rt::File::from_delegation` is the owned
    /// redemption, and it closes the descriptor on every path out.
    ///
    /// [`BlobMode::Read`] refuses a blob the application does not hold;
    /// [`BlobMode::ReadWrite`] creates it. Creation is the mode's business
    /// rather than a separate flag, so "create but do not write" is not a
    /// request that exists.
    ///
    /// # Errors
    ///
    /// The service's own typed refusal, or the transport's:
    /// [`Errno::NotFound`] when nothing has bound the endpoint or the blob
    /// does not exist, [`Errno::PermissionDenied`] for a caller running no
    /// signed bundle or a store another publisher owns,
    /// [`Errno::LimitExceeded`] when the application already holds as many
    /// blobs as it may, [`Errno::OutOfRange`] for a name outside the
    /// store-name grammar, [`Errno::DeviceOffline`] for a volume that cannot
    /// be reached.
    pub fn open(host: &mut dyn AppDataHost, name: &str, mode: BlobMode) -> Result<u64, Errno> {
        let mut frame = [0u8; APPDATA_HEADER_LEN + APPDATA_NAME_MAX];
        let len = AppDataRequest::BlobOpen { name, mode }.encode(&mut frame)?;
        let mut reply = [0u8; APPDATA_GRANT_REPLY_LEN];
        let got = host.call(&frame[..len], &mut reply)?;
        decode_grant_reply(&reply[..got])
    }

    /// Delete the calling application's blob `name`.
    ///
    /// Deleting one it does not hold changes nothing and is not an error, so
    /// this is never an oracle for what the store holds.
    ///
    /// # Errors
    ///
    /// As [`open`], without [`Errno::NotFound`] for an absent blob.
    pub fn remove(host: &mut dyn AppDataHost, name: &str) -> Result<(), Errno> {
        let mut frame = [0u8; APPDATA_HEADER_LEN + APPDATA_NAME_MAX];
        let len = AppDataRequest::BlobDelete { name }.encode(&mut frame)?;
        let mut reply = [0u8; STATUS_REPLY_LEN];
        let got = host.call(&frame[..len], &mut reply)?;
        tairix_abi::reply::decode_status_reply(&reply[..got])
    }

    /// Every blob the calling application holds, with its length in bytes.
    ///
    /// **One call, always.** The widest listing that can exist is bounded by
    /// the blob count and the fixed entry width, and that bound is a few
    /// kilobytes — so this asks for it outright rather than negotiating a
    /// capacity. A document read has to negotiate because a document's ceiling
    /// is 64 KiB and sizing every start-up buffer to it would cost every
    /// application the allocation; a listing has no such spread. The wire
    /// still answers a capacity refusal honestly, for a caller with a tighter
    /// buffer than this one.
    ///
    /// The answer is whole or nothing, so no caller acts on a listing spliced
    /// out of two snapshots of a changing store.
    ///
    /// # Errors
    ///
    /// As [`open`], without [`Errno::NotFound`] for an absent blob.
    pub fn list(host: &mut dyn AppDataHost) -> Result<Vec<(String, u64)>, Errno> {
        let mut frame = [0u8; APPDATA_HEADER_LEN];
        let len = AppDataRequest::BlobList {
            capacity: u32::try_from(APPDATA_BLOB_LIST_MAX).map_err(|_| Errno::LengthOutOfRange)?,
        }
        .encode(&mut frame)?;
        let mut reply = Vec::new();
        let frame_len = STATUS_REPLY_LEN + APPDATA_BLOB_LIST_HEADER_LEN + APPDATA_BLOB_LIST_MAX;
        reply
            .try_reserve_exact(frame_len)
            .map_err(|_| Errno::OutOfMemory)?;
        reply.resize(frame_len, 0);
        let got = host.call(&frame[..len], &mut reply)?;
        match decode_blob_list_reply(&reply[..got])? {
            BlobListing::Whole(listing) => Ok(BlobListing::Whole(listing)
                .entries()
                .map(|entry| (String::from(entry.name), entry.len))
                .collect()),
            // Unreachable: the capacity asked for is the widest listing the
            // contract allows. Fail closed rather than trust the arithmetic.
            BlobListing::NeedsCapacity(_) => Err(Errno::OutOfRange),
        }
    }

    // The request buffers above are sized to the widest frame each operation
    // can produce, not to the widest in the protocol: a blob call carries a
    // name at most, so a `ConfigSet`'s kilobyte of value has no business on
    // its stack.
    const _: () = assert!(APPDATA_HEADER_LEN + APPDATA_NAME_MAX <= APPDATA_MAX_REQUEST);
}

/// The **scratch** scope: an application's temporary files, reached as
/// descriptors exactly as [`blobs`] are.
///
/// # The service names the file, and nothing opens one
///
/// [`temp::create`] answers a fresh file every time, with a name the *service*
/// chose. That is the whole point of the scope: freshness without
/// coordination, so two instances of one application cannot land on each
/// other's scratch the way two that both picked `"spill"` would. There is no
/// operation that opens a temporary file by name, so an application can never
/// read scratch it did not write in this process — not even its own from an
/// earlier run.
///
/// The name it is handed back is good for exactly one thing, [`temp::release`].
///
/// # Their lifetime is the boot
///
/// A file left behind by an earlier boot is reclaimed before the next is
/// created, and is reachable by nothing in the meantime. An application that
/// releases what it finishes with therefore holds one slot at a time; one that
/// never releases holds them until the next boot, which is what
/// [`APPDATA_TEMP_MAX_COUNT`](tairix_abi::appdata_ipc::APPDATA_TEMP_MAX_COUNT)
/// bounds — and it bounds nothing but that application.
pub mod temp {
    use super::AppDataHost;
    use alloc::string::String;
    use tairix_abi::appdata_ipc::{
        decode_temp_reply, AppDataRequest, APPDATA_HEADER_LEN, APPDATA_NAME_MAX,
        APPDATA_TEMP_REPLY_LEN,
    };
    use tairix_abi::reply::{decode_status_reply, STATUS_REPLY_LEN};
    use tairix_abi::Errno;

    /// A temporary file the service has just created.
    #[derive(Clone, Debug, Eq, PartialEq)]
    pub struct TempFile {
        /// The one-shot delegation handle, redeemed with
        /// `tairix_rt::File::from_delegation`. It conveys read and write
        /// access bounded by an extent ceiling the kernel enforces.
        pub grant: u64,
        /// The name the service gave the file, to [`release`] it by.
        pub name: String,
    }

    /// Create a fresh temporary file for the calling application.
    ///
    /// # Errors
    ///
    /// The service's own typed refusal, or the transport's:
    /// [`Errno::NotFound`] when nothing has bound the endpoint,
    /// [`Errno::PermissionDenied`] for a caller running no signed bundle or a
    /// store another publisher owns, [`Errno::LimitExceeded`] when the
    /// application already holds as many temporary files as it may,
    /// [`Errno::EntropyNotReady`] on a machine whose boot carries no identity —
    /// which stays true for that whole boot — and [`Errno::DeviceOffline`] for
    /// a volume that cannot be reached.
    pub fn create(host: &mut dyn AppDataHost) -> Result<TempFile, Errno> {
        let mut frame = [0u8; APPDATA_HEADER_LEN];
        let len = AppDataRequest::TempCreate {}.encode(&mut frame)?;
        let mut reply = [0u8; APPDATA_TEMP_REPLY_LEN];
        let got = host.call(&frame[..len], &mut reply)?;
        let (grant, name) = decode_temp_reply(&reply[..got])?;
        Ok(TempFile {
            grant,
            name: String::from(name),
        })
    }

    /// Delete the temporary file `name`.
    ///
    /// Releasing one the application does not hold changes nothing and is not
    /// an error, so this is never an oracle for what the store holds. An
    /// application that never releases leaves its scratch until the next boot.
    ///
    /// # Errors
    ///
    /// As [`create`], without [`Errno::LimitExceeded`].
    pub fn release(host: &mut dyn AppDataHost, name: &str) -> Result<(), Errno> {
        let mut frame = [0u8; APPDATA_HEADER_LEN + APPDATA_NAME_MAX];
        let len = AppDataRequest::TempRelease { name }.encode(&mut frame)?;
        let mut reply = [0u8; STATUS_REPLY_LEN];
        let got = host.call(&frame[..len], &mut reply)?;
        decode_status_reply(&reply[..got])
    }
}

/// The calling application's bulk-store usage — blobs and temporary files
/// both — and the ceilings it is bounded by.
///
/// One answer for both scopes, because they are one store: an application
/// deciding whether to spill to scratch or evict a cached index reads one
/// moment rather than two that could disagree.
///
/// # Errors
///
/// The service's own typed refusal, or the transport's: [`Errno::NotFound`]
/// when nothing has bound the endpoint, [`Errno::PermissionDenied`] for a
/// caller running no signed bundle or a store another publisher owns,
/// [`Errno::DeviceOffline`] for a volume that cannot be reached.
pub fn bulk_quota(host: &mut dyn AppDataHost) -> Result<BulkQuota, Errno> {
    let mut frame = [0u8; APPDATA_HEADER_LEN];
    let len = AppDataRequest::QuotaGet {}.encode(&mut frame)?;
    let mut reply = [0u8; APPDATA_QUOTA_REPLY_LEN];
    let got = host.call(&frame[..len], &mut reply)?;
    decode_quota_reply(&reply[..got])
}

#[cfg(test)]
#[path = "tests.rs"]
mod tests;
