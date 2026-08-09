//! The terminal profile: the settings a user keeps between sessions, and the
//! document they are stored in.
//!
//! One profile is one [`Profile`]: the colour scheme in force, the sixteen
//! ANSI colours and three screen roles of the user's own custom scheme, the
//! text size, and the strength of every screen effect. It is stored per user
//! at [`user_profile_path`] in the same `key value` / `#` comment grammar
//! every line-oriented TAIRiX configuration store shares, so a user can read
//! and edit it with any text editor.
//!
//! Because `#` begins a comment, no value may contain one: a colour is
//! spelled as bare `rrggbb` digits ([`Rgb::from_hex`]) rather than `#rrggbb`.
//!
//! [`parse`] and [`render`] are inverses for every canonical document:
//! parsing a rendered document yields the same profile, and rendering a
//! parsed profile yields byte-identical text. [`render`] always emits every
//! key, including one still at its default, so a document a user opens shows
//! the whole registry.
//!
//! An **absent** document is the ordinary state of a fresh account and means
//! [`Profile::default`]; a document the parser refuses is refused whole,
//! never half-applied, and the caller runs on the defaults rather than
//! guessing at a partial intent.

use alloc::format;
use alloc::string::{String, ToString};
use core::fmt;

use tairix_util::conf::strip_comment;

use crate::effects::{Effects, FULL, MIN_OPACITY};
use crate::scheme::{ColorScheme, Rgb, Scheme, ANSI_COLORS};

/// Maximum length, in bytes, of a whole profile document.
///
/// A fixed security and format bound, not a growable capacity: the document
/// holds a fixed registry of short settings, so this is sized to the longest
/// canonical document with slack for comments a user may have added, never to
/// admit an unboundedly large store.
pub const MAX_PROFILE_LEN: usize = 8192;

/// The smallest text size the profile may name, in logical pixels.
///
/// Below this the shared monospace face loses the strokes that keep a
/// character grid legible.
pub const MIN_FONT_SIZE_PX: u16 = 8;

/// The largest text size the profile may name, in logical pixels.
pub const MAX_FONT_SIZE_PX: u16 = 48;

/// The text size a terminal opens at when the user has not chosen one, in
/// logical pixels.
///
/// Sized so the conventional 80×25 screen, drawn with the shared monospace
/// face and wrapped in the theme's window furniture, fits inside a 640×480
/// display without covering it: the face advances seven physical pixels per
/// column at this height, so the grid is 560×350 and the framed window
/// 562×380. A denser display multiplies this through the desktop scale, and a
/// display too small even for this steps the size down until the grid fits
/// (`fit_font_size`).
pub const DEFAULT_FONT_SIZE_PX: u16 = 14;

/// The step a *Larger text* / *Smaller text* command moves the text size by,
/// in logical pixels.
pub const FONT_SIZE_STEP_PX: u16 = 1;

/// The profile store's directory name inside a `Settings/` tree — the one
/// component a settings browser creates.
pub const PROFILE_SETTINGS_SUBDIR: &str = "Terminal";

/// The profile document's file name.
pub const PROFILE_FILE: &str = "terminal.conf";

/// The per-user profile path inside `home`, the account's home directory
/// exactly as the session inherited it (`HOME`).
///
/// A trailing `/` is normalised away; an empty or root home yields `None`
/// rather than a guessed rootward path, so a caller with no home fails
/// closed.
#[must_use]
pub fn user_profile_path(home: &str) -> Option<String> {
    let home = home.strip_suffix('/').unwrap_or(home);
    if home.is_empty() || home == "/" {
        return None;
    }
    Some(format!(
        "{home}/Settings/{PROFILE_SETTINGS_SUBDIR}/{PROFILE_FILE}"
    ))
}

/// One key of the closed profile registry.
///
/// Adding a key means adding a variant here, its row in [`ProfileKey::ALL`],
/// and its arms in this module's private `set_field` and `field_value`
/// bridges — the compiler then forces every consumer to state what the new
/// key means. There is no free-form key namespace: an unknown key fails
/// closed at parse.
#[derive(Copy, Clone, Debug, Eq, PartialEq)]
pub enum ProfileKey {
    /// `scheme` — which colour scheme is in force.
    Scheme,
    /// `font-size` — the text size in logical pixels.
    FontSize,
    /// `opacity` — how opaque the background is, in permille.
    Opacity,
    /// `blur` — how strongly the compositor blurs the backdrop, in permille.
    Blur,
    /// `scanlines` — how deeply alternate rows are dimmed, in permille.
    ScanLines,
    /// `fuzz` — how much per-pixel jitter is added, in permille.
    Fuzz,
    /// `phosphor` — how long a lit pixel persists, in permille.
    Phosphor,
    /// `wobble` — how far rows are displaced, in permille.
    Wobble,
    /// `custom-background` — the custom scheme's default background.
    CustomBackground,
    /// `custom-foreground` — the custom scheme's default foreground.
    CustomForeground,
    /// `custom-cursor` — the custom scheme's cursor block.
    CustomCursor,
    /// `custom-cursor-text` — the glyph colour inside the cursor block.
    CustomCursorText,
    /// `custom-ansi` — the custom scheme's sixteen ANSI colours, in order.
    CustomAnsi,
}

