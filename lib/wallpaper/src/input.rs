//! The input policy the desktop document carries: which button is primary,
//! how fast the pointer moves, how far apart a double-click's presses may be,
//! and how a held key repeats.
//!
//! Every span is a [`Duration64`] in memory; the document a person edits
//! spells it in whole milliseconds.

use alloc::format;
use alloc::string::{String, ToString};

use tairix_abi::time::Duration64;

/// Which physical button acts as the primary one.
#[derive(Copy, Clone, Debug, Default, Eq, PartialEq)]
pub enum PrimaryButton {
    /// The left button activates, the right opens a context menu.
    #[default]
    Left,
    /// The two are swapped, for a mouse used in the left hand.
    Right,
}

impl PrimaryButton {
    /// Both, in the order a chooser offers them.
    pub const ALL: [Self; 2] = [Self::Left, Self::Right];

    /// The canonical value spelling.
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Left => "left",
            Self::Right => "right",
        }
    }

    /// Decode a value spelling; `None` for anything outside the closed set.
    #[must_use]
    pub fn from_value(value: &str) -> Option<Self> {
        Self::ALL
            .into_iter()
            .find(|button| button.as_str() == value)
    }
}

/// How far the pointer moves for a movement of the mouse, as a percentage of
/// the distance the mouse reports.
#[derive(Copy, Clone, Debug, Eq, PartialEq)]
pub struct PointerSpeed(u16);

impl PointerSpeed {
    /// The slowest: a quarter of the reported distance.
    pub const MIN: Self = Self(25);
    /// The fastest: four times it.
    pub const MAX: Self = Self(400);
    /// The distance exactly as reported.
    pub const NORMAL: Self = Self(100);

    /// The speed `percent` names, or `None` outside [`Self::MIN`]`..=`[`Self::MAX`].
    #[must_use]
    pub const fn from_percent(percent: u16) -> Option<Self> {
        if percent < Self::MIN.0 || percent > Self::MAX.0 {
            None
        } else {
            Some(Self(percent))
        }
    }

    /// This speed as a percentage.
    #[must_use]
    pub const fn percent(self) -> u16 {
        self.0
    }
}

impl Default for PointerSpeed {
    fn default() -> Self {
        Self::NORMAL
    }
}

/// How often a held key repeats once it has been held for the repeat delay.
#[derive(Copy, Clone, Debug, Eq, PartialEq)]
pub enum RepeatRate {
    /// A held key does not repeat.
    Off,
    /// A held key repeats this many times a second, within
    /// `1..=`[`Self::MAX_PER_SECOND`].
    PerSecond(u8),
}

impl RepeatRate {
    /// The fastest repeat offered.
    pub const MAX_PER_SECOND: u8 = 60;

    const OFF: &'static str = "off";

    /// The span between two repeats, or `None` when keys do not repeat.
    #[must_use]
    pub fn interval(self) -> Option<Duration64> {
        match self {
            Self::Off => None,
            Self::PerSecond(rate) => Some(Duration64::from_nanos(
                1_000_000_000 / u64::from(rate.max(1)),
            )),
        }
    }

    pub(crate) fn from_value(value: &str) -> Option<Self> {
        if value == Self::OFF {
            return Some(Self::Off);
        }
        let rate = u8::try_from(parse_decimal(value)?).ok()?;
        (1..=Self::MAX_PER_SECOND)
            .contains(&rate)
            .then_some(Self::PerSecond(rate))
    }

    pub(crate) fn render_value(self) -> String {
        match self {
            Self::Off => Self::OFF.to_string(),
            Self::PerSecond(rate) => format!("{rate}"),
        }
    }
}

impl Default for RepeatRate {
    fn default() -> Self {
        Self::PerSecond(30)
    }
}

/// The shortest a key is held before it repeats.
pub const REPEAT_DELAY_MIN: Duration64 = Duration64::from_millis(100);

/// The longest a key is held before it repeats.
pub const REPEAT_DELAY_MAX: Duration64 = Duration64::from_millis(2_000);

/// How long a key is held before it repeats until its user chooses another.
pub const REPEAT_DELAY_DEFAULT: Duration64 = Duration64::from_millis(500);

/// Decode a bare decimal: ASCII digits only, because a signed, spaced or
/// radix-prefixed number is a second spelling of the same value.
pub(crate) fn parse_decimal(value: &str) -> Option<u32> {
    if value.is_empty() || !value.bytes().all(|byte| byte.is_ascii_digit()) {
        return None;
    }
    value.bytes().try_fold(0u32, |total, byte| {
        total.checked_mul(10)?.checked_add(u32::from(byte - b'0'))
    })
}

