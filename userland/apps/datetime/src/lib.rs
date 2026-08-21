//! The Date & Time application's host-tested engine: six editable civil
//! fields, their validation, the instant they compose, and the one status
//! line every surface states a result through.
//!
//! # No calendar arithmetic of its own
//!
//! Seeding the fields decomposes an instant with [`CivilTime::from_time64`]
//! and committing composes one with [`days_from_civil`] — the exact inverse,
//! from the same shared calendar the desktop clock and `ls`'s date column
//! read. There is no second day-counting rule here to drift from theirs, and
//! no leap-year table of this app's own.
//!
//! # An unset clock shows nothing, never `1970-01-01`
//!
//! A machine whose wall time has never been established this boot reports
//! [`WallTimeState::Unset`](tairix_abi::time::WallTimeState::Unset), whose
//! instant is the Unix-epoch placeholder and means nothing. The fields are
//! then left **empty**: the epoch presented as a reading would be a
//! fabricated date, and a user asked to correct it would be correcting a
//! fiction. Empty fields still compose nothing until the user types a whole
//! date, so an unset clock cannot be committed by accident.
//!
//! # Every field is checked, and a refusal is stated
//!
//! Committing validates all six fields and refuses the whole edit on the
//! first fault, naming it — never clamping, wrapping, or saturating a value
//! into range, which would set a time the user did not ask for. Day-of-month
//! is checked against the month *and* the year, so 29 February is accepted
//! exactly in leap years. Years before 1970 and far past 2038 are ordinary
//! inputs: the instant is a 64-bit `Time64`, so neither is a boundary here.

#![no_std]
#![forbid(unsafe_code)]
#![deny(missing_docs)]

extern crate alloc;

pub mod view;

use alloc::string::String;
use core::fmt::Write as _;

use tairix_abi::time::{Time64, WallClockReading};
use tairix_fsmeta::calendar::{days_from_civil, CivilTime};

/// Seconds in one minute.
const SECS_PER_MIN: i64 = 60;
/// Seconds in one hour.
const SECS_PER_HOUR: i64 = 60 * SECS_PER_MIN;
/// Seconds in one day.
const SECS_PER_DAY: i64 = 24 * SECS_PER_HOUR;

/// Widest year the app accepts in either direction.
///
/// A bound on *typing*, not on `Time64`: the ABI instant is 64-bit and the
/// calendar is proleptic, so this only keeps a mistyped run of digits from
/// composing an instant no clock could mean. Wide enough that no real date a
/// user wants is out of reach.
pub const YEAR_MAX: i64 = 9999;

/// Narrowest year the app accepts. Negative years are representable in the
/// calendar and are not a boundary of the timestamp.
pub const YEAR_MIN: i64 = -9999;

/// Longest run of characters any one field holds. A year is the longest
/// legitimate entry (a sign and four digits); anything longer is a mistype.
pub const FIELD_MAX: usize = 5;

/// Which civil field an edit or a fault refers to.
///
/// [`Field::ALL`] is the single definition of the fields and their order;
/// the surface's rows, the tab order, and every loop over the six read it
/// rather than restating a list.
#[derive(Copy, Clone, Debug, Default, Eq, PartialEq)]
pub enum Field {
    /// Proleptic-Gregorian year.
    #[default]
    Year,
    /// Month of the year, `1..=12`.
    Month,
    /// Day of the month, valid for the entered month and year.
    Day,
    /// Hour of the day, `0..=23`.
    Hour,
    /// Minute of the hour, `0..=59`.
    Minute,
    /// Second of the minute, `0..=59`.
    Second,
}

impl Field {
    /// The six fields, in the order they are presented and tabbed through.
    pub const ALL: [Self; 6] = [
        Self::Year,
        Self::Month,
        Self::Day,
        Self::Hour,
        Self::Minute,
        Self::Second,
    ];