impl ProfileKey {
    /// Every registry key, in the canonical listing (and render) order.
    pub const ALL: [Self; 13] = [
        Self::Scheme,
        Self::FontSize,
        Self::Opacity,
        Self::Blur,
        Self::ScanLines,
        Self::Fuzz,
        Self::Phosphor,
        Self::Wobble,
        Self::CustomBackground,
        Self::CustomForeground,
        Self::CustomCursor,
        Self::CustomCursorText,
        Self::CustomAnsi,
    ];

    /// The canonical key spelling.
    #[must_use]
    pub const fn name(self) -> &'static str {
        match self {
            Self::Scheme => "scheme",
            Self::FontSize => "font-size",
            Self::Opacity => "opacity",
            Self::Blur => "blur",
            Self::ScanLines => "scanlines",
            Self::Fuzz => "fuzz",
            Self::Phosphor => "phosphor",
            Self::Wobble => "wobble",
            Self::CustomBackground => "custom-background",
            Self::CustomForeground => "custom-foreground",
            Self::CustomCursor => "custom-cursor",
            Self::CustomCursorText => "custom-cursor-text",
            Self::CustomAnsi => "custom-ansi",
        }
    }

    /// Decode a key spelling; `None` for anything outside the registry (keys
    /// are case-sensitive — one canonical spelling).
    #[must_use]
    pub fn from_name(name: &str) -> Option<Self> {
        Self::ALL.into_iter().find(|key| key.name() == name)
    }
}

impl fmt::Display for ProfileKey {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.name())
    }
}

/// Why a profile document was refused.
#[derive(Copy, Clone, Debug, Eq, PartialEq)]
pub enum ParseError {
    /// The document exceeded [`MAX_PROFILE_LEN`].
    DocumentTooLong,
    /// A line names a key outside the closed [`ProfileKey`] registry.
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
            Self::DocumentTooLong => f.write_str("terminal profile document is too long"),
            Self::UnknownKey => f.write_str("unknown terminal profile key"),
            Self::MissingValue => f.write_str("setting has no value"),
            Self::DuplicateKey => f.write_str("setting is repeated"),
            Self::InvalidValue => f.write_str("setting value is invalid"),
        }
    }
}

/// A refused profile document, and where in it the refusal was raised.
#[derive(Copy, Clone, Debug, Eq, PartialEq)]
pub struct ProfileError {
    line: Option<usize>,
    kind: ParseError,
}

impl ProfileError {
    /// The 1-based line the refusal was raised at, or `None` for a
    /// whole-document refusal that belongs to no single line.
    #[must_use]
    pub const fn line(&self) -> Option<usize> {
        self.line
    }

    /// What was wrong.
    #[must_use]
    pub const fn kind(&self) -> ParseError {
        self.kind
    }
}

impl fmt::Display for ProfileError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self.line {
            Some(line) => write!(f, "line {line}: {}", self.kind),
            None => write!(f, "{}", self.kind),
        }
    }
}

/// The user's terminal profile.
///
/// [`Profile::default`] is what an absent document implies: the system colour
/// scheme, the default text size, and every screen effect off.
#[derive(Copy, Clone, Debug, Eq, PartialEq)]
pub struct Profile {
    /// The colour scheme in force.
    pub scheme: Scheme,
    /// The text size, in logical pixels, always within
    /// [`MIN_FONT_SIZE_PX`]..=[`MAX_FONT_SIZE_PX`].
    pub font_size_px: u16,
    /// The strength of every screen effect.
    pub effects: Effects,
    /// The user's own scheme, used when [`scheme`](Self::scheme) is
    /// [`Scheme::Custom`] and editable whether or not it is in force.
    pub custom: ColorScheme,
}

impl Default for Profile {
    fn default() -> Self {
        Self {
            scheme: Scheme::System,
            font_size_px: DEFAULT_FONT_SIZE_PX,
            effects: Effects::default(),
            custom: default_custom_scheme(),
        }
    }
}

