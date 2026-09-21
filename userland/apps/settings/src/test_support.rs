//! The scaffolding every pane's host tests drive the shell through.
//!
//! One definition of "a shell showing this pane", "what its rows say",
//! "press the band" and "offer an account", rather than a copy per test
//! module: six modules exercise the same seams, and six copies of the
//! pointer sequence a band press is made of would drift the moment the
//! band's anatomy moved.
//!
//! No transport and no broker anywhere. The shell is *told* what a reading
//! answered and what an elevated run came to, which is exactly the seam
//! the production caller drives it through.

use alloc::string::String;
use alloc::vec::Vec;

use tairix_controls::{FieldControl, FieldRow};
use tairix_font::install_test_transport;
use tairix_geometry::{to_i32, Point, Rect, Region, Scale};
use tairix_input::{InputEvent, Key, Modifiers, NamedKey, PointerButton};
use tairix_theme::Theme;
use tairix_wallpaper::DesktopSettings;

use crate::registry::{Pane, PaneRow, CATEGORIES};
use crate::shell::{Elevation, Shell, ShellOutcome};

/// A window wide enough to seat the strip and a full content column.
pub(crate) const WIDE: Rect = Rect::new(0, 0, 900, 640);

/// The dark theme, with the solid test glyph transport installed so a
/// paint measures text without a running font service.
pub(crate) fn theme() -> Theme {
    install_test_transport();
    Theme::dark()
}

/// A damage sink to route an event into.
pub(crate) fn damage() -> Region {
    tairix_controls::damage::sink()
}

/// A shell showing `pane`, laid out for [`WIDE`].
pub(crate) fn showing(pane: &str) -> Shell {
    let mut shell = Shell::new(DesktopSettings::default()).expect("a registry");
    let mut sink = damage();
    assert!(
        shell.go_to_pane(pane, WIDE, Scale::ONE, &theme(), &mut sink),
        "the registry carries the pane"
    );
    shell.lay_out(WIDE, Scale::ONE, &theme());
    shell
}

/// Every row the pane on show draws, whichever shape of body it is.
pub(crate) fn rows(shell: &Shell) -> Vec<FieldRow> {
    if let Some(facts) = shell.facts_for_test() {
        return facts.rows();
    }
    shell.form_for_test().map_or_else(Vec::new, |form| {
        form.groups()
            .iter()
            .flat_map(|group| group.rows().iter().cloned())
            .collect()
    })
}

/// Every value the pane on show states, in listing order: what a reading
/// says, what an entry holds, and what a list has chosen.
pub(crate) fn stated(shell: &Shell) -> Vec<String> {
    rows(shell).iter().map(value_of).collect()
}

/// What one row says.
pub(crate) fn value_of(row: &FieldRow) -> String {
    match row.control() {
        FieldControl::Reading(value) | FieldControl::Unmeasured(value) => value.clone(),
        FieldControl::Text(entry) => String::from(entry.text()),
        FieldControl::Combo(combo) => combo.selected_text().map(String::from).unwrap_or_default(),
        _ => String::new(),
    }
}

/// Every label the pane on show states, in listing order.
pub(crate) fn labels(shell: &Shell) -> Vec<String> {
    rows(shell)
        .iter()
        .map(|row| String::from(row.label()))
        .collect()
}

/// Each plate's caption, in listing order.
pub(crate) fn captions(shell: &Shell) -> Vec<String> {
    shell.form_for_test().map_or_else(Vec::new, |form| {
        form.groups()
            .iter()
            .map(|group| String::from(group.caption()))
            .collect()
    })
}

/// Which row of which plate carries `label`.
pub(crate) fn row_at(shell: &Shell, caption: &str, label: &str) -> (usize, usize) {
    let form = shell.form_for_test().expect("a composed pane");
    for (group, plate) in form.groups().iter().enumerate() {
        if plate.caption() != caption {
            continue;
        }
        for (row, held) in plate.rows().iter().enumerate() {
            if held.label() == label {
                return (group, row);
            }
        }
    }
    panic!("no `{label}` row on the `{caption}` plate");
}

/// Press the band's command at `index` (`0` Revert, `1` Apply on a staged
/// band; `0` the one command on a reading band), where the band actually
/// drew it.
pub(crate) fn press_band(shell: &mut Shell, index: usize) -> ShellOutcome {
    let theme = theme();
    let rects = shell.action_rects(WIDE, Scale::ONE, &theme);
    let rect = rects[index];
    let at = Point::new(
        rect.left() + to_i32(rect.width / 2),
        rect.top() + to_i32(rect.height / 2),
    );
    let mut sink = damage();
    let mut outcome = ShellOutcome::Idle;
    for event in [
        InputEvent::PointerMoved { to: at },
        InputEvent::PointerPressed {
            button: PointerButton::Primary,
        },
        InputEvent::PointerReleased {
            button: PointerButton::Primary,
        },
    ] {
        outcome = shell.on_pointer(&event, WIDE, Scale::ONE, &theme, &mut sink);
    }
    outcome
}

/// Offer an account to the question standing over the window, and hand
/// back the elevation the shell asked for.
pub(crate) fn offer_account(shell: &mut Shell) -> Elevation {
    let theme = theme();
    let mut sink = damage();
    assert!(shell.asking(), "the pane asks for an account");
    let mut key = |key: Key| {
        shell.on_key(
            key,
            Modifiers::default(),
            WIDE,
            Scale::ONE,
            &theme,
            &mut sink,
        )
    };
    for ch in "root".chars() {
        key(Key::Char(ch));
    }
    key(Key::Named(NamedKey::Tab));
    for ch in "hunter2".chars() {
        key(Key::Char(ch));
    }
    let ShellOutcome::Elevate(asked) = key(Key::Named(NamedKey::Enter)) else {
        panic!("offering an account asks for the run");
    };
    asked
}

/// What the pane's action band is saying.
pub(crate) fn band_line(shell: &Shell) -> String {
    shell.band_line_for_test().unwrap_or_default()
}

/// The registry row for `pane`.
pub(crate) fn row_for(pane: Pane) -> &'static PaneRow {
    CATEGORIES
        .iter()
        .flat_map(|category| category.panes)
        .find(|row| row.pane == pane)
        .expect("every pane has a row")
}