    /// The field's label.
    #[must_use]
    pub const fn label(self) -> &'static str {
        match self {
            Self::Year => "Year",
            Self::Month => "Month",
            Self::Day => "Day",
            Self::Hour => "Hour",
            Self::Minute => "Minute",
            Self::Second => "Second",
        }
    }

    /// The field's index within [`Field::ALL`].
    #[must_use]
    pub const fn index(self) -> usize {
        match self {
            Self::Year => 0,
            Self::Month => 1,
            Self::Day => 2,
            Self::Hour => 3,
            Self::Minute => 4,
            Self::Second => 5,
        }
    }

    /// The field at `index` within [`Field::ALL`], or `None` past the end —
    /// an index that names no field never resolves to the nearest one.
    #[must_use]
    pub fn at(index: usize) -> Option<Self> {
        Self::ALL.get(index).copied()
    }

    /// The next field in tab order, wrapping.
    #[must_use]
    pub fn next(self) -> Self {
        Self::at((self.index() + 1) % Self::ALL.len()).unwrap_or(Self::Year)
    }
}

/// Why an edit cannot be committed.
///
/// Every variant names the offending field, so the surface states *which*
/// entry is wrong rather than that something is.
#[derive(Copy, Clone, Debug, Eq, PartialEq)]
pub enum Fault {
    /// The field is empty, or holds no number at all.
    Missing(Field),
    /// The field holds something that is not a number in the accepted
    /// spelling (digits, with an optional leading sign on the year).
    NotANumber(Field),
    /// The field holds a number outside the field's own range.
    OutOfRange(Field),
    /// The day is a real number but does not exist in the entered month and
    /// year — 31 April, or 29 February outside a leap year.
    NoSuchDay,
}

impl Fault {
    /// The field the fault is about. [`Fault::NoSuchDay`] is the day's.
    #[must_use]
    pub const fn field(self) -> Field {
        match self {
            Self::Missing(field) | Self::NotANumber(field) | Self::OutOfRange(field) => field,
            Self::NoSuchDay => Field::Day,
        }
    }

    /// The terse sentence a surface states.
    #[must_use]
    pub const fn message(self) -> &'static str {
        match self {
            Self::Missing(field) => match field {
                Field::Year => "Enter a year.",
                Field::Month => "Enter a month.",
                Field::Day => "Enter a day.",
                Field::Hour => "Enter an hour.",
                Field::Minute => "Enter a minute.",
                Field::Second => "Enter a second.",
            },
            Self::NotANumber(field) => match field {
                Field::Year => "The year must be a number.",
                Field::Month => "The month must be a number.",
                Field::Day => "The day must be a number.",
                Field::Hour => "The hour must be a number.",
                Field::Minute => "The minute must be a number.",
                Field::Second => "The second must be a number.",
            },
            Self::OutOfRange(field) => match field {
                Field::Year => "That year is out of range.",
                Field::Month => "The month must be 1 to 12.",
                Field::Day => "The day must be 1 to 31.",
                Field::Hour => "The hour must be 0 to 23.",
                Field::Minute => "The minute must be 0 to 59.",
                Field::Second => "The second must be 0 to 59.",
            },
            Self::NoSuchDay => "That day does not exist in that month.",
        }
    }
}

/// What the app is telling the user about its last action.
#[derive(Clone, Debug, Default, Eq, PartialEq)]
pub enum Status {
    /// Nothing has been attempted since the fields were last seeded.
    #[default]
    Idle,
    /// The clock reports no wall time this boot, so the fields are empty and
    /// there is nothing to correct until one is entered.
    Unset,
    /// The edit was refused before anything was set.
    Rejected(Fault),
    /// The clock was stepped to the composed instant.
    Applied,
    /// The kernel refused the set. The account running this app does not hold
    /// the capability the request needs, so nothing was changed.
    Denied,
    /// The kernel refused the set for some other reason, stated verbatim
    /// rather than guessed at.
    Failed(&'static str),
}

impl Status {
    /// The terse sentence a surface states, or `None` when there is nothing
    /// to say.
    #[must_use]
    pub const fn message(&self) -> Option<&'static str> {
        match *self {
            Self::Idle => None,
            Self::Unset => Some("The clock has not been set on this machine."),
            Self::Rejected(fault) => Some(fault.message()),
            Self::Applied => Some("The clock was set."),
            Self::Denied => Some("This account may not set the clock. Nothing was changed."),
            Self::Failed(reason) => Some(reason),
        }
    }