impl Profile {
    /// Clamp every field into its valid range.
    ///
    /// Applied after parsing and after any in-app edit, so a profile can
    /// never carry a size the font will not render or an opacity that would
    /// make text unreadable.
    pub fn clamp(&mut self) {
        self.font_size_px = self.font_size_px.clamp(MIN_FONT_SIZE_PX, MAX_FONT_SIZE_PX);
        self.effects.opacity = self.effects.opacity.clamp(MIN_OPACITY, FULL);
        self.effects.blur = self.effects.blur.min(FULL);
        self.effects.scanlines = self.effects.scanlines.min(FULL);
        self.effects.fuzz = self.effects.fuzz.min(FULL);
        self.effects.phosphor = self.effects.phosphor.min(FULL);
        self.effects.wobble = self.effects.wobble.min(FULL);
    }

    /// The text size one step larger, clamped.
    pub fn enlarge(&mut self) {
        self.font_size_px = self
            .font_size_px
            .saturating_add(FONT_SIZE_STEP_PX)
            .min(MAX_FONT_SIZE_PX);
    }

    /// The text size one step smaller, clamped.
    pub fn reduce(&mut self) {
        self.font_size_px = self
            .font_size_px
            .saturating_sub(FONT_SIZE_STEP_PX)
            .max(MIN_FONT_SIZE_PX);
    }
}

/// The custom scheme a user starts from before editing one of their own: the
/// xterm palette on the classic black ground, so every slot is already a
/// sensible colour rather than an undifferentiated block.
fn default_custom_scheme() -> ColorScheme {
    let mut scheme = Scheme::Contrast.palette().unwrap_or(ColorScheme {
        background: Rgb::new(0, 0, 0),
        foreground: Rgb::new(0xe8, 0xe8, 0xe8),
        cursor: Rgb::new(0xe8, 0xe8, 0xe8),
        cursor_text: Rgb::new(0, 0, 0),
        ansi: [Rgb::new(0, 0, 0); ANSI_COLORS],
    });
    scheme.cursor = Rgb::new(0x7a, 0xc8, 0xff);
    scheme.cursor_text = Rgb::new(0x00, 0x00, 0x00);
    scheme
}

/// Set `key` on `profile` to the setting `value` names.
///
/// Returns `false` when `value` is outside `key`'s closed set; `profile` is
/// left unchanged on refusal.
#[must_use]
fn set_field(profile: &mut Profile, key: ProfileKey, value: &str) -> bool {
    match key {
        ProfileKey::Scheme => match Scheme::from_name(value) {
            Some(scheme) => profile.scheme = scheme,
            None => return false,
        },
        ProfileKey::FontSize => match parse_bounded(value, MIN_FONT_SIZE_PX, MAX_FONT_SIZE_PX) {
            Some(size) => profile.font_size_px = size,
            None => return false,
        },
        ProfileKey::Opacity => match parse_bounded(value, MIN_OPACITY, FULL) {
            Some(permille) => profile.effects.opacity = permille,
            None => return false,
        },
        ProfileKey::Blur => match parse_bounded(value, 0, FULL) {
            Some(permille) => profile.effects.blur = permille,
            None => return false,
        },
        ProfileKey::ScanLines => match parse_bounded(value, 0, FULL) {
            Some(permille) => profile.effects.scanlines = permille,
            None => return false,
        },
        ProfileKey::Fuzz => match parse_bounded(value, 0, FULL) {
            Some(permille) => profile.effects.fuzz = permille,
            None => return false,
        },
        ProfileKey::Phosphor => match parse_bounded(value, 0, FULL) {
            Some(permille) => profile.effects.phosphor = permille,
            None => return false,
        },
        ProfileKey::Wobble => match parse_bounded(value, 0, FULL) {
            Some(permille) => profile.effects.wobble = permille,
            None => return false,
        },
        ProfileKey::CustomBackground => match Rgb::from_hex(value) {
            Some(color) => profile.custom.background = color,
            None => return false,
        },
        ProfileKey::CustomForeground => match Rgb::from_hex(value) {
            Some(color) => profile.custom.foreground = color,
            None => return false,
        },
        ProfileKey::CustomCursor => match Rgb::from_hex(value) {
            Some(color) => profile.custom.cursor = color,
            None => return false,
        },
        ProfileKey::CustomCursorText => match Rgb::from_hex(value) {
            Some(color) => profile.custom.cursor_text = color,
            None => return false,
        },
        ProfileKey::CustomAnsi => match parse_ansi(value) {
            Some(ansi) => profile.custom.ansi = ansi,
            None => return false,
        },
    }
    true
}

