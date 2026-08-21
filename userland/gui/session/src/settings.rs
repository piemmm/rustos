//! The session's ownership of the user's pinboard settings: the per-user
//! store, and the one policy that decides whether a foreign caller's
//! *apply* request may be honoured (`plans/PINBOARD.md` §2, §6).
//!
//! Settings are per-user configuration (`lib/wallpaper`), stored at
//! `~/Settings/Pinboard/pinboard.conf` under the user's own identity — no
//! new capability, exactly like the program-library overlay. The session is
//! the
//! store's only writer: every change, whether the backdrop menu asked for
//! it or the wallpaper chooser did, is rendered through the one
//! `lib/wallpaper` engine and written through the one
//! [`SessionFileWriter`] seam, and the desktop adopts it **only after the
//! write succeeded**, so memory and disk can never diverge.
//!
//! The apply channel's decision is here, not beside the serve loop, so it
//! is host-testable without a running kernel: the caller's uid is the
//! kernel-attested one the embedder reads off the endpoint, and this module
//! compares it against the session's own and parses the carried document
//! with the same engine the on-disk store is parsed with. Nothing here
//! performs I/O, so the path the document names is read later, by the
//! session, under the session's own identity — a caller can never use this
//! channel to reach a file it could not read itself.

use alloc::format;
use alloc::string::{String, ToString};

use tairix_abi::pinboard_ipc::PinboardRequest;
use tairix_abi::Errno;
use tairix_wallpaper::{parse, render, user_settings_path, PinboardSettings};

use crate::assets::{SessionFileReader, SessionFileWriter};

/// The user's pinboard settings store: the path changes are persisted to.
///
/// The settings themselves live in the desktop model, which is what applies
/// them; holding a second copy here would let the two drift.
#[derive(Clone, Debug, Default)]
pub struct PinboardStore {
    path: Option<String>,
}

/// What loading the user's pinboard settings produced: the store later
/// changes persist through, the settings the desktop starts on, and the
/// ready-to-print warning line for a document that could not be used.
#[derive(Clone, Debug)]
pub struct LoadedPinboard {
    /// The store the session persists later changes through.
    pub store: PinboardStore,
    /// The settings to apply before the desktop's first listing.
    pub settings: PinboardSettings,
    /// The `stderr` line for a store that could not be used, already
    /// newline-terminated. `None` when the store loaded, and when it was
    /// simply absent.
    pub warning: Option<String>,
}

/// Why persisting the pinboard settings changed nothing. Every variant is a
/// refusal the embedder reports; neither leaves memory and disk diverged.
#[derive(Copy, Clone, Debug, Eq, PartialEq)]
pub enum PinboardStoreError {
    /// The session has no home directory, so there is no per-user store to
    /// persist to (pinboard settings are per-user state only).
    NoHome,
    /// The store write was refused; nothing was adopted.
    Write(Errno),
}

impl PinboardStoreError {
    /// The wire [`Errno`] a caller that asked for this change receives.
    #[must_use]
    pub const fn errno(self) -> Errno {
        match self {
            Self::NoHome => Errno::NotFound,
            Self::Write(err) => err,
        }
    }
}

impl core::fmt::Display for PinboardStoreError {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        match self {
            Self::NoHome => f.write_str("no home directory; desktop settings cannot be saved"),
            Self::Write(err) => {
                write!(f, "the desktop settings could not be written ({err:?})")
            }
        }
    }
}

/// Why an *apply* request arriving on the pinboard channel was refused.
///
/// Carries both the wire [`Errno`] the caller receives and the `stderr`
/// line the session states in its own house style, so the one refusal
/// decision names both without this pure policy performing any I/O.
#[derive(Copy, Clone, Debug, Eq, PartialEq)]
pub enum PinboardApplyRefusal {
    /// The caller's kernel-attested origin uid is not the uid this session
    /// runs as, so it is asking to rewrite somebody else's desktop.
    Unattested,
    /// The request frame itself failed to decode.
    Malformed(Errno),
    /// The carried document is not a settings document this engine accepts.
    Undecodable,
}

impl PinboardApplyRefusal {
    /// The wire [`Errno`] the caller receives.
    #[must_use]
    pub const fn errno(self) -> Errno {
        match self {
            Self::Unattested => Errno::PermissionDenied,
            Self::Malformed(err) => err,
            Self::Undecodable => Errno::OutOfRange,
        }
    }

