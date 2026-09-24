//! The desktop a window is displayed on, as its session reports it: the
//! screen extent, the UI scale, the four axes of how its theme is drawn —
//! light or dark, contrast, density, and motion — and the one double-click
//! interval every surface pairs presses under.
//!
//! These are the facts an application needs before it can lay itself out
//! honestly — how large the screen it will be shown on is, how many
//! physical pixels a logical one is worth, and how the user has asked the
//! desktop to be drawn. All of them belong to the seat's desktop, all are
//! known to the session that composites it, and none describes another
//! principal's data or authorises an action: they are descriptive,
//! seat-scoped, and delivered over the window channel the application
//! already holds ([`crate::window_ipc`]), so learning them needs no
//! capability and opens no new endpoint.
//!
//! The accessibility axes travel with the appearance rather than beside
//! it because they are one decision to a reader and one repaint to an
//! application: a user who turns contrast up has not changed the screen,
//! and an application that learned only half of what changed would draw
//! the other half the way the user just stopped asking for.
//!
//! The record travels in two places, from one definition: an application
//! *asks* for it with `QueryDesktop` — before it opens a window, so its
//! very first frame is the right size, at the right density, in the right
//! colours — and the session *publishes* it on the
//! [`NoticeTopic::Desktop`](crate::notice::NoticeTopic::Desktop) system
//! notice whenever it changes any of it, so an application converges on the
//! change even with no window open.
//!
//! Every decode fails closed: a zero extent, a zero scale, an unknown
//! appearance code, or a dirty reserved byte is refused rather than
//! guessed at.

use crate::le::{put_u16, put_u32, read_u16, read_u32};
use crate::time::Duration64;
use crate::Errno;

/// Which way round a theme's colours run.
///
/// The vocabulary lives here, in the ABI, because it crosses the window
/// channel: `lib/theme` re-exports this very type rather than restating
/// it, so the byte on the wire and the value a theme carries can never
/// drift apart.
#[derive(Copy, Clone, Debug, Default, Eq, PartialEq, Hash)]
pub enum Appearance {
    /// Light foreground on dark surfaces. TAIRiX's default.
    #[default]
    Dark,
    /// Dark foreground on light surfaces.
    Light,
}

/// Wire code of [`Appearance::Dark`]. Zero is deliberately not a valid
/// appearance, so an all-zero frame can never decode as a desktop.
const APPEARANCE_DARK: u8 = 1;
/// Wire code of [`Appearance::Light`].
const APPEARANCE_LIGHT: u8 = 2;

impl Appearance {
    /// This appearance's wire code.
    #[must_use]
    pub const fn code(self) -> u8 {
        match self {
            Self::Dark => APPEARANCE_DARK,
            Self::Light => APPEARANCE_LIGHT,
        }
    }

    /// The appearance `code` names.
    ///
    /// # Errors
    ///
    /// [`Errno::OutOfRange`] for any other byte, including the zero a
    /// blank frame carries.
    pub const fn from_code(code: u8) -> Result<Self, Errno> {
        match code {
            APPEARANCE_DARK => Ok(Self::Dark),
            APPEARANCE_LIGHT => Ok(Self::Light),
            _ => Err(Errno::OutOfRange),
        }
    }

    /// Every appearance, in the canonical listing order a chooser offers
    /// them in.
    pub const ALL: [Self; 2] = [Self::Dark, Self::Light];

    /// The canonical settings-document spelling.
    ///
    /// The desktop's settings document carries this same closed set as
    /// text, so the spelling lives with the value rather than beside each
    /// reader of it.
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Dark => "dark",
            Self::Light => "light",
        }
    }

    /// Decode a settings-document spelling; `None` for anything outside
    /// the closed set (case-sensitive — one canonical spelling).
    #[must_use]
    pub fn from_value(value: &str) -> Option<Self> {
        Self::ALL.into_iter().find(|kind| kind.as_str() == value)
    }
}

/// How much separation a theme draws between what a control *is* and what
/// is behind it.
///
/// Like [`Appearance`] this vocabulary lives in the ABI because it crosses
/// the window channel: `lib/theme` re-exports it rather than restating it,
/// so the byte on the wire and the value a theme carries cannot drift
/// apart.
#[derive(Copy, Clone, Debug, Default, Eq, PartialEq, Hash)]
pub enum Contrast {
    /// The theme's own contrast.
    #[default]
    Normal,
    /// Increased rim, rail and text contrast.
    High,
    /// Monochrome-safe: a semantic role must be told apart by shape, never
    /// by hue alone.
    Monochrome,
}