    /// Whether the status reports a failure, so a surface can colour it.
    #[must_use]
    pub const fn is_fault(&self) -> bool {
        matches!(
            *self,
            Self::Rejected(_) | Self::Denied | Self::Failed(_) | Self::Unset
        )
    }
}

/// The six editable fields and the status line beneath them.
#[derive(Clone, Debug, Default, Eq, PartialEq)]
pub struct Editor {
    year: String,
    month: String,
    day: String,
    hour: String,
    minute: String,
    second: String,
    status: Status,
    focus: Field,
}

impl Editor {
    /// An editor with every field empty and nothing stated.
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// Seed the fields from `reading`.
    ///
    /// A set reading fills all six from the shared calendar decomposition. An
    /// unset one clears them and says so: the epoch placeholder is not a
    /// reading, and presenting it as one would invite the user to "correct" a
    /// date the machine never claimed.
    pub fn seed(&mut self, reading: WallClockReading) {
        if !reading.state().is_set() {
            self.clear();
            self.status = Status::Unset;
            return;
        }
        let civil = CivilTime::from_time64(reading.time());
        self.year = spell(civil.year);
        self.month = spell(i64::from(civil.month));
        self.day = spell(i64::from(civil.day));
        self.hour = spell(i64::from(civil.hour));
        self.minute = spell(i64::from(civil.minute));
        self.second = spell(i64::from(civil.second));
        self.status = Status::Idle;
    }

    /// The text of one field.
    #[must_use]
    pub fn text(&self, field: Field) -> &str {
        match field {
            Field::Year => &self.year,
            Field::Month => &self.month,
            Field::Day => &self.day,
            Field::Hour => &self.hour,
            Field::Minute => &self.minute,
            Field::Second => &self.second,
        }
    }

    /// Replace one field's text, bounded by [`FIELD_MAX`] and accepting only
    /// the spellings a civil field can have: digits, and a leading `-` on the
    /// year alone. Anything else is dropped rather than stored to be rejected
    /// later, so a field can never hold a character the commit would fault
    /// on.
    pub fn set_text(&mut self, field: Field, text: &str) {
        let mut kept = String::new();
        for (at, ch) in text.chars().enumerate() {
            if kept.chars().count() >= FIELD_MAX {
                break;
            }
            let allowed = ch.is_ascii_digit() || (at == 0 && ch == '-' && field == Field::Year);
            if allowed {
                kept.push(ch);
            }
        }
        *self.field_mut(field) = kept;
    }

    /// Append one typed character to `field`, honouring the same spelling and
    /// bound as [`set_text`](Self::set_text).
    pub fn push(&mut self, field: Field, ch: char) {
        let current = self.text(field);
        let allowed =
            ch.is_ascii_digit() || (current.is_empty() && ch == '-' && field == Field::Year);
        if !allowed || current.chars().count() >= FIELD_MAX {
            return;
        }
        self.field_mut(field).push(ch);
    }

    /// Remove the last character of `field`, if it has one.
    pub fn backspace(&mut self, field: Field) {
        let _ = self.field_mut(field).pop();
    }

    /// Empty every field.
    pub fn clear(&mut self) {
        for field in Field::ALL {
            self.field_mut(field).clear();
        }
    }

    /// The field holding the keyboard.
    #[must_use]
    pub const fn focus(&self) -> Field {
        self.focus
    }

    /// Give the keyboard to `field`.
    pub fn set_focus(&mut self, field: Field) {
        self.focus = field;
    }

    /// Move the keyboard to the next field in tab order.
    pub fn focus_next(&mut self) {
        self.focus = self.focus.next();
    }

    /// What the app is currently stating.
    #[must_use]
    pub const fn status(&self) -> &Status {
        &self.status
    }

    /// Adopt `status`, so the surface states the outcome of a set the caller
    /// performed.
    pub fn set_status(&mut self, status: Status) {
        self.status = status;
    }

