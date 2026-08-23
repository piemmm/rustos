//! The pinboard settings document: its validated model, closed key
//! registry, and the two readings the registry has.
//!
//! One document is one [`PinboardSettings`]: the user's chosen wallpaper
//! (or none), how it is fitted to the screen, the backdrop colour shown
//! where the wallpaper does not reach, the desktop icon flow, and the sort
//! order the `Desktop` folder is listed in. Every field is a closed value
//! set, and the document itself is a plain `lib/appconf` `key = value`
//! document — the one format engine the app-data store speaks, so this
//! crate defines the *registry* over it and no grammar of its own.
//!
//! # Where the document lives
//!
//! In the desktop session's **published** app-data scope
//! (`plans/APPDATA.md` §3.11): the session is the only principal that can
//! write it, because an application publishes only its own scope, and any
//! application of that user may read it through one request shape that
//! cannot name a private one. That is what replaces the hand-rolled
//! `~/Settings/Pinboard/pinboard.conf` path the chooser used to open
//! directly — the concrete instance of the app-from-app defect the store
//! exists to close.
//!
//! # Two readings, deliberately different
//!
//! [`PinboardSettings::load`] is the **tolerant** one, for a document held
//! in a store: a value the registry refuses leaves that one field at its
//! documented default and is *named* to the caller, so one stale setting
//! costs only itself and never blanks a user's desktop.
//!
//! [`decode`] is the **strict** one, for a document that arrived over a
//! channel: a line outside the grammar, a key outside the registry, or a
//! value outside a key's closed set is a defect in the *sender* rather than
//! something a person typed, and adopting a desktop the sender did not
//! describe is worse than refusing it.
//!
//! [`PinboardSettings::document`] renders the canonical form both readings
//! accept: every registry key, in registry order, so a render/read round
//! trip is exact.

use alloc::format;
use alloc::string::{String, ToString};
use alloc::vec::Vec;
use core::fmt;

use tairix_appconf::{ConfError, Document, Lookup};

use crate::catalog;

/// Maximum length, in bytes, of a wallpaper path named by the `wallpaper`
/// key.
///
/// A fixed validation bound on untrusted input: a wallpaper path names a
/// file under the shipped store or somewhere in the user's own files, and a
/// legitimate one is a handful of path components, so this bounds how much
/// hostile work a single value can demand before the path parser even runs.
/// It is the registry's own bound and must sit inside the format engine's
/// [`tairix_appconf::MAX_VALUE_LEN`], or a path this crate accepts could
/// never be written to the store; the assertion below holds that at compile
/// time rather than leaving it to a test that could be deleted.
pub const MAX_WALLPAPER_PATH_LEN: usize = 1024;

const _: () = assert!(
    MAX_WALLPAPER_PATH_LEN <= tairix_appconf::MAX_VALUE_LEN,
    "a wallpaper path must fit one settings value"
);

/// Why a wallpaper path was refused.
#[derive(Copy, Clone, Debug, Eq, PartialEq)]
pub enum WallpaperPathError {
    /// The path exceeded [`MAX_WALLPAPER_PATH_LEN`].
    TooLong,
    /// The path was empty, relative, held an embedded control character, or
    /// otherwise failed to parse as an absolute session-view path.
    Malformed,
}

impl fmt::Display for WallpaperPathError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::TooLong => f.write_str("wallpaper path is too long"),
            Self::Malformed => f.write_str("wallpaper path is not an absolute path"),
        }
    }
}

/// A validated absolute path naming a wallpaper image.
///
/// The stored spelling is the shared path parser's canonical rendering, not
/// the caller's original wording, so two spellings of one path are one
/// value and a store write-back never carries a redundant `.`/`..` detour.
/// The path is rooted in the `/` session view: it is empty, relative,
/// non-view-rooted (an alias or volume-id spelling), or embedded-control-
/// character input that is refused, never "fixed up". A path surviving
/// validation still names untrusted content — the session reads it under
/// its own identity and the decoder sniffs and bounds it before drawing a
/// pixel; this is only the earlier, cheaper spelling refusal.
///
/// The refusals are the *path grammar's* alone. Nothing here refuses a
/// character on the document's account: `lib/appconf` quotes a value that
/// carries a `#`, a quote, or surrounding space, so every path the path
/// grammar admits round-trips through the store exactly as written. A file
/// the user genuinely named `sunset#2.png` is therefore choosable, where the
/// hand-rolled grammar this replaced had to refuse it to stay unambiguous.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct WallpaperPath(String);