/// Wire code of [`Contrast::Normal`]. Zero is deliberately no contrast, so
/// an all-zero frame can never decode as a desktop.
const CONTRAST_NORMAL: u8 = 1;
/// Wire code of [`Contrast::High`].
const CONTRAST_HIGH: u8 = 2;
/// Wire code of [`Contrast::Monochrome`].
const CONTRAST_MONOCHROME: u8 = 3;

impl Contrast {
    /// This contrast's wire code.
    #[must_use]
    pub const fn code(self) -> u8 {
        match self {
            Self::Normal => CONTRAST_NORMAL,
            Self::High => CONTRAST_HIGH,
            Self::Monochrome => CONTRAST_MONOCHROME,
        }
    }

    /// The contrast `code` names.
    ///
    /// # Errors
    ///
    /// [`Errno::OutOfRange`] for any other byte, including the zero a blank
    /// frame carries.
    pub const fn from_code(code: u8) -> Result<Self, Errno> {
        match code {
            CONTRAST_NORMAL => Ok(Self::Normal),
            CONTRAST_HIGH => Ok(Self::High),
            CONTRAST_MONOCHROME => Ok(Self::Monochrome),
            _ => Err(Errno::OutOfRange),
        }
    }

    /// Every contrast, in the canonical listing order a chooser offers
    /// them in.
    pub const ALL: [Self; 3] = [Self::Normal, Self::High, Self::Monochrome];

    /// The canonical settings-document spelling.
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Normal => "normal",
            Self::High => "high",
            Self::Monochrome => "monochrome",
        }
    }

    /// Decode a settings-document spelling; `None` for anything outside
    /// the closed set.
    #[must_use]
    pub fn from_value(value: &str) -> Option<Self> {
        Self::ALL.into_iter().find(|kind| kind.as_str() == value)
    }
}

/// How much room a theme gives a control: the spacing axis, never the
/// meaning of a state.
#[derive(Copy, Clone, Debug, Default, Eq, PartialEq, Hash)]
pub enum Density {
    /// Tables, task lists, sidebars and dense system panels.
    Compact,
    /// Ordinary desktop applications.
    #[default]
    Normal,
    /// Touch-adjacent or distance-viewed surfaces.
    Comfortable,
}

/// Wire code of [`Density::Compact`]. Zero is deliberately no density.
const DENSITY_COMPACT: u8 = 1;
/// Wire code of [`Density::Normal`].
const DENSITY_NORMAL: u8 = 2;
/// Wire code of [`Density::Comfortable`].
const DENSITY_COMFORTABLE: u8 = 3;

impl Density {
    /// This density's wire code.
    #[must_use]
    pub const fn code(self) -> u8 {
        match self {
            Self::Compact => DENSITY_COMPACT,
            Self::Normal => DENSITY_NORMAL,
            Self::Comfortable => DENSITY_COMFORTABLE,
        }
    }

    /// The density `code` names.
    ///
    /// # Errors
    ///
    /// [`Errno::OutOfRange`] for any other byte, including the zero a blank
    /// frame carries.
    pub const fn from_code(code: u8) -> Result<Self, Errno> {
        match code {
            DENSITY_COMPACT => Ok(Self::Compact),
            DENSITY_NORMAL => Ok(Self::Normal),
            DENSITY_COMFORTABLE => Ok(Self::Comfortable),
            _ => Err(Errno::OutOfRange),
        }
    }

    /// Every density, in the canonical listing order a chooser offers them
    /// in.
    pub const ALL: [Self; 3] = [Self::Compact, Self::Normal, Self::Comfortable];

    /// The canonical settings-document spelling.
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Compact => "compact",
            Self::Normal => "normal",
            Self::Comfortable => "comfortable",
        }
    }

    /// Decode a settings-document spelling; `None` for anything outside
    /// the closed set.
    #[must_use]
    pub fn from_value(value: &str) -> Option<Self> {
        Self::ALL.into_iter().find(|kind| kind.as_str() == value)
    }
}