    /// The `stderr` diagnosis line, without the leading `desktop: ` prefix
    /// or trailing newline the caller adds in its own house style.
    #[must_use]
    pub const fn reason(self) -> &'static str {
        match self {
            Self::Unattested => "pinboard apply from another user refused",
            Self::Malformed(_) => "malformed pinboard apply refused",
            Self::Undecodable => "pinboard apply carries an unusable settings document",
        }
    }
}

impl PinboardStore {
    /// Load the user's pinboard settings.
    ///
    /// Mirrors the program library's posture exactly: an absent document is
    /// the
    /// ordinary fresh-account state and silently yields the defaults; an
    /// unreadable, over-long, or malformed one also yields the defaults
    /// plus a ready-to-print warning line — the desktop always comes up on
    /// a fully-specified backdrop rather than a guessed one. Without a home
    /// there is no store at all and every later change refuses with
    /// [`PinboardStoreError::NoHome`].
    pub fn load<R>(reader: &mut R, home: Option<&str>) -> LoadedPinboard
    where
        R: SessionFileReader + ?Sized,
    {
        let Some(path) = home.and_then(user_settings_path) else {
            return LoadedPinboard {
                store: Self::default(),
                settings: PinboardSettings::default(),
                warning: None,
            };
        };
        let (settings, warning) = match reader.read(&path) {
            Err(Errno::NotFound) => (PinboardSettings::default(), None),
            Err(err) => (
                PinboardSettings::default(),
                Some(warning(&path, &format!("read failed ({err:?})"))),
            ),
            Ok(bytes) if bytes.len() > tairix_wallpaper::MAX_SETTINGS_LEN => (
                PinboardSettings::default(),
                Some(warning(&path, "longer than any valid settings document")),
            ),
            Ok(bytes) => match core::str::from_utf8(&bytes) {
                Err(_) => (
                    PinboardSettings::default(),
                    Some(warning(&path, "not valid UTF-8")),
                ),
                Ok(text) => match parse(text) {
                    Ok(settings) => (settings, None),
                    Err(err) => (
                        PinboardSettings::default(),
                        Some(warning(&path, &err.to_string())),
                    ),
                },
            },
        };
        LoadedPinboard {
            store: Self { path: Some(path) },
            settings,
            warning,
        }
    }

    /// Render `settings` through the one settings engine and replace the
    /// user's own document with it.
    ///
    /// The caller adopts the settings in the desktop model only once this
    /// has succeeded, so a refused write leaves the desktop exactly as it
    /// was rather than showing a backdrop the next login would not restore.
    ///
    /// # Errors
    ///
    /// [`PinboardStoreError`] when there is no home to store settings in,
    /// or the write itself was refused.
    pub fn persist<W>(
        &self,
        writer: &mut W,
        settings: &PinboardSettings,
    ) -> Result<(), PinboardStoreError>
    where
        W: SessionFileWriter + ?Sized,
    {
        let Some(path) = self.path.as_deref() else {
            return Err(PinboardStoreError::NoHome);
        };
        let rendered = render(settings);
        writer
            .write(path, rendered.as_bytes())
            .map_err(PinboardStoreError::Write)
    }

    /// The document path changes are persisted to, or `None` without a
    /// home.
    #[must_use]
    pub fn path(&self) -> Option<&str> {
        self.path.as_deref()
    }
}

/// Attest an *apply* request against the session's own identity and decode
/// the settings it carries.
///
/// `caller_uid` is the **kernel-attested** origin uid of the calling task,
/// read off the endpoint by the embedder — never anything the caller said
/// about itself. Only a caller running as this session's own user may
/// rewrite this session's desktop: a request from any other uid is
/// [`PinboardApplyRefusal::Unattested`] and decodes nothing. A frame that
/// will not decode, or a document the one settings engine refuses, is
/// likewise a typed refusal.
///
/// The returned settings are only *what was asked for*: the session still
/// persists and applies them through its own path, and still reads the
/// wallpaper the document names under its own identity, so this channel
/// grants a caller no reach it did not already have.
///
/// # Errors
///
/// The [`PinboardApplyRefusal`] naming why the request was refused.
pub fn serve_pinboard_apply(
    session_uid: u32,
    caller_uid: u32,
    request: &[u8],
) -> Result<PinboardSettings, PinboardApplyRefusal> {
    if caller_uid != session_uid {
        return Err(PinboardApplyRefusal::Unattested);
    }
    let request = PinboardRequest::from_bytes(request).map_err(PinboardApplyRefusal::Malformed)?;
    let PinboardRequest::Apply { document } = request;
    parse(document.as_str()).map_err(|_| PinboardApplyRefusal::Undecodable)
}

