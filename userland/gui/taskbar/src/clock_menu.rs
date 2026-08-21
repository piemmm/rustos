//! The taskbar clock's context menu: the reading it is showing, and the one
//! command it offers.
//!
//! [`ROWS`] is the single definition of the menu's shape; the rendered rows
//! and the row → command mapping are both derived from it, so a row can
//! never exist without a command behind it (or the reverse).
//!
//! Nothing here holds or checks authority. Setting the machine's time needs
//! `CAP_TIME_SET`, which neither the bar nor the desktop session holds: the
//! command asks the embedder to re-authenticate an account that has it and
//! run the Date & Time application as that account. A console with no
//! broker to authenticate against renders the row non-actionable with the
//! reason stated, rather than offering a command that could only fail.

use alloc::string::String;
use alloc::vec::Vec;

use tairix_controls::{AuthorityState, ControlRole, ControlState, MenuItem};

use crate::clock;

/// One command the clock's menu can offer.
///
/// A closed set: every variant has a row in [`ROWS`] and a typed outcome the
/// session knows how to apply.
#[derive(Copy, Clone, Debug, Eq, PartialEq)]
pub enum ClockAction {
    /// Set the machine's date and time, by way of an account that may.
    SetDateTime,
}

/// One row of the clock's menu.
#[derive(Copy, Clone, Debug, Eq, PartialEq)]
pub enum ClockRow {
    /// The reading the bar is showing, stated so the menu names what it is
    /// about. It commands nothing.
    Reading,
    /// Set the machine's date and time.
    SetDateTime,
}

impl ClockRow {
    /// The command choosing this row asks for, or `None` for a row that is
    /// a statement rather than a command.
    #[must_use]
    pub const fn action(self) -> Option<ClockAction> {
        match self {
            Self::Reading => None,
            Self::SetDateTime => Some(ClockAction::SetDateTime),
        }
    }
}

/// The clock menu, in order. This is the single definition of the menu's
/// shape; the rendered rows and the row → command mapping are both derived
/// from it, never written out a second time.
///
/// The reading leads because it says what the menu is about; the command
/// follows it in its own group.
pub const ROWS: &[ClockRow] = &[ClockRow::Reading, ClockRow::SetDateTime];

/// The label of the row that sets the machine's time.
///
/// Public because aiming *at* the row is the same fact as reading one back:
/// the desktop's QEMU vertical finds it by this name rather than by
/// restating its position.
pub const SET_ROW_LABEL: &str = "Set Date & Time…";

/// What the reading row states when no wall-clock time has been
/// established this boot.
///
/// The bar draws [`clock::UNSET_LABEL`] then, which is enough on a bar and
/// too terse for a heading; showing `00:00` would be a fabricated time.
pub const READING_UNSET_LABEL: &str = "Time not set";

/// Why the set-time row cannot act: this session's console has no
/// re-authentication broker, so no account holding `CAP_TIME_SET` can be
/// authenticated to run the application.
pub const REASON_NO_BROKER: &str = "This session cannot authenticate to set the time";

/// What the clock's menu is allowed to offer, as attested from outside the
/// bar.
///
/// The taskbar holds none of this authority itself; it renders what it was
/// told, and the refusing answer is the default.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ClockPermits {
    /// The reading the bar is drawing, or [`clock::UNSET_LABEL`] when the
    /// wall clock has never been set. The menu states it; it never re-derives
    /// it.
    pub reading: String,
    /// Whether this session can re-authenticate an account to set the time
    /// with — that is, whether its console has an elevation broker.
    pub set_available: bool,
}

/// Build the menu's rows for `permits`, in [`ROWS`] order.
///
/// The command is rendered whether or not it can act: a missing broker is
/// stated on a non-actionable row, never hidden (which would leave the user
/// guessing) and never silently offered (which would promise an action that
/// cannot happen).
#[must_use]
pub fn rows(permits: &ClockPermits) -> Vec<MenuItem> {
    ROWS.iter()
        .map(|row| match row {
            ClockRow::Reading => {
                let label = if states_a_time(&permits.reading) {
                    permits.reading.as_str()
                } else {
                    READING_UNSET_LABEL
                };
                MenuItem::new(label).with_state(ControlState::disabled())
            }
            ClockRow::SetDateTime => {
                let item = MenuItem::new(SET_ROW_LABEL)
                    .with_group_break(true)
                    .with_role(ControlRole::Neutral);
                if permits.set_available {
                    item
                } else {
                    item.with_state(
                        ControlState::default().with_authority(AuthorityState::NeedsCapability),
                    )
                    .with_reason(REASON_NO_BROKER)
                }
            }
        })
        .collect()
}

/// Whether `label` is a reading rather than the bar's stand-in for no time
/// at all — which the bar spells [`clock::UNSET_LABEL`], and which is empty
/// before the first reading is spelled.
fn states_a_time(label: &str) -> bool {
    !label.is_empty() && label != clock::UNSET_LABEL
}

/// The command the row at `index` asks for, or `None` for an index that
/// names no command — a statement row, or one past the end (fail closed,
/// never a guessed command).
#[must_use]
pub fn action_at(index: usize) -> Option<ClockAction> {
    ROWS.get(index).copied().and_then(ClockRow::action)
}

#[cfg(test)]
mod tests {
    use super::{
        action_at, rows, ClockAction, ClockPermits, ClockRow, READING_UNSET_LABEL,
        REASON_NO_BROKER, ROWS, SET_ROW_LABEL,
    };
    use alloc::string::ToString;

    fn permits(reading: &str, set_available: bool) -> ClockPermits {
        ClockPermits {
            reading: reading.to_string(),
            set_available,
        }
    }

    #[test]
    fn the_reading_leads_and_states_the_bars_own_label() {
        let items = rows(&permits("09:41", true));
        assert_eq!(items.len(), ROWS.len());
        assert_eq!(items[0].label(), "09:41");
        // A statement, never a command: it cannot be chosen.
        assert!(!items[0].state().is_actionable());
        assert_eq!(items[1].label(), SET_ROW_LABEL);
        assert!(items[1].state().is_actionable());
    }

    #[test]
    fn an_unset_clock_says_so_rather_than_showing_a_fabricated_time() {
        // What the bar actually draws while the clock is unset, and the state
        // before the first reading has been spelled at all.
        for reading in [crate::clock::UNSET_LABEL, ""] {
            let items = rows(&permits(reading, true));
            assert_eq!(items[0].label(), READING_UNSET_LABEL);
        }
    }

    #[test]
    fn without_a_broker_the_command_is_rendered_refused_with_its_reason() {
        let items = rows(&permits("09:41", false));
        assert!(!items[1].state().is_actionable());
        assert_eq!(items[1].reason(), Some(REASON_NO_BROKER));
    }

    #[test]
    fn only_the_command_row_names_a_command() {
        assert_eq!(action_at(0), None);
        assert_eq!(action_at(1), Some(ClockAction::SetDateTime));
        // Past the end names nothing rather than the nearest row.
        assert_eq!(action_at(2), None);
    }

    #[test]
    fn every_row_in_the_table_renders_and_maps_back_consistently() {
        let items = rows(&permits("09:41", true));
        for (index, row) in ROWS.iter().enumerate() {
            assert_eq!(action_at(index), row.action());
            assert!(!items[index].label().is_empty());
        }
        assert_eq!(ROWS.first().copied(), Some(ClockRow::Reading));
    }
}
