//! The terminal profile: the settings a user keeps between sessions, and how
//! they reach the app-data store.
//!
//! One profile is one [`Profile`]: the colour scheme in force, the sixteen
//! ANSI colours and three screen roles of the user's own custom scheme, the
//! text size, and the strength of every screen effect. It is a closed registry
//! of dotted keys ([`ProfileKey`]) in the OS app-data store, reached through
//! [`tairix_appdata`] — so it is private to this application, gated on the
//! kernel-attested bundle identity, and readable or writable by no other app
//! the user launches.
//!
//! Because `#` begins a comment in the store's format, no value carries one: a
//! colour is spelled as bare `rrggbb` digits ([`Rgb::from_hex`]) rather than
//! `#rrggbb`.
//!
//! # What a save writes, and what it does not
//!
//! [`Profile::save`] writes only the keys whose value differs from what the
//! store's layers already imply, so a user's own document holds what the user
//! actually changed and nothing else — not a copy of every default. A key no
//! layer sets reads as its documented default, and [`Profile::clear`] removes
//! the user's opinions so the layers beneath (the machine's policy, the
//! bundle's shipped defaults) apply again.
//!
//! A value the registry refuses — a size outside its bounds, a malformed
//! colour — leaves that one field at its default and is *named* to the caller,
//! which reports it. One broken setting therefore costs only itself, and never
//! silently becomes something else.

use alloc::string::{String, ToString};
use alloc::vec::Vec;

use tairix_abi::Errno;
use tairix_appdata::Settings;

use crate::effects::{Effects, FULL, MIN_OPACITY};
use crate::scheme::{ColorScheme, Rgb, Scheme, ANSI_COLORS};

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

/// One key of the closed profile registry.
///
/// Adding a key means adding a variant here, its row in [`ProfileKey::ALL`],
/// and its arms in this module's private `set_field` and `field_value`
/// bridges — the compiler then forces every consumer to state what the new
/// key means. The store's key namespace is open, but this application's is
/// closed: a key outside the registry is one this profile does not read.
#[derive(Copy, Clone, Debug, Eq, PartialEq)]
pub enum ProfileKey {
    /// `scheme` — which colour scheme is in force.
    Scheme,
    /// `font.size` — the text size in logical pixels.
    FontSize,
    /// `effects.opacity` — how opaque the background is, in permille.
    Opacity,
    /// `effects.blur` — how strongly the compositor blurs the backdrop, in
    /// permille.
    Blur,
    /// `effects.scanlines` — how deeply alternate rows are dimmed, in permille.
    ScanLines,
    /// `effects.fuzz` — how much per-pixel jitter is added, in permille.
    Fuzz,
    /// `effects.phosphor` — how long a lit pixel persists, in permille.
    Phosphor,
    /// `effects.wobble` — how far rows are displaced, in permille.
    Wobble,
    /// `custom.background` — the custom scheme's default background.
    CustomBackground,
    /// `custom.foreground` — the custom scheme's default foreground.
    CustomForeground,
    /// `custom.cursor` — the custom scheme's cursor block.
    CustomCursor,
    /// `custom.cursor-text` — the glyph colour inside the cursor block.
    CustomCursorText,
    /// `custom.ansi` — the custom scheme's sixteen ANSI colours, in order.
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
            Self::FontSize => "font.size",
            Self::Opacity => "effects.opacity",
            Self::Blur => "effects.blur",
            Self::ScanLines => "effects.scanlines",
            Self::Fuzz => "effects.fuzz",
            Self::Phosphor => "effects.phosphor",
            Self::Wobble => "effects.wobble",
            Self::CustomBackground => "custom.background",
            Self::CustomForeground => "custom.foreground",
            Self::CustomCursor => "custom.cursor",
            Self::CustomCursorText => "custom.cursor-text",
            Self::CustomAnsi => "custom.ansi",
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

    /// The profile the store's layers imply, and the keys whose stored value
    /// the registry refused.
    ///
    /// Every key the layers do not set reads as its documented default, so a
    /// fresh account and a hand-cleared store both yield
    /// [`Profile::default`]. A refused value leaves that one field at its
    /// default and is named in the returned list, so a caller reports the
    /// broken setting instead of running on a value the user cannot account
    /// for.
    #[must_use]
    pub fn load(settings: &Settings<'_>) -> (Self, Vec<ProfileKey>) {
        let mut profile = Self::default();
        let mut refused = Vec::new();
        for key in ProfileKey::ALL {
            let Some(value) = settings.get(key.name()) else {
                continue;
            };
            if !set_field(&mut profile, key, value) {
                refused.push(key);
            }
        }
        profile.clamp();
        (profile, refused)
    }

    /// Publish this profile, writing only what the store's layers do not
    /// already imply.
    ///
    /// A key whose effective value already matches is left alone, so saving a
    /// profile the user did not change rewrites nothing, and a value that
    /// comes from the machine's policy or the bundle's defaults is never
    /// copied up into the user's own document. The whole profile lands as one
    /// atomic commit.
    ///
    /// # Errors
    ///
    /// The app-data service's own typed refusal — no service bound, no store
    /// for a caller running no signed bundle, or an unreachable volume. The
    /// edits stay staged, so a caller may retry.
    pub fn save(&self, settings: &mut Settings<'_>) -> Result<(), Errno> {
        let (stored, _) = Self::load(settings);
        for key in ProfileKey::ALL {
            let value = field_value(self, key);
            if field_value(&stored, key) == value {
                continue;
            }
            // The registry's own spellings are inside the format's grammar, so
            // a refusal here would be a defect in this module rather than a
            // user's mistake; it is reported as a refused write either way.
            settings
                .set(key.name(), &value)
                .map_err(|_| Errno::OutOfRange)?;
        }
        settings.commit()
    }

    /// Remove every profile key from the store, so the layers beneath the
    /// user's own document apply again.
    ///
    /// This is what *Restore defaults* means: not "write the shipped values
    /// into my document" but "stop having an opinion". The profile that then
    /// applies is [`Profile::load`]'s answer, which may be the machine's
    /// policy rather than this application's own default.
    ///
    /// # Errors
    ///
    /// As [`Profile::save`].
    pub fn clear(settings: &mut Settings<'_>) -> Result<(), Errno> {
        for key in ProfileKey::ALL {
            settings.unset(key.name());
        }
        settings.commit()
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

#[cfg(test)]
#[path = "profile_tests.rs"]
mod tests;