/// The current value of `key` on `profile`, in its canonical spelling.
fn field_value(profile: &Profile, key: ProfileKey) -> String {
    match key {
        ProfileKey::Scheme => profile.scheme.name().to_string(),
        ProfileKey::FontSize => profile.font_size_px.to_string(),
        ProfileKey::Opacity => profile.effects.opacity.to_string(),
        ProfileKey::Blur => profile.effects.blur.to_string(),
        ProfileKey::ScanLines => profile.effects.scanlines.to_string(),
        ProfileKey::Fuzz => profile.effects.fuzz.to_string(),
        ProfileKey::Phosphor => profile.effects.phosphor.to_string(),
        ProfileKey::Wobble => profile.effects.wobble.to_string(),
        ProfileKey::CustomBackground => hex(profile.custom.background),
        ProfileKey::CustomForeground => hex(profile.custom.foreground),
        ProfileKey::CustomCursor => hex(profile.custom.cursor),
        ProfileKey::CustomCursorText => hex(profile.custom.cursor_text),
        ProfileKey::CustomAnsi => render_ansi(&profile.custom.ansi),
    }
}

/// One colour in the canonical bare `rrggbb` spelling.
fn hex(color: Rgb) -> String {
    let mut text = String::new();
    color.write_hex(&mut text);
    text
}

/// Decode a decimal `value` bounded to `min..=max`; `None` for a
/// non-decimal, an empty value, or one outside the range.
///
/// A value outside the range is refused rather than clamped: the document
/// said something the registry does not allow, and silently changing it would
/// hide the mistake from the user who wrote it.
fn parse_bounded(value: &str, min: u16, max: u16) -> Option<u16> {
    let parsed: u16 = value.parse().ok()?;
    (min..=max).contains(&parsed).then_some(parsed)
}

/// Decode the sixteen space-separated bare `rrggbb` colours of a custom ANSI
/// palette; `None` unless exactly sixteen well-formed colours are present.
fn parse_ansi(value: &str) -> Option<[Rgb; ANSI_COLORS]> {
    let mut colors = [Rgb::default(); ANSI_COLORS];
    let mut seen = 0;
    for field in value.split_whitespace() {
        let slot = colors.get_mut(seen)?;
        *slot = Rgb::from_hex(field)?;
        seen += 1;
    }
    (seen == ANSI_COLORS).then_some(colors)
}

/// Render sixteen ANSI colours as the space-separated bare `rrggbb` list
/// [`parse_ansi`] reads back.
fn render_ansi(ansi: &[Rgb; ANSI_COLORS]) -> String {
    let mut text = String::new();
    for (index, color) in ansi.iter().enumerate() {
        if index > 0 {
            text.push(' ');
        }
        color.write_hex(&mut text);
    }
    text
}

/// Parse a terminal profile document.
///
/// Parsing starts from [`Profile::default`] and applies each setting line in
/// turn, so a document naming only a subset of keys leaves the rest at their
/// documented default.
///
/// # Errors
///
/// [`ProfileError`] — the document is refused whole, never half-applied. An
/// absent or unusable document is not this function's concern: a caller that
/// cannot read a store, or whose read yields a document this refuses, falls
/// back to [`Profile::default`] rather than guessing at a partial intent.
pub fn parse(text: &str) -> Result<Profile, ProfileError> {
    if text.len() > MAX_PROFILE_LEN {
        return Err(ProfileError {
            line: None,
            kind: ParseError::DocumentTooLong,
        });
    }

    let mut profile = Profile::default();
    let mut seen = [false; ProfileKey::ALL.len()];

    for (index, raw) in text.lines().enumerate() {
        let number = index + 1;
        let at = |kind: ParseError| ProfileError {
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

        let key = ProfileKey::from_name(name).ok_or_else(|| at(ParseError::UnknownKey))?;
        let value = value.ok_or_else(|| at(ParseError::MissingValue))?;

        let position = ProfileKey::ALL
            .iter()
            .position(|candidate| *candidate == key)
            .unwrap_or_default();
        if seen.get(position).copied().unwrap_or(false) {
            return Err(at(ParseError::DuplicateKey));
        }
        if let Some(slot) = seen.get_mut(position) {
            *slot = true;
        }

        if !set_field(&mut profile, key, value) {
            return Err(at(ParseError::InvalidValue));
        }
    }

    Ok(profile)
}

/// Render `profile` as the canonical document text.
///
/// Every key is written in [`ProfileKey::ALL`] order — including a key still
/// at its default — so the document a user opens always shows the whole
/// registry, and a render/parse round trip is exact.
#[must_use]
pub fn render(profile: &Profile) -> String {
    use core::fmt::Write as _;

    let mut text = String::new();
    for key in ProfileKey::ALL {
        let _ = writeln!(text, "{key} {}", field_value(profile, key));
    }
    text
}

#[cfg(test)]
#[path = "profile_tests.rs"]
mod tests;