/// Whether the desktop animates a state change or steps straight to it.
///
/// [`Motion::Reduced`] is not "no feedback": the state still changes
/// visibly, through contrast, rail thickness, shape marks and labels. It is
/// the animation between the two states that goes.
#[derive(Copy, Clone, Debug, Default, Eq, PartialEq, Hash)]
pub enum Motion {
    /// Animated transitions run at the theme's tuned durations.
    #[default]
    Full,
    /// Every animated transition collapses to an immediate state change.
    Reduced,
}

/// Wire code of [`Motion::Full`]. Zero is deliberately no motion policy.
const MOTION_FULL: u8 = 1;
/// Wire code of [`Motion::Reduced`].
const MOTION_REDUCED: u8 = 2;

impl Motion {
    /// This motion policy's wire code.
    #[must_use]
    pub const fn code(self) -> u8 {
        match self {
            Self::Full => MOTION_FULL,
            Self::Reduced => MOTION_REDUCED,
        }
    }

    /// The motion policy `code` names.
    ///
    /// # Errors
    ///
    /// [`Errno::OutOfRange`] for any other byte, including the zero a blank
    /// frame carries.
    pub const fn from_code(code: u8) -> Result<Self, Errno> {
        match code {
            MOTION_FULL => Ok(Self::Full),
            MOTION_REDUCED => Ok(Self::Reduced),
            _ => Err(Errno::OutOfRange),
        }
    }

    /// Whether this policy suppresses animated transitions, which is the
    /// form a theme's motion table asks the question in.
    #[must_use]
    pub const fn is_reduced(self) -> bool {
        matches!(self, Self::Reduced)
    }

    /// The policy a reduced-motion flag names, so a theme and the wire have
    /// one conversion rather than each carrying its own.
    #[must_use]
    pub const fn from_reduced(reduced: bool) -> Self {
        if reduced {
            Self::Reduced
        } else {
            Self::Full
        }
    }

    /// Every motion policy, in the canonical listing order a chooser
    /// offers them in.
    pub const ALL: [Self; 2] = [Self::Full, Self::Reduced];

    /// The canonical settings-document spelling.
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Full => "full",
            Self::Reduced => "reduced",
        }
    }

    /// Decode a settings-document spelling; `None` for anything outside
    /// the closed set.
    #[must_use]
    pub fn from_value(value: &str) -> Option<Self> {
        Self::ALL.into_iter().find(|kind| kind.as_str() == value)
    }
}

/// Longest cursor-set name the desktop offers or stores.
///
/// A fixed format bound, not a capacity: a set's name is the directory it
/// occupies in the shipped store *and* the label a chooser draws, so it is
/// held to a length a reply frame and a settings value can both carry
/// outright. A directory whose name exceeds it is refused by the image
/// build rather than shipped as artwork nothing could offer.
pub const CURSOR_SET_NAME_MAX: usize = 32;

/// Most cursor sets the desktop offers.
///
/// A containment bound on a store listing, like the wallpaper catalog's:
/// the sets fit one reply frame, so a chooser learns the whole choice space
/// in one call and no paging exists to get wrong. A store holding more sets
/// than this offers the first this many in name order.
pub const CURSOR_SETS_MAX: usize = 16;

/// The shortest double-click interval a desktop publishes: faster than a
/// hand can press twice deliberately, so a shorter one would make a
/// double-click impossible rather than quick.
pub const DOUBLE_CLICK_MIN: Duration64 = Duration64::from_millis(100);

/// The longest double-click interval a desktop publishes: past it, two
/// separate clicks on one thing would read as one gesture.
pub const DOUBLE_CLICK_MAX: Duration64 = Duration64::from_millis(2_000);

/// The double-click interval a desktop publishes until its user chooses
/// another.
pub const DOUBLE_CLICK_DEFAULT: Duration64 = Duration64::from_millis(500);