impl WallpaperPath {
    /// Validate `path` as a wallpaper path.
    ///
    /// # Errors
    ///
    /// [`WallpaperPathError::TooLong`] or [`WallpaperPathError::Malformed`].
    pub fn new(path: &str) -> Result<Self, WallpaperPathError> {
        if path.len() > MAX_WALLPAPER_PATH_LEN {
            return Err(WallpaperPathError::TooLong);
        }
        let parsed = tairix_path::parse(path).map_err(|_| WallpaperPathError::Malformed)?;
        if !matches!(parsed.root(), tairix_path::Root::View) || parsed.components().is_empty() {
            return Err(WallpaperPathError::Malformed);
        }
        Ok(Self(parsed.to_string()))
    }

    /// Build the path for the shipped default wallpaper.
    ///
    /// [`crate::catalog::default_wallpaper_path`] is a compile-time-fixed,
    /// already-canonical absolute path, so this bypasses [`Self::new`]'s
    /// runtime parse rather than re-validating a value that can never fail;
    /// `the_default_wallpaper_path_is_itself_a_valid_wallpaper_path` pins
    /// that the bypass and [`Self::new`] agree.
    fn shipped_default() -> Self {
        Self(catalog::default_wallpaper_path())
    }

    /// The canonical path text.
    #[must_use]
    pub fn as_str(&self) -> &str {
        &self.0
    }
}

impl fmt::Display for WallpaperPath {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.0)
    }
}

/// The user's wallpaper choice.
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum WallpaperChoice {
    /// No wallpaper: the backdrop colour fills the whole desktop.
    None,
    /// The wallpaper image at this validated absolute path.
    Image(WallpaperPath),
}

impl WallpaperChoice {
    const NONE_VALUE: &'static str = "none";

    fn from_value(value: &str) -> Option<Self> {
        if value == Self::NONE_VALUE {
            return Some(Self::None);
        }
        WallpaperPath::new(value).ok().map(Self::Image)
    }

    fn render_value(&self) -> String {
        match self {
            Self::None => Self::NONE_VALUE.to_string(),
            Self::Image(path) => path.as_str().to_string(),
        }
    }
}

/// How a wallpaper's pixels are mapped onto the screen.
///
/// Shared by the settings document and [`crate::fit::place`], so the
/// desktop renderer and the chooser's preview can never disagree about
/// what a fit means.
#[derive(Copy, Clone, Debug, Default, Eq, PartialEq)]
pub enum WallpaperFit {
    /// Cover the screen, cropping the overflow, centred.
    #[default]
    Fill,
    /// Contain the whole image, letterboxed, centred.
    Fit,
    /// Scale to the exact screen size, ignoring aspect ratio.
    Stretch,
    /// Draw at 1:1, centred, cropped when larger than the screen.
    Centre,
    /// Draw at 1:1, repeated from the origin.
    Tile,
}

impl WallpaperFit {
    /// The canonical value spelling.
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Fill => "fill",
            Self::Fit => "fit",
            Self::Stretch => "stretch",
            Self::Centre => "centre",
            Self::Tile => "tile",
        }
    }

    /// Decode a value spelling; `None` for anything outside the closed set
    /// (case-sensitive — one canonical spelling).
    #[must_use]
    pub fn from_value(value: &str) -> Option<Self> {
        match value {
            "fill" => Some(Self::Fill),
            "fit" => Some(Self::Fit),
            "stretch" => Some(Self::Stretch),
            "centre" => Some(Self::Centre),
            "tile" => Some(Self::Tile),
            _ => None,
        }
    }
}

/// A straight-alpha-free RGB colour: the backdrop shown behind, or in place
/// of, the wallpaper.
///
/// This is deliberately **not** `tairix_theme::Rgba`: a backdrop colour is
/// always fully opaque — there is nothing further behind the desktop
/// backdrop to blend with — so carrying `Rgba`'s alpha channel would admit
/// a value this field can never mean (a translucent desktop backdrop) and a
/// render/parse round trip would have to invent an alpha to fill in on
/// read. Keeping the settings model to exactly the channels this field can
/// hold keeps the type total.
#[derive(Copy, Clone, Debug, Eq, PartialEq)]
pub struct Rgb {
    /// Red channel.
    pub r: u8,
    /// Green channel.
    pub g: u8,
    /// Blue channel.
    pub b: u8,
}

impl Rgb {
    /// Construct a colour from its channels.
    #[must_use]
    pub const fn new(r: u8, g: u8, b: u8) -> Self {
        Self { r, g, b }
    }