/// Decode a span spelled in whole milliseconds, within `min..=max`.
pub(crate) fn parse_millis(value: &str, min: Duration64, max: Duration64) -> Option<Duration64> {
    let span = Duration64::from_millis(parse_decimal(value)?);
    (min..=max).contains(&span).then_some(span)
}

/// A span's spelling in whole milliseconds.
pub(crate) fn render_millis(span: Duration64) -> String {
    format!("{}", span.saturating_total_nanos() / 1_000_000)
}

#[cfg(test)]
mod tests {
    use tairix_abi::desktop::{DOUBLE_CLICK_MAX, DOUBLE_CLICK_MIN};
    use tairix_abi::time::Duration64;

    use super::{
        parse_decimal, parse_millis, render_millis, PointerSpeed, PrimaryButton, RepeatRate,
        REPEAT_DELAY_MAX, REPEAT_DELAY_MIN,
    };

    fn parse_double_click(value: &str) -> Option<Duration64> {
        parse_millis(value, DOUBLE_CLICK_MIN, DOUBLE_CLICK_MAX)
    }

    fn parse_repeat_delay(value: &str) -> Option<Duration64> {
        parse_millis(value, REPEAT_DELAY_MIN, REPEAT_DELAY_MAX)
    }

    fn parse_speed(value: &str) -> Option<PointerSpeed> {
        PointerSpeed::from_percent(u16::try_from(parse_decimal(value)?).ok()?)
    }

    #[test]
    fn a_decimal_has_one_spelling() {
        assert_eq!(parse_decimal("500"), Some(500));
        for other in ["", "+500", "-5", " 500", "5e2", "0x1f4", "4294967296"] {
            assert_eq!(parse_decimal(other), None, "{other:?}");
        }
    }

    #[test]
    fn a_millisecond_span_round_trips_within_its_bounds() {
        for ms in ["100", "500", "2000"] {
            let span = parse_double_click(ms).expect("in bounds");
            assert_eq!(render_millis(span), ms);
        }
        assert_eq!(parse_double_click("99"), None);
        assert_eq!(parse_double_click("2001"), None);
        assert_eq!(parse_double_click("100"), Some(DOUBLE_CLICK_MIN));
        assert_eq!(parse_double_click("2000"), Some(DOUBLE_CLICK_MAX));
        assert_eq!(parse_repeat_delay("100"), Some(REPEAT_DELAY_MIN));
        assert_eq!(parse_repeat_delay("2000"), Some(REPEAT_DELAY_MAX));
        assert_eq!(parse_repeat_delay("50"), None);
    }

    #[test]
    fn the_pointer_speed_is_bounded() {
        assert_eq!(parse_speed("100"), Some(PointerSpeed::NORMAL));
        assert_eq!(parse_speed("25"), Some(PointerSpeed::MIN));
        assert_eq!(parse_speed("400"), Some(PointerSpeed::MAX));
        assert_eq!(parse_speed("24"), None);
        assert_eq!(parse_speed("401"), None);
        assert_eq!(parse_speed("70000"), None);
    }

    #[test]
    fn the_repeat_rate_is_off_or_a_bounded_rate() {
        assert_eq!(RepeatRate::from_value("off"), Some(RepeatRate::Off));
        assert_eq!(
            RepeatRate::from_value("30"),
            Some(RepeatRate::PerSecond(30))
        );
        for bad in ["0", "61", "Off", "fast", "-1"] {
            assert_eq!(RepeatRate::from_value(bad), None, "{bad:?}");
        }
        for rate in [
            RepeatRate::Off,
            RepeatRate::PerSecond(1),
            RepeatRate::PerSecond(60),
        ] {
            assert_eq!(RepeatRate::from_value(&rate.render_value()), Some(rate));
        }
    }

    #[test]
    fn a_repeat_rate_is_the_span_between_repeats() {
        assert_eq!(RepeatRate::Off.interval(), None);
        assert_eq!(
            RepeatRate::PerSecond(20).interval(),
            Some(Duration64::from_millis(50))
        );
    }

    #[test]
    fn the_primary_button_has_one_spelling_each() {
        for button in PrimaryButton::ALL {
            assert_eq!(PrimaryButton::from_value(button.as_str()), Some(button));
        }
        assert_eq!(PrimaryButton::from_value("Left"), None);
    }
}