/// The desktop a window is displayed on.
///
/// Opaque and validated on the way in: a desktop with a zero-sized screen
/// or a zero scale cannot be constructed, so no consumer has to defend
/// against one. The scale is carried as a percentage of the reference
/// density — the number `tairix_geometry::Scale` is spelled in — and the
/// bounds of a *usable* scale belong to that type, not to the wire: the
/// window client resolves the percentage into a `Scale` and refuses a
/// value outside its range.
#[derive(Copy, Clone, Debug, Eq, PartialEq, Hash)]
pub struct DesktopInfo {
    screen_width_px: u32,
    screen_height_px: u32,
    scale_percent: u16,
    appearance: Appearance,
    contrast: Contrast,
    density: Density,
    motion: Motion,
    double_click: Duration64,
}

/// Byte offset of the double-click interval in an encoded [`DesktopInfo`].
const DOUBLE_CLICK_OFFSET: usize = 16;

impl DesktopInfo {
    /// Encoded size on the wire: screen width (4), screen height (4),
    /// scale percentage (2), appearance (1), one reserved byte that must be
    /// zero, contrast (1), density (1), motion (1), a second reserved byte
    /// that must be zero, and the double-click interval (12).
    pub const WIRE_LEN: usize = DOUBLE_CLICK_OFFSET + Duration64::WIRE_LEN;

    /// The desktop with a `screen_width_px` × `screen_height_px` screen,
    /// drawn at `scale_percent` of the reference density in `appearance`,
    /// on the theme's own contrast, density and motion, pairing presses under
    /// [`DOUBLE_CLICK_DEFAULT`].
    ///
    /// [`with_axes`](Self::with_axes) derives the same desktop on other
    /// axes, so a caller that only knows the extent and the appearance —
    /// the great majority — says so rather than restating three defaults.
    ///
    /// # Errors
    ///
    /// [`Errno::OutOfRange`] if either screen dimension or the scale is
    /// zero — a screen no pixel fits on, and a scale that collapses every
    /// length to nothing, are refused rather than propagated.
    pub const fn new(
        screen_width_px: u32,
        screen_height_px: u32,
        scale_percent: u16,
        appearance: Appearance,
    ) -> Result<Self, Errno> {
        if screen_width_px == 0 || screen_height_px == 0 || scale_percent == 0 {
            return Err(Errno::OutOfRange);
        }
        Ok(Self {
            screen_width_px,
            screen_height_px,
            scale_percent,
            appearance,
            contrast: Contrast::Normal,
            density: Density::Normal,
            motion: Motion::Full,
            double_click: DOUBLE_CLICK_DEFAULT,
        })
    }

    /// The same desktop pairing presses under `interval`.
    ///
    /// # Errors
    ///
    /// [`Errno::OutOfRange`] for an interval outside
    /// [`DOUBLE_CLICK_MIN`]`..=`[`DOUBLE_CLICK_MAX`].
    pub fn with_double_click(self, interval: Duration64) -> Result<Self, Errno> {
        if interval < DOUBLE_CLICK_MIN || interval > DOUBLE_CLICK_MAX {
            return Err(Errno::OutOfRange);
        }
        Ok(Self {
            double_click: interval,
            ..self
        })
    }

    /// The same desktop drawn on `contrast`, `density` and `motion`.
    #[must_use]
    pub const fn with_axes(self, contrast: Contrast, density: Density, motion: Motion) -> Self {
        Self {
            contrast,
            density,
            motion,
            ..self
        }
    }

    /// The screen's width in physical pixels; never zero.
    #[must_use]
    pub const fn screen_width_px(&self) -> u32 {
        self.screen_width_px
    }

    /// The screen's height in physical pixels; never zero.
    #[must_use]
    pub const fn screen_height_px(&self) -> u32 {
        self.screen_height_px
    }

    /// The desktop UI scale, as a percentage of the reference density;
    /// never zero.
    #[must_use]
    pub const fn scale_percent(&self) -> u16 {
        self.scale_percent
    }

    /// Which way round the active theme's colours run.
    #[must_use]
    pub const fn appearance(&self) -> Appearance {
        self.appearance
    }

    /// How much separation the desktop draws around a control.
    #[must_use]
    pub const fn contrast(&self) -> Contrast {
        self.contrast
    }

    /// How much room the desktop gives a control.
    #[must_use]
    pub const fn density(&self) -> Density {
        self.density
    }

    /// Whether the desktop animates a state change.
    #[must_use]
    pub const fn motion(&self) -> Motion {
        self.motion
    }