    /// Decode the canonical bare `rrggbb` spelling (case-insensitive hex
    /// digits, no leading `#`); `None` for anything else.
    ///
    /// A colour has exactly one spelling in this crate, and it carries no
    /// `#`. That is a registry rule rather than a grammar one — the document
    /// engine would quote a `#rrggbb` value and carry it perfectly well — and
    /// it is kept because two spellings of one colour are two ways for
    /// consumers to disagree about whether they mean the same backdrop. A
    /// `#`-prefixed value is refused here, not accepted on one path and lost
    /// on another.
    #[must_use]
    pub fn from_hex(text: &str) -> Option<Self> {
        if text.len() != 6 || !text.is_ascii() {
            return None;
        }
        let r = u8::from_str_radix(&text[0..2], 16).ok()?;
        let g = u8::from_str_radix(&text[2..4], 16).ok()?;
        let b = u8::from_str_radix(&text[4..6], 16).ok()?;
        Some(Self { r, g, b })
    }

    /// Render the canonical lowercase bare `rrggbb` spelling [`Self::from_hex`]
    /// reads back.
    #[must_use]
    pub fn to_hex(self) -> String {
        format!("{:02x}{:02x}{:02x}", self.r, self.g, self.b)
    }
}

/// The flat colour shown wherever the wallpaper does not reach, and the
/// whole backdrop when [`WallpaperChoice::None`] is set.
#[derive(Copy, Clone, Debug, Eq, PartialEq)]
pub enum Backdrop {
    /// The desktop theme's own backdrop colour.
    Theme,
    /// A user-chosen flat colour.
    Colour(Rgb),
}

impl Backdrop {
    const THEME_VALUE: &'static str = "theme";

    /// Decode the `theme` keyword or a bare [`Rgb::from_hex`] `rrggbb`
    /// colour; `None` for anything else.
    fn from_value(value: &str) -> Option<Self> {
        if value == Self::THEME_VALUE {
            return Some(Self::Theme);
        }
        Rgb::from_hex(value).map(Self::Colour)
    }

    fn render_value(self) -> String {
        match self {
            Self::Theme => Self::THEME_VALUE.to_string(),
            Self::Colour(rgb) => rgb.to_hex(),
        }
    }
}

/// Where the desktop icon grid grows from.
#[derive(Copy, Clone, Debug, Default, Eq, PartialEq)]
pub enum IconFlow {
    /// From the top-left, filling downward and growing a new column to the
    /// right.
    #[default]
    Leading,
    /// Hugging the trailing edge.
    Trailing,
}

impl IconFlow {
    /// The canonical value spelling.
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Leading => "leading",
            Self::Trailing => "trailing",
        }
    }

    /// Decode a value spelling; `None` for anything outside the closed set.
    #[must_use]
    pub fn from_value(value: &str) -> Option<Self> {
        match value {
            "leading" => Some(Self::Leading),
            "trailing" => Some(Self::Trailing),
            _ => None,
        }
    }
}

/// How the `Desktop` folder's icons are ordered.
#[derive(Copy, Clone, Debug, Default, Eq, PartialEq)]
pub enum IconSort {
    /// By file name.
    #[default]
    Name,
    /// By content-type kind.
    Kind,
    /// By file size.
    Size,
    /// By modification date.
    Date,
}

impl IconSort {
    /// The canonical value spelling.
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Name => "name",
            Self::Kind => "kind",
            Self::Size => "size",
            Self::Date => "date",
        }
    }

    /// Decode a value spelling; `None` for anything outside the closed set.
    #[must_use]
    pub fn from_value(value: &str) -> Option<Self> {
        match value {
            "name" => Some(Self::Name),
            "kind" => Some(Self::Kind),
            "size" => Some(Self::Size),
            "date" => Some(Self::Date),
            _ => None,
        }
    }
}

/// One key of the closed pinboard settings registry.
///
/// Adding a key means adding a variant here, its row in [`SettingsKey::ALL`],
/// its field on [`PinboardSettings`], and its arms in this module's private
/// `set_field` and `field_value` bridges — the compiler then forces every
/// consumer to state what the new key means. There is no free-form key
/// namespace: an unknown key fails closed at parse.
#[derive(Copy, Clone, Debug, Eq, PartialEq)]
pub enum SettingsKey {
    /// `wallpaper` — the wallpaper image, or `none`.
    Wallpaper,
    /// `fit` — how the wallpaper is mapped onto the screen.
    Fit,
    /// `backdrop` — the flat colour behind (or instead of) the wallpaper.
    Backdrop,
    /// `icons` — where the desktop icon grid grows from.
    Icons,
    /// `sort` — how the `Desktop` folder's icons are ordered.
    Sort,
}