/// The warning line for a settings document the desktop could not use,
/// ready for the embedder to print.
fn warning(path: &str, detail: &str) -> String {
    format!("desktop: pinboard settings {path}: {detail}; using the defaults\n")
}

#[cfg(test)]
mod tests {
    use alloc::string::{String, ToString};
    use alloc::vec::Vec;

    use tairix_abi::pinboard_ipc::{PinboardDocument, PinboardRequest};
    use tairix_abi::Errno;
    use tairix_wallpaper::{
        render, IconFlow, IconSort, PinboardSettings, WallpaperChoice, WallpaperFit,
    };

    use super::{serve_pinboard_apply, PinboardApplyRefusal, PinboardStore, PinboardStoreError};
    use crate::assets::{SessionFileReader, SessionFileWriter};

    /// The uid the session under test runs as.
    const SESSION_UID: u32 = 1000;

    /// A reader answering one canned outcome for any path.
    struct Canned(Result<Vec<u8>, Errno>);

    impl SessionFileReader for Canned {
        fn read(&mut self, _path: &str) -> Result<Vec<u8>, Errno> {
            self.0.clone()
        }
    }

    /// A writer recording what it was handed, or refusing every write.
    #[derive(Default)]
    struct Recorder {
        written: Option<(String, String)>,
        refuse: Option<Errno>,
    }

    impl SessionFileWriter for Recorder {
        fn write(&mut self, path: &str, bytes: &[u8]) -> Result<(), Errno> {
            if let Some(err) = self.refuse {
                return Err(err);
            }
            self.written = Some((
                path.to_string(),
                String::from_utf8_lossy(bytes).into_owned(),
            ));
            Ok(())
        }
    }

    /// Settings distinguishable from the defaults in every field the
    /// document carries.
    fn edited() -> PinboardSettings {
        PinboardSettings {
            wallpaper: WallpaperChoice::None,
            fit: WallpaperFit::Centre,
            icons: IconFlow::Trailing,
            sort: IconSort::Size,
            ..PinboardSettings::default()
        }
    }

    /// An `Apply` frame carrying `settings` as its rendered document.
    fn apply_frame(settings: &PinboardSettings) -> Vec<u8> {
        let document = PinboardDocument::new(&render(settings)).expect("renders a valid document");
        PinboardRequest::Apply { document }.to_le_bytes().to_vec()
    }

    #[test]
    fn no_home_loads_the_defaults_and_refuses_every_write() {
        let loaded = PinboardStore::load(&mut Canned(Err(Errno::NotFound)), None);
        assert_eq!(loaded.settings, PinboardSettings::default());
        assert!(loaded.warning.is_none());
        assert!(loaded.store.path().is_none());
        assert_eq!(
            loaded
                .store
                .persist(&mut Recorder::default(), &PinboardSettings::default()),
            Err(PinboardStoreError::NoHome)
        );
    }

    #[test]
    fn an_absent_document_is_the_silent_fresh_account_state() {
        let loaded = PinboardStore::load(&mut Canned(Err(Errno::NotFound)), Some("/Users/ada"));
        assert_eq!(loaded.settings, PinboardSettings::default());
        assert!(loaded.warning.is_none());
        assert_eq!(
            loaded.store.path(),
            Some("/Users/ada/Settings/Pinboard/pinboard.conf")
        );
    }

    #[test]
    fn a_stored_document_is_adopted_verbatim() {
        let stored = render(&edited());
        let loaded = PinboardStore::load(&mut Canned(Ok(stored.into_bytes())), Some("/Users/ada"));
        assert_eq!(loaded.settings, edited());
        assert!(loaded.warning.is_none());
    }

    #[test]
    fn an_unreadable_document_warns_once_and_uses_the_defaults() {
        let loaded = PinboardStore::load(
            &mut Canned(Err(Errno::PermissionDenied)),
            Some("/Users/ada"),
        );
        assert_eq!(loaded.settings, PinboardSettings::default());
        let warning = loaded.warning.expect("an unreadable store warns");
        assert!(warning.starts_with("desktop: pinboard settings /Users/ada/"));
        assert!(warning.contains("read failed"));
        assert!(warning.ends_with("using the defaults\n"));
    }

    #[test]
    fn an_oversize_document_warns_and_uses_the_defaults() {
        let bytes = alloc::vec![b'\n'; tairix_wallpaper::MAX_SETTINGS_LEN + 1];
        let loaded = PinboardStore::load(&mut Canned(Ok(bytes)), Some("/Users/ada"));
        assert_eq!(loaded.settings, PinboardSettings::default());
        let warning = loaded.warning.expect("an oversize store warns");
        assert!(warning.contains("longer than any valid settings document"));
    }