    /// The longest two presses on one thing may be apart and still be one
    /// double-click; within [`DOUBLE_CLICK_MIN`]`..=`[`DOUBLE_CLICK_MAX`].
    #[must_use]
    pub const fn double_click(&self) -> Duration64 {
        self.double_click
    }

    /// Encode `self` little-endian.
    #[must_use]
    pub fn to_le_bytes(&self) -> [u8; Self::WIRE_LEN] {
        let mut out = [0u8; Self::WIRE_LEN];
        self.write_to(&mut out);
        out
    }

    /// Write `self` into the `WIRE_LEN` bytes of `out` starting at `at`,
    /// so a larger frame can carry the record inline without a second
    /// encoding of the same fields.
    ///
    /// Writes nothing if `out` is too short for the record at `at`; the
    /// callers in this crate are fixed-width frames sized to hold it.
    pub fn write_to_at(&self, out: &mut [u8], at: usize) {
        let Some(slot) = out.get_mut(at..at + Self::WIRE_LEN) else {
            return;
        };
        let mut record = [0u8; Self::WIRE_LEN];
        self.write_to(&mut record);
        slot.copy_from_slice(&record);
    }

    /// Fill exactly one encoded record.
    fn write_to(&self, out: &mut [u8; Self::WIRE_LEN]) {
        put_u32(out, 0, self.screen_width_px);
        put_u32(out, 4, self.screen_height_px);
        put_u16(out, 8, self.scale_percent);
        out[10] = self.appearance.code();
        out[12] = self.contrast.code();
        out[13] = self.density.code();
        out[14] = self.motion.code();
        out[DOUBLE_CLICK_OFFSET..].copy_from_slice(&self.double_click.to_le_bytes());
    }

    /// Decode the record occupying the `WIRE_LEN` bytes of `bytes` from
    /// `at`.
    ///
    /// # Errors
    ///
    /// * [`Errno::BufferTooSmall`] — `bytes` does not hold a whole record
    ///   at `at`.
    /// * [`Errno::OutOfRange`] — a zero extent, a zero scale, an
    ///   appearance code this version does not define, or a double-click
    ///   interval outside its bounds.
    /// * [`Errno::TimestampOutOfRange`] — a double-click interval whose
    ///   nanosecond field is not canonical.
    /// * [`Errno::BadMagic`] — either reserved byte is not zero (wire
    ///   corruption or a smuggled field, never silently ignored).
    pub fn from_bytes_at(bytes: &[u8], at: usize) -> Result<Self, Errno> {
        let Some(record) = bytes.get(at..at + Self::WIRE_LEN) else {
            return Err(Errno::BufferTooSmall);
        };
        if record[11] != 0 || record[15] != 0 {
            return Err(Errno::BadMagic);
        }
        Self::new(
            read_u32(record, 0),
            read_u32(record, 4),
            read_u16(record, 8),
            Appearance::from_code(record[10])?,
        )?
        .with_axes(
            Contrast::from_code(record[12])?,
            Density::from_code(record[13])?,
            Motion::from_code(record[14])?,
        )
        .with_double_click(Duration64::from_bytes(&record[DOUBLE_CLICK_OFFSET..])?)
    }

    /// Decode a record that occupies the whole of `bytes`.
    ///
    /// # Errors
    ///
    /// As [`from_bytes_at`](Self::from_bytes_at).
    pub fn from_bytes(bytes: &[u8]) -> Result<Self, Errno> {
        Self::from_bytes_at(bytes, 0)
    }
}

#[cfg(test)]
mod tests {
    use super::{
        Appearance, Contrast, Density, DesktopInfo, Motion, DOUBLE_CLICK_DEFAULT, DOUBLE_CLICK_MAX,
        DOUBLE_CLICK_MIN, DOUBLE_CLICK_OFFSET,
    };
    use crate::time::Duration64;
    use crate::Errno;

    /// A desktop for the tests to round-trip.
    fn desktop() -> DesktopInfo {
        match DesktopInfo::new(1920, 1080, 150, Appearance::Light) {
            Ok(info) => info,
            Err(_) => unreachable!("a 1920x1080 screen at 150% is in range"),
        }
    }