impl SettingsKey {
    /// Every registry key, in the canonical listing (and render) order.
    pub const ALL: [Self; 5] = [
        Self::Wallpaper,
        Self::Fit,
        Self::Backdrop,
        Self::Icons,
        Self::Sort,
    ];

    /// The canonical key spelling.
    #[must_use]
    pub const fn name(self) -> &'static str {
        match self {
            Self::Wallpaper => "wallpaper",
            Self::Fit => "fit",
            Self::Backdrop => "backdrop",
            Self::Icons => "icons",
            Self::Sort => "sort",
        }
    }

    /// Decode a key spelling; `None` for anything outside the registry
    /// (keys are case-sensitive — one canonical spelling).
    #[must_use]
    pub fn from_name(name: &str) -> Option<Self> {
        Self::ALL.into_iter().find(|key| key.name() == name)
    }

    /// The value of this key on `settings`, in its canonical spelling.
    ///
    /// The one place a setting becomes text, so a writer publishing to the
    /// store and a sender rendering a document cannot spell one differently.
    #[must_use]
    pub fn value_of(self, settings: &PinboardSettings) -> String {
        field_value(settings, self)
    }
}

impl fmt::Display for SettingsKey {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.name())
    }
}

/// Why a pinboard settings document that arrived over a channel was refused.
///
/// Only [`decode`] raises these: a document held in the store is read
/// tolerantly, key by key, by [`PinboardSettings::load`].
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum DocumentRefusal {
    /// The document is outside the format engine's own bounds or grammar.
    Malformed(ConfError),
    /// A line the `key = value` grammar did not read as a setting, by its
    /// 1-based line number.
    Unparsed(usize),
    /// A key outside the closed [`SettingsKey`] registry.
    UnknownKey(String),
    /// A value outside its key's closed set, or malformed.
    InvalidValue(SettingsKey),
}

impl fmt::Display for DocumentRefusal {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Malformed(err) => write!(f, "not a settings document ({err})"),
            Self::Unparsed(line) => write!(f, "line {line}: not a setting"),
            Self::UnknownKey(key) => write!(f, "unknown pinboard settings key `{key}`"),
            Self::InvalidValue(key) => write!(f, "`{key}` is not a value that setting accepts"),
        }
    }
}

/// The per-user pinboard settings: the desktop backdrop's wallpaper, fit,
/// colour, and the `Desktop` folder's icon flow and sort order.
///
/// [`PinboardSettings::default`] is the settings an **absent** document
/// implies, exactly the table `plans/PINBOARD.md` §2 specifies: the shipped
/// default wallpaper, `Fill`, the theme's own backdrop colour, a leading
/// icon flow, and a name sort — so a fresh account or an unusable document
/// runs on a calm, fully-specified desktop rather than a guessed one.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct PinboardSettings {
    /// The wallpaper image, or [`WallpaperChoice::None`].
    pub wallpaper: WallpaperChoice,
    /// How the wallpaper is mapped onto the screen.
    pub fit: WallpaperFit,
    /// The flat colour behind, or instead of, the wallpaper.
    pub backdrop: Backdrop,
    /// Where the desktop icon grid grows from.
    pub icons: IconFlow,
    /// How the `Desktop` folder's icons are ordered.
    pub sort: IconSort,
}

impl Default for PinboardSettings {
    fn default() -> Self {
        Self {
            wallpaper: WallpaperChoice::Image(WallpaperPath::shipped_default()),
            fit: WallpaperFit::default(),
            backdrop: Backdrop::Theme,
            icons: IconFlow::default(),
            sort: IconSort::default(),
        }
    }
}

