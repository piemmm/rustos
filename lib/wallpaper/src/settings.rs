//! The pinboard settings document: its validated model, closed key
//! registry, bounded fail-closed parser, and canonical render.
//!
//! One document is one [`PinboardSettings`]: the user's chosen wallpaper
//! (or none), how it is fitted to the screen, the backdrop colour shown
//! where the wallpaper does not reach, the desktop icon flow, and the sort
//! order the `Desktop` folder is listed in. Every field is a closed value
//! set — a line names a [`SettingsKey`] registry key, whitespace, and one
//! value from that key's own vocabulary — the same `key value` / `#`
//! comment grammar every line-oriented TAIRiX configuration store shares.
//!
//! Because `#` begins a comment, no value may contain one: a backdrop colour
//! is spelled as bare `rrggbb` digits ([`Rgb::from_hex`]) rather than
//! `#rrggbb`, and a wallpaper path carrying a `#` is refused outright rather
//! than silently truncated at it.
//!
//! [`parse`] and [`render`] are inverses for every canonical document:
//! parsing a rendered document yields the same settings, and rendering
//! parsed settings yields byte-identical text. [`render`] always emits
//! every key, including one still at its default, so a document a user
//! opens always shows the whole registry.

use alloc::format;
use alloc::string::{String, ToString};
use core::fmt;

use tairix_util::conf::strip_comment;

use crate::catalog;

/// Maximum length, in bytes, of a whole pinboard settings document.
///
/// A fixed security and format bound, not a growable capacity: the
/// document holds exactly five short settings, so this is sized to the
/// longest possible rendered document (a [`MAX_WALLPAPER_PATH_LEN`] wallpaper
/// path plus the four short fields and their keys) with slack for comments a
/// user may have added, never to admit an unboundedly large store.
pub const MAX_SETTINGS_LEN: usize = MAX_WALLPAPER_PATH_LEN + 1024;

/// Maximum length, in bytes, of a wallpaper path named by the `wallpaper`
/// key.
///
/// A fixed validation bound on untrusted input: a wallpaper path names a
/// file under the shipped store or somewhere in the user's own files, and a
/// legitimate one is a handful of path components, so this bounds how much
/// hostile work a single value can demand before the path parser even runs.
pub const MAX_WALLPAPER_PATH_LEN: usize = 1024;

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
        let canonical = parsed.to_string();
        // A value stored in the document is the remainder of its line: it
        // must hold no `#` (which would begin a comment) and no leading or
        // trailing whitespace (which the parser trims), or a render/parse
        // round trip would not reproduce it.
        if canonical.contains('#') || canonical.trim() != canonical {
            return Err(WallpaperPathError::Malformed);
        }
        Ok(Self(canonical))
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
    /// `#`: the settings document's own comment grammar cuts a line at the
    /// first `#`, so a `#rrggbb` value would be truncated to nothing before
    /// a colour parser ever saw it. Keeping the one spelling the document
    /// can hold means a consumer cannot pick the wrong one — a `#`-prefixed
    /// value is refused here, not silently accepted on one path and lost on
    /// another.
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
}

impl fmt::Display for SettingsKey {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.name())
    }
}

/// Why a pinboard settings document was refused.
#[derive(Copy, Clone, Debug, Eq, PartialEq)]
pub enum ParseError {
    /// The document exceeded [`MAX_SETTINGS_LEN`].
    DocumentTooLong,
    /// A line names a key outside the closed [`SettingsKey`] registry.
    UnknownKey,
    /// A line names a key but carries no value.
    MissingValue,
    /// A registry key appeared more than once.
    DuplicateKey,
    /// A value was outside its key's closed set, or malformed.
    InvalidValue,
}

impl fmt::Display for ParseError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::DocumentTooLong => f.write_str("pinboard settings document is too long"),
            Self::UnknownKey => f.write_str("unknown pinboard settings key"),
            Self::MissingValue => f.write_str("setting has no value"),
            Self::DuplicateKey => f.write_str("setting is repeated"),
            Self::InvalidValue => f.write_str("setting value is invalid"),
        }
    }
}

/// A refused pinboard settings document, and where in it the refusal was
/// raised.
#[derive(Copy, Clone, Debug, Eq, PartialEq)]
pub struct SettingsError {
    line: Option<usize>,
    kind: ParseError,
}

impl SettingsError {
    /// The 1-based line the refusal was raised at, or `None` for a
    /// whole-document refusal that belongs to no single line.
    #[must_use]
    pub fn line(&self) -> Option<usize> {
        self.line
    }

    /// What was wrong.
    #[must_use]
    pub fn kind(&self) -> ParseError {
        self.kind
    }
}

impl fmt::Display for SettingsError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self.line {
            Some(line) => write!(f, "line {line}: {}", self.kind),
            None => write!(f, "{}", self.kind),
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

/// Parse a pinboard settings document.
///
/// Parsing starts from [`PinboardSettings::default`] and applies each
/// setting line in turn, so a document naming only a subset of keys leaves
/// the rest at their documented default.
///
/// # Errors
///
/// [`SettingsError`] — the document is refused whole, never half-applied.
/// An absent or unusable document is not this function's concern: a caller
/// that cannot read a store, or whose read yields a document this refuses,
/// falls back to [`PinboardSettings::default`] rather than guessing at a
/// partial intent.
pub fn parse(text: &str) -> Result<PinboardSettings, SettingsError> {
    if text.len() > MAX_SETTINGS_LEN {
        return Err(SettingsError {
            line: None,
            kind: ParseError::DocumentTooLong,
        });
    }

    let mut settings = PinboardSettings::default();
    let mut seen = [false; SettingsKey::ALL.len()];

    for (index, raw) in text.lines().enumerate() {
        let number = index + 1;
        let at = |kind: ParseError| SettingsError {
            line: Some(number),
            kind,
        };

        let line = strip_comment(raw).trim();
        if line.is_empty() {
            continue;
        }

        let mut fields = line.splitn(2, char::is_whitespace);
        let name = fields.next().unwrap_or_default();
        let value = fields.next().map(str::trim).filter(|v| !v.is_empty());

        let key = SettingsKey::from_name(name).ok_or_else(|| at(ParseError::UnknownKey))?;
        let value = value.ok_or_else(|| at(ParseError::MissingValue))?;

        let position = SettingsKey::ALL
            .iter()
            .position(|k| *k == key)
            .unwrap_or_default();
        if seen[position] {
            return Err(at(ParseError::DuplicateKey));
        }
        seen[position] = true;

        if !set_field(&mut settings, key, value) {
            return Err(at(ParseError::InvalidValue));
        }
    }

    Ok(settings)
}

/// Render `settings` as the canonical document text.
///
/// Every key is written in [`SettingsKey::ALL`] order — including a key
/// still at its default — so the document a user opens always shows the
/// whole registry, and a render/parse round trip is exact.
#[must_use]
pub fn render(settings: &PinboardSettings) -> String {
    use core::fmt::Write as _;

    let mut text = String::new();
    for key in SettingsKey::ALL {
        let _ = writeln!(text, "{key} {}", field_value(settings, key));
    }
    text
}

#[cfg(test)]
#[path = "settings_tests.rs"]
mod tests;