    #[test]
    fn a_desktop_round_trips_through_the_wire() {
        let info = desktop();
        assert_eq!(DesktopInfo::from_bytes(&info.to_le_bytes()), Ok(info));
        assert_eq!(info.screen_width_px(), 1920);
        assert_eq!(info.screen_height_px(), 1080);
        assert_eq!(info.scale_percent(), 150);
        assert_eq!(info.appearance(), Appearance::Light);
        assert_eq!(info.contrast(), Contrast::Normal);
        assert_eq!(info.density(), Density::Normal);
        assert_eq!(info.motion(), Motion::Full);
        assert_eq!(info.double_click(), DOUBLE_CLICK_DEFAULT);
    }

    #[test]
    fn the_double_click_interval_round_trips_within_its_bounds() {
        for interval in [
            DOUBLE_CLICK_MIN,
            Duration64::from_millis(750),
            DOUBLE_CLICK_MAX,
        ] {
            let info = desktop().with_double_click(interval).expect("in bounds");
            assert_eq!(info.double_click(), interval);
            assert_eq!(DesktopInfo::from_bytes(&info.to_le_bytes()), Ok(info));
        }
        for outside in [
            Duration64::ZERO,
            Duration64::from_millis(99),
            Duration64::from_millis(2_001),
        ] {
            assert_eq!(desktop().with_double_click(outside), Err(Errno::OutOfRange));
        }
    }

    #[test]
    fn a_double_click_interval_outside_its_bounds_is_refused_on_decode() {
        let mut bytes = desktop().to_le_bytes();
        bytes[DOUBLE_CLICK_OFFSET..].copy_from_slice(&Duration64::ZERO.to_le_bytes());
        assert_eq!(DesktopInfo::from_bytes(&bytes), Err(Errno::OutOfRange));
        let mut uncanonical = desktop().to_le_bytes();
        uncanonical[DOUBLE_CLICK_OFFSET + 8..].copy_from_slice(&u32::MAX.to_le_bytes());
        assert_eq!(
            DesktopInfo::from_bytes(&uncanonical),
            Err(Errno::TimestampOutOfRange)
        );
    }

    #[test]
    fn the_accessibility_axes_round_trip_through_the_wire() {
        let info = desktop().with_axes(Contrast::Monochrome, Density::Compact, Motion::Reduced);
        let back = DesktopInfo::from_bytes(&info.to_le_bytes());
        assert_eq!(back, Ok(info));
        assert_eq!(info.contrast(), Contrast::Monochrome);
        assert_eq!(info.density(), Density::Compact);
        assert_eq!(info.motion(), Motion::Reduced);
        // The axes are the only thing that moved: a desktop that differs
        // only in how it is drawn still describes the same screen.
        assert_eq!(info.screen_width_px(), desktop().screen_width_px());
        assert_eq!(info.appearance(), desktop().appearance());
    }

    #[test]
    fn a_record_round_trips_inside_a_larger_frame() {
        let info = desktop();
        let mut frame = [0xAAu8; DesktopInfo::WIRE_LEN + 8];
        info.write_to_at(&mut frame, 4);
        assert_eq!(DesktopInfo::from_bytes_at(&frame, 4), Ok(info));
        // The record wrote only its own bytes: the surrounding frame is
        // untouched, so a caller's other fields cannot be clobbered.
        assert_eq!(&frame[..4], &[0xAA; 4]);
        assert_eq!(&frame[4 + DesktopInfo::WIRE_LEN..], &[0xAA; 4]);
    }

    #[test]
    fn write_to_at_writes_nothing_when_the_frame_is_too_short() {
        let mut frame = [0xAAu8; DesktopInfo::WIRE_LEN];
        desktop().write_to_at(&mut frame, 1);
        assert_eq!(frame, [0xAA; DesktopInfo::WIRE_LEN]);
    }

    #[test]
    fn an_impossible_desktop_cannot_be_constructed() {
        assert_eq!(
            DesktopInfo::new(0, 1080, 100, Appearance::Dark),
            Err(Errno::OutOfRange)
        );
        assert_eq!(
            DesktopInfo::new(1920, 0, 100, Appearance::Dark),
            Err(Errno::OutOfRange)
        );
        assert_eq!(
            DesktopInfo::new(1920, 1080, 0, Appearance::Dark),
            Err(Errno::OutOfRange)
        );
    }