    /// Validate all six fields and compose the instant they name.
    ///
    /// The composed instant has zero sub-second part: the fields carry
    /// whole seconds, so inventing nanoseconds would claim a precision the
    /// user never entered.
    ///
    /// # Errors
    ///
    /// The first [`Fault`] found, in field order, with nothing composed and
    /// nothing set. The whole edit is refused, never partially applied.
    pub fn compose(&self) -> Result<Time64, Fault> {
        let year = self.number(Field::Year)?;
        let month = self.number(Field::Month)?;
        let day = self.number(Field::Day)?;
        let hour = self.number(Field::Hour)?;
        let minute = self.number(Field::Minute)?;
        let second = self.number(Field::Second)?;

        if !(YEAR_MIN..=YEAR_MAX).contains(&year) {
            return Err(Fault::OutOfRange(Field::Year));
        }
        let month = in_range(month, 1, 12, Field::Month)?;
        let day = in_range(day, 1, 31, Field::Day)?;
        let hour = in_range(hour, 0, 23, Field::Hour)?;
        let minute = in_range(minute, 0, 59, Field::Minute)?;
        let second = in_range(second, 0, 59, Field::Second)?;
        if day > days_in_month(year, month) {
            return Err(Fault::NoSuchDay);
        }

        let days = days_from_civil(year, month, day);
        let secs = days
            .checked_mul(SECS_PER_DAY)
            .and_then(|d| d.checked_add(i64::from(hour) * SECS_PER_HOUR))
            .and_then(|s| s.checked_add(i64::from(minute) * SECS_PER_MIN))
            .and_then(|s| s.checked_add(i64::from(second)))
            .ok_or(Fault::OutOfRange(Field::Year))?;
        // Whole seconds only, so the sub-second part is always in range and
        // the construction cannot refuse a value the fields produced.
        Time64::new(secs, 0).map_err(|_| Fault::OutOfRange(Field::Year))
    }

    /// The parsed number in `field`.
    fn number(&self, field: Field) -> Result<i64, Fault> {
        let text = self.text(field);
        if text.is_empty() || text == "-" {
            return Err(Fault::Missing(field));
        }
        text.parse::<i64>().map_err(|_| Fault::NotANumber(field))
    }

    /// Mutable access to one field's text.
    fn field_mut(&mut self, field: Field) -> &mut String {
        match field {
            Field::Year => &mut self.year,
            Field::Month => &mut self.month,
            Field::Day => &mut self.day,
            Field::Hour => &mut self.hour,
            Field::Minute => &mut self.minute,
            Field::Second => &mut self.second,
        }
    }
}

/// `value` if it lies within `low..=high`, else the field's range fault.
fn in_range(value: i64, low: i64, high: i64, field: Field) -> Result<u32, Fault> {
    if !(low..=high).contains(&value) {
        return Err(Fault::OutOfRange(field));
    }
    u32::try_from(value).map_err(|_| Fault::OutOfRange(field))
}

/// Days in `month` of `year`, February following the proleptic-Gregorian
/// leap rule.
#[must_use]
pub fn days_in_month(year: i64, month: u32) -> u32 {
    match month {
        1 | 3 | 5 | 7 | 8 | 10 | 12 => 31,
        4 | 6 | 9 | 11 => 30,
        2 if is_leap_year(year) => 29,
        2 => 28,
        // Never reached: the month is range-checked before this is asked.
        // Answering zero refuses every day rather than inventing a length.
        _ => 0,
    }
}

/// Whether `year` is a leap year in the proleptic Gregorian calendar, which
/// is the calendar the shared day count uses.
#[must_use]
pub const fn is_leap_year(year: i64) -> bool {
    year % 4 == 0 && (year % 100 != 0 || year % 400 == 0)
}

/// One field's value as the text the user sees and edits.
///
/// Unpadded: a field is an edit box, not a formatted reading, and a leading
/// zero the user did not type would be text they then have to delete.
fn spell(value: i64) -> String {
    let mut out = String::new();
    // Writing into a `String` never fails; the `Result` is discarded
    // deliberately rather than unwrapped.
    let _ = write!(out, "{value}");
    out
}

#[cfg(test)]
#[path = "tests.rs"]
mod tests;
