//! The idle policy the desktop document carries: when the screensaver starts,
//! what it shows, and when the screen locks.
//!
//! Both deadlines count from the last input. The document spells each in whole
//! minutes, or `never`.

use alloc::format;
use alloc::string::{String, ToString};

use tairix_abi::time::Duration64;

use crate::input::parse_decimal;

/// What the screensaver draws over the desktop.
#[derive(Copy, Clone, Debug, Default, Eq, PartialEq)]
pub enum ScreensaverKind {
    /// A black screen.
    #[default]
    Blank,
    /// The desktop's own backdrop, dimmed, with no window on it.
    Dim,
    /// The shipped pictures, one after another.
    Slideshow,
}

impl ScreensaverKind {
    /// Every kind, in the order a chooser offers them.
    pub const ALL: [Self; 3] = [Self::Blank, Self::Dim, Self::Slideshow];

    /// The canonical value spelling.
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Blank => "blank",
            Self::Dim => "dim",
            Self::Slideshow => "slideshow",
        }
    }

    /// Decode a value spelling; `None` for anything outside the closed set.
    #[must_use]
    pub fn from_value(value: &str) -> Option<Self> {
        Self::ALL.into_iter().find(|kind| kind.as_str() == value)
    }
}

/// How long the desktop may sit idle before an idle action, or never.
#[derive(Copy, Clone, Debug, Eq, PartialEq)]
pub enum IdleAfter {
    /// The action never happens on its own.
    Never,
    /// After this many whole minutes without input, within
    /// `1..=`[`Self::MAX_MINUTES`].
    Minutes(u16),
}

impl IdleAfter {
    /// The longest idle wait a document may name: a day.
    pub const MAX_MINUTES: u16 = 24 * 60;

    const NEVER: &'static str = "never";

    /// The wait as a span, or `None` for [`Self::Never`].
    #[must_use]
    pub fn span(self) -> Option<Duration64> {
        match self {
            Self::Never => None,
            Self::Minutes(minutes) => Some(Duration64::from_secs(i64::from(minutes) * 60)),
        }
    }

    /// Decode a value spelling: `never`, or a whole number of minutes.
    #[must_use]
    pub fn from_value(value: &str) -> Option<Self> {
        if value == Self::NEVER {
            return Some(Self::Never);
        }
        let minutes = u16::try_from(parse_decimal(value)?).ok()?;
        (1..=Self::MAX_MINUTES)
            .contains(&minutes)
            .then_some(Self::Minutes(minutes))
    }

    /// The canonical value spelling.
    #[must_use]
    pub fn render_value(self) -> String {
        match self {
            Self::Never => Self::NEVER.to_string(),
            Self::Minutes(minutes) => format!("{minutes}"),
        }
    }
}

#[cfg(test)]
mod tests {
    use tairix_abi::time::Duration64;

    use super::{IdleAfter, ScreensaverKind};

    #[test]
    fn an_idle_wait_is_never_or_a_bounded_number_of_minutes() {
        assert_eq!(IdleAfter::from_value("never"), Some(IdleAfter::Never));
        assert_eq!(IdleAfter::from_value("5"), Some(IdleAfter::Minutes(5)));
        assert_eq!(
            IdleAfter::from_value("1440"),
            Some(IdleAfter::Minutes(1440))
        );
        for bad in ["0", "1441", "Never", "5m", "-5", ""] {
            assert_eq!(IdleAfter::from_value(bad), None, "{bad:?}");
        }
        for wait in [
            IdleAfter::Never,
            IdleAfter::Minutes(1),
            IdleAfter::Minutes(90),
        ] {
            assert_eq!(IdleAfter::from_value(&wait.render_value()), Some(wait));
        }
    }

    #[test]
    fn an_idle_wait_is_its_span_in_whole_minutes() {
        assert_eq!(IdleAfter::Never.span(), None);
        assert_eq!(
            IdleAfter::Minutes(5).span(),
            Some(Duration64::from_secs(300))
        );
    }

    #[test]
    fn every_screensaver_has_one_spelling() {
        for kind in ScreensaverKind::ALL {
            assert_eq!(ScreensaverKind::from_value(kind.as_str()), Some(kind));
        }
        assert_eq!(ScreensaverKind::from_value("Blank"), None);
    }
}