    #[test]
    fn decoding_fails_closed() {
        let good = desktop().to_le_bytes();
        assert_eq!(
            DesktopInfo::from_bytes(&good[..DesktopInfo::WIRE_LEN - 1]),
            Err(Errno::BufferTooSmall)
        );

        // A blank frame is not a desktop: zero is no appearance.
        assert_eq!(
            DesktopInfo::from_bytes(&[0u8; DesktopInfo::WIRE_LEN]),
            Err(Errno::OutOfRange)
        );

        let mut reserved = good;
        reserved[11] = 1;
        assert_eq!(DesktopInfo::from_bytes(&reserved), Err(Errno::BadMagic));

        let mut reserved_tail = good;
        reserved_tail[15] = 1;
        assert_eq!(
            DesktopInfo::from_bytes(&reserved_tail),
            Err(Errno::BadMagic)
        );

        let mut appearance = good;
        appearance[10] = 3;
        assert_eq!(DesktopInfo::from_bytes(&appearance), Err(Errno::OutOfRange));

        // Each axis is decoded, so an unknown one is refused rather than
        // drawn as the default the user did not ask for.
        for slot in [12usize, 13, 14] {
            let mut axis = good;
            axis[slot] = 9;
            assert_eq!(DesktopInfo::from_bytes(&axis), Err(Errno::OutOfRange));
        }

        let mut width = good;
        width[..4].copy_from_slice(&0u32.to_le_bytes());
        assert_eq!(DesktopInfo::from_bytes(&width), Err(Errno::OutOfRange));

        let mut scale = good;
        scale[8..10].copy_from_slice(&0u16.to_le_bytes());
        assert_eq!(DesktopInfo::from_bytes(&scale), Err(Errno::OutOfRange));
    }

    #[test]
    fn every_appearance_code_round_trips_and_zero_is_none() {
        for appearance in [Appearance::Dark, Appearance::Light] {
            assert_eq!(Appearance::from_code(appearance.code()), Ok(appearance));
        }
        assert_eq!(Appearance::from_code(0), Err(Errno::OutOfRange));
        assert_ne!(Appearance::Dark.code(), Appearance::Light.code());
    }

    #[test]
    fn every_axis_code_round_trips_and_zero_is_none() {
        for contrast in [Contrast::Normal, Contrast::High, Contrast::Monochrome] {
            assert_eq!(Contrast::from_code(contrast.code()), Ok(contrast));
        }
        for density in [Density::Compact, Density::Normal, Density::Comfortable] {
            assert_eq!(Density::from_code(density.code()), Ok(density));
        }
        for motion in [Motion::Full, Motion::Reduced] {
            assert_eq!(Motion::from_code(motion.code()), Ok(motion));
            assert_eq!(Motion::from_reduced(motion.is_reduced()), motion);
        }
        assert_eq!(Contrast::from_code(0), Err(Errno::OutOfRange));
        assert_eq!(Density::from_code(0), Err(Errno::OutOfRange));
        assert_eq!(Motion::from_code(0), Err(Errno::OutOfRange));
    }

    #[test]
    fn every_axis_spelling_round_trips_and_is_distinct() {
        // One spelling per value, and no two values sharing one, or a
        // settings document could name a desktop that is two things at
        // once.
        for appearance in Appearance::ALL {
            assert_eq!(
                Appearance::from_value(appearance.as_str()),
                Some(appearance)
            );
        }
        for contrast in Contrast::ALL {
            assert_eq!(Contrast::from_value(contrast.as_str()), Some(contrast));
        }
        for density in Density::ALL {
            assert_eq!(Density::from_value(density.as_str()), Some(density));
        }
        for motion in Motion::ALL {
            assert_eq!(Motion::from_value(motion.as_str()), Some(motion));
        }
        assert_eq!(Appearance::from_value("Dark"), None);
        assert_eq!(Contrast::from_value(""), None);
        assert_eq!(Density::from_value("dense"), None);
        assert_eq!(Motion::from_value("none"), None);
        // `normal` is a value of two different axes and must stay each
        // axis's own: a document key decides which set is read.
        assert_eq!(Contrast::from_value("normal"), Some(Contrast::Normal));
        assert_eq!(Density::from_value("normal"), Some(Density::Normal));
    }
}
