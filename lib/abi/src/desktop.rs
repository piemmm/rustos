//! The desktop a window is displayed on, as its session reports it: the
//! screen extent, the UI scale, and which way round the theme's colours
//! run.
//!
//! These are the facts an application needs before it can lay itself out
//! honestly — how large the screen it will be shown on is, how many
//! physical pixels a logical one is worth, and whether to paint light-on-
//! dark or dark-on-light. All three belong to the seat's desktop, all
//! three are known to the session that composites it, and none of them
//! describes another principal's data or authorises an action: they are
//! descriptive, seat-scoped, and delivered over the window channel the
//! application already holds ([`crate::window_ipc`]), so learning them
//! needs no capability and opens no new endpoint.
//!
//! The record travels in two places, from one definition: an application
//! *asks* for it with `QueryDesktop` — before it opens a window, so its
//! very first frame is the right size, at the right density, in the right
//! colours — and the session *pushes* a `DesktopChanged` event to each of
//! that application's windows whenever it changes any of it. A client that
//! ignores the event simply keeps drawing at the state it asked for.
//!
//! Every decode fails closed: a zero extent, a zero scale, an unknown
//! appearance code, or a dirty reserved byte is refused rather than
//! guessed at.

use crate::le::{put_u16, put_u32, read_u16, read_u32};
use crate::Errno;

/// Which way round a theme's colours run.
///
/// The vocabulary lives here, in the ABI, because it crosses the window
/// channel: `lib/theme` re-exports this very type rather than restating
/// it, so the byte on the wire and the value a theme carries can never
/// drift apart.
#[derive(Copy, Clone, Debug, Eq, PartialEq, Hash)]
pub enum Appearance {
    /// Light foreground on dark surfaces.
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
}

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
}

impl DesktopInfo {
    /// Encoded size on the wire: screen width (4), screen height (4),
    /// scale percentage (2), appearance (1), and one reserved byte that
    /// must be zero.
    pub const WIRE_LEN: usize = 12;

    /// The desktop with a `screen_width_px` × `screen_height_px` screen,
    /// drawn at `scale_percent` of the reference density in `appearance`.
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
        })
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
    }

    /// Decode the record occupying the `WIRE_LEN` bytes of `bytes` from
    /// `at`.
    ///
    /// # Errors
    ///
    /// * [`Errno::BufferTooSmall`] — `bytes` does not hold a whole record
    ///   at `at`.
    /// * [`Errno::OutOfRange`] — a zero extent, a zero scale, or an
    ///   appearance code this version does not define.
    /// * [`Errno::BadMagic`] — the reserved byte is not zero (wire
    ///   corruption or a smuggled field, never silently ignored).
    pub fn from_bytes_at(bytes: &[u8], at: usize) -> Result<Self, Errno> {
        let Some(record) = bytes.get(at..at + Self::WIRE_LEN) else {
            return Err(Errno::BufferTooSmall);
        };
        if record[11] != 0 {
            return Err(Errno::BadMagic);
        }
        Self::new(
            read_u32(record, 0),
            read_u32(record, 4),
            read_u16(record, 8),
            Appearance::from_code(record[10])?,
        )
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
    use super::{Appearance, DesktopInfo};
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

        let mut appearance = good;
        appearance[10] = 3;
        assert_eq!(DesktopInfo::from_bytes(&appearance), Err(Errno::OutOfRange));

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
}
