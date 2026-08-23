//! The app-data client (`tairix-appdata`): how an application reaches its own
//! settings, and the only way it can.
//!
//! ```ignore
//! let mut host = RtHost;
//! let mut settings = Settings::open(&mut host, OWN_WORD);
//! let size = settings.u32("font.size")?.unwrap_or(14);
//! settings.set_u32("font.size", 16)?;
//! settings.commit()?;                       // one atomic publish
//!
//! let mut mine = Settings::open_published(&mut host);   // what others may read
//! mine.set("font.family", "berkeley")?;
//! mine.commit()?;
//!
//! let theirs = read_published(&mut host, "os.tairix.terminal")?;
//! let family = theirs.get("font.family");
//!
//! let mut vault = Vault::open(&mut host)?;      // the sealed scope
//! vault.set("imap.password", secret)?;          // sealed before it returns
//! let saved = vault.get("imap.password");
//! ```
//!
//! # No app spells a path, and none names itself
//!
//! Nothing here takes a store path or a user, and nothing but
//! [`read_published`] takes a bundle identifier: the app-data service derives
//! every one of those from the identity the kernel attests for the calling
//! task. So an application cannot reach outside its own scope by construction
//! rather than by a check some caller might forget, and this library has no
//! privileged surface to misuse. The one identifier a caller does name selects
//! a *published* document and nothing else — there is no request shape that
//! reaches another application's private settings.
//!
//! # Three scopes
//!
//! [`Settings::open`] is the application's **private** scope: the user's
//! settings for it, which nothing else can read. [`Settings::open_published`]
//! is its **published** scope: what the application says about itself for
//! other applications to read, through [`read_published`]. The two are
//! separate documents with separate commits, because one atomic publish
//! replaces one document.
//!
//! [`Vault`] is the **sealed** scope: the application's secrets, encrypted at
//! rest under a key the service derives per (account, application). It differs
//! from the other two in three ways, each of them the sealed scope's own: it has
//! no layer beneath it, because a secret an application did not write is not one
//! it may be made to believe; it has no staging and no commit, because the
//! service seals each write before it replies; and opening it can *fail*,
//! because "I could not read your secrets" is not "you have none".
//!
//! # Three layers, and which of them this library owns
//!
//! Layering is the **private** scope's; the published scope is one document
//! (see [`Settings::open_published`]). A private read answers from the highest
//! layer that sets the key:
//!
//! 1. `<Bundle>.app/DefaultSettings/settings.conf` — the defaults the bundle
//!    ships. **This library's layer**: it needs the *bundle's* path, and
//!    nothing attested gives the service one, while a program can name its own
//!    bundle through the one shared command-word resolution order. A wrong pick
//!    there could only hand an application another build of *itself*'s
//!    defaults, so no boundary is crossed.
//! 2. `/System/Settings/<bundle-id>/settings.conf` — optional machine-wide
//!    administrator policy. The service's layer.
//! 3. The user's own document — overrides only. The service's layer.
//!
//! Layers 2 and 3 arrive already merged, as one document, in one call. That is
//! also what makes the "no service, degrade to the shipped defaults" path one
//! code path instead of two: an unreachable store simply leaves layer 1
//! standing, [`Settings::store_refusal`] says why, and a write fails with that
//! same typed error rather than silently going nowhere.
//!
//! # Reads are local; writes are staged and published once
//!
//! Opening does the one round trip. Every read after it is a lookup
//! in memory, so an application that consults forty settings issues no further
//! calls — and every [`Settings::set`] is memory too, until
//! [`Settings::commit`] stages the keys that changed and publishes them as one
//! atomic document replacement. A handle that is never committed changes
//! nothing on the volume.
//!
//! A commit ends by re-reading the store, so the handle always reflects what
//! the service actually holds — which matters after a [`Settings::unset`],
//! where the effective value comes back from a layer below rather than from
//! the value that was removed.
//!
//! # Layering
//!
//! The crate is `no_std` (with `alloc`) and performs no I/O of its own: every
//! syscall it needs sits behind the [`AppDataHost`] seam, so the whole client
//! — the layered read, the capacity negotiation, staging, the commit, and the
//! sealed scope — is exercised on the host. The `rt` feature supplies the real
//! seam over `ipc_call` and `fs_*`.

#![no_std]
#![forbid(unsafe_code)]
#![deny(missing_docs)]

extern crate alloc;

use alloc::string::String;
use alloc::vec::Vec;

use tairix_abi::appdata_ipc::{
    decode_document_reply, AppDataRequest, ConfigDocument, ConfigScope,
    APPDATA_DOCUMENT_HEADER_LEN, APPDATA_DOCUMENT_MAX, APPDATA_HEADER_LEN, APPDATA_MAX_REQUEST,
    APPDATA_SETTINGS_FILE,
};
use tairix_abi::appinfo::{BundleEntry, BUNDLE_ID_MAX};
use tairix_abi::reply::{decode_status_reply, STATUS_REPLY_LEN};
use tairix_abi::Errno;
use tairix_appconf::{as_bool, as_i64, as_permille, as_u32, bool_text, ConfError, Document};
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
        let got = host.call(&frame[..len], &mut reply)?;
        match decode_document_reply(&reply[..got])? {
            ConfigDocument::Whole(text) => {
                return Document::parse(text).map_err(|_| Errno::OutOfRange)
            }
            ConfigDocument::NeedsCapacity(needed) => capacity = needed,
        }
    }
    // A writer that grew the document under every attempt: say so rather than
    // chase it, and never answer with a document that is not whole.
    Err(Errno::Busy)
}

#[cfg(test)]
#[path = "tests.rs"]
mod tests;