    #[test]
    fn a_non_utf8_document_warns_and_uses_the_defaults() {
        let loaded = PinboardStore::load(&mut Canned(Ok(alloc::vec![0xffu8])), Some("/Users/ada"));
        assert_eq!(loaded.settings, PinboardSettings::default());
        let warning = loaded.warning.expect("a non-UTF-8 store warns");
        assert!(warning.contains("not valid UTF-8"));
    }

    #[test]
    fn a_malformed_document_warns_and_uses_the_defaults() {
        let loaded = PinboardStore::load(
            &mut Canned(Ok(b"sort sideways\n".to_vec())),
            Some("/Users/ada"),
        );
        assert_eq!(loaded.settings, PinboardSettings::default());
        assert!(loaded.warning.is_some());
    }

    #[test]
    fn persisting_writes_the_rendered_document_to_the_user_store() {
        let loaded = PinboardStore::load(&mut Canned(Err(Errno::NotFound)), Some("/Users/ada"));
        let mut writer = Recorder::default();
        assert_eq!(loaded.store.persist(&mut writer, &edited()), Ok(()));
        let (path, text) = writer.written.expect("the document is written");
        assert_eq!(path, "/Users/ada/Settings/Pinboard/pinboard.conf");
        assert_eq!(text, render(&edited()));
    }

    #[test]
    fn a_refused_write_is_reported_and_adopts_nothing() {
        let loaded = PinboardStore::load(&mut Canned(Err(Errno::NotFound)), Some("/Users/ada"));
        let mut writer = Recorder {
            refuse: Some(Errno::NoSpace),
            ..Recorder::default()
        };
        assert_eq!(
            loaded.store.persist(&mut writer, &edited()),
            Err(PinboardStoreError::Write(Errno::NoSpace))
        );
        assert!(writer.written.is_none());
        assert_eq!(
            PinboardStoreError::Write(Errno::NoSpace).errno(),
            Errno::NoSpace
        );
        assert!(PinboardStoreError::NoHome.to_string().contains("no home"));
    }

    #[test]
    fn an_apply_from_the_session_user_decodes_its_document() {
        let frame = apply_frame(&edited());
        assert_eq!(
            serve_pinboard_apply(SESSION_UID, SESSION_UID, &frame),
            Ok(edited())
        );
    }

    #[test]
    fn an_apply_from_another_user_is_refused_without_decoding() {
        let frame = apply_frame(&edited());
        assert_eq!(
            serve_pinboard_apply(SESSION_UID, SESSION_UID + 1, &frame),
            Err(PinboardApplyRefusal::Unattested)
        );
        // Even a frame that could never decode is refused on identity
        // first, so a foreign caller learns nothing about the grammar.
        assert_eq!(
            serve_pinboard_apply(SESSION_UID, 0, &[]),
            Err(PinboardApplyRefusal::Unattested)
        );
        assert_eq!(
            PinboardApplyRefusal::Unattested.errno(),
            Errno::PermissionDenied
        );
    }

    #[test]
    fn a_malformed_frame_from_the_session_user_is_refused() {
        let refusal = serve_pinboard_apply(SESSION_UID, SESSION_UID, &[])
            .expect_err("a truncated frame cannot decode");
        assert_eq!(
            refusal,
            PinboardApplyRefusal::Malformed(Errno::BufferTooSmall)
        );
        assert_eq!(refusal.errno(), Errno::BufferTooSmall);
    }

    #[test]
    fn an_undecodable_document_is_refused_rather_than_guessed() {
        let document = PinboardDocument::new("sort sideways\n").expect("well-formed transport");
        let frame = PinboardRequest::Apply { document }.to_le_bytes();
        assert_eq!(
            serve_pinboard_apply(SESSION_UID, SESSION_UID, &frame),
            Err(PinboardApplyRefusal::Undecodable)
        );
        assert_eq!(PinboardApplyRefusal::Undecodable.errno(), Errno::OutOfRange);
    }

    #[test]
    fn every_refusal_states_a_reason() {
        for refusal in [
            PinboardApplyRefusal::Unattested,
            PinboardApplyRefusal::Malformed(Errno::BadMagic),
            PinboardApplyRefusal::Undecodable,
        ] {
            assert!(!refusal.reason().is_empty());
        }
    }
}