impl PinboardSettings {
    /// The settings `source` holds, and every key whose stored value the
    /// registry refused.
    ///
    /// This is the **tolerant** reading, for a document held in a store: a
    /// key the source does not set keeps its documented default, so an
    /// absent document and a fresh account are the same thing, and a value
    /// outside a key's closed set leaves that one field at its default and
    /// is named in the returned list. One stale setting therefore costs only
    /// itself — a desktop is never blanked because a single value predates
    /// this build — and the caller still reports what it could not use
    /// rather than running on a value the user cannot account for.
    ///
    /// `source` is anything the format engine can be read through: the
    /// desktop session's own published-scope handle, or the [`Document`]
    /// another application's foreign read answered with.
    #[must_use]
    pub fn load<L: Lookup + ?Sized>(source: &L) -> (Self, Vec<SettingsKey>) {
        let mut settings = Self::default();
        let mut refused = Vec::new();
        for key in SettingsKey::ALL {
            let Some(value) = source.get(key.name()) else {
                continue;
            };
            if !set_field(&mut settings, key, value) {
                refused.push(key);
            }
        }
        (settings, refused)
    }

    /// These settings as the canonical document: every registry key, in
    /// registry order.
    ///
    /// Every key is written, including one still at its default, so the
    /// document is self-describing and a render/[`decode`] round trip is
    /// exact. It is what a program that must hand a whole desktop to another
    /// sends; publishing to the store instead goes key by key, so that only
    /// what actually changed is written.
    #[must_use]
    pub fn document(&self) -> Document {
        let mut document = Document::new();
        for key in SettingsKey::ALL {
            // Every registry key is inside the format's key grammar and every
            // rendered value inside its value grammar, which
            // `the_canonical_document_holds_every_registry_key` pins; a
            // refusal here would be a defect in this registry, and dropping
            // the key is the only answer that cannot publish a wrong one.
            let _ = document.set(key.name(), &key.value_of(self));
        }
        document
    }
}

/// Set `key` on `settings` to the setting `value` names.
///
/// Returns `false` when `value` is outside `key`'s closed set; `settings`
/// is left unchanged on refusal.
#[must_use]
fn set_field(settings: &mut PinboardSettings, key: SettingsKey, value: &str) -> bool {
    match key {
        SettingsKey::Wallpaper => {
            let Some(wallpaper) = WallpaperChoice::from_value(value) else {
                return false;
            };
            settings.wallpaper = wallpaper;
        }
        SettingsKey::Fit => {
            let Some(fit) = WallpaperFit::from_value(value) else {
                return false;
            };
            settings.fit = fit;
        }
        SettingsKey::Backdrop => {
            let Some(backdrop) = Backdrop::from_value(value) else {
                return false;
            };
            settings.backdrop = backdrop;
        }
        SettingsKey::Icons => {
            let Some(icons) = IconFlow::from_value(value) else {
                return false;
            };
            settings.icons = icons;
        }
        SettingsKey::Sort => {
            let Some(sort) = IconSort::from_value(value) else {
                return false;
            };
            settings.sort = sort;
        }
    }
    true
}

/// The current value of `key` on `settings`, in its canonical spelling.
fn field_value(settings: &PinboardSettings, key: SettingsKey) -> String {
    match key {
        SettingsKey::Wallpaper => settings.wallpaper.render_value(),
        SettingsKey::Fit => settings.fit.as_str().to_string(),
        SettingsKey::Backdrop => settings.backdrop.render_value(),
        SettingsKey::Icons => settings.icons.as_str().to_string(),
        SettingsKey::Sort => settings.sort.as_str().to_string(),
    }
}

/// Read a pinboard settings document that arrived over a channel, refusing
/// anything the registry does not fully understand.
///
/// This is the **strict** reading. A document on the wire was rendered by a
/// program from this same registry, so a line outside the grammar, a key
/// outside the registry, or a value outside a key's closed set means the
/// sender is not describing a desktop this build can show — and adopting
/// half of what it asked for would put the user in front of a backdrop
/// nobody chose. A document that names only some keys is *not* a refusal:
/// the rest keep their documented defaults, exactly as an absent document
/// does.
///
/// # Errors
///
/// The [`DocumentRefusal`] naming what was wrong; the document is refused
/// whole, never half-applied.
pub fn decode(text: &str) -> Result<PinboardSettings, DocumentRefusal> {
    let document = Document::parse(text).map_err(DocumentRefusal::Malformed)?;
    if let Some(line) = document.unparsed().next() {
        return Err(DocumentRefusal::Unparsed(line.line));
    }
    let mut settings = PinboardSettings::default();
    for setting in document.settings() {
        let key = SettingsKey::from_name(setting.key)
            .ok_or_else(|| DocumentRefusal::UnknownKey(setting.key.to_string()))?;
        if !set_field(&mut settings, key, setting.value) {
            return Err(DocumentRefusal::InvalidValue(key));
        }
    }
    Ok(settings)
}

#[cfg(test)]
#[path = "settings_tests.rs"]
mod tests;
