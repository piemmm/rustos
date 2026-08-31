//! Unit tests for the terminal's window menu: the row model it hands the
//! desktop, and the shortcuts that reach the same commands without one.
//!
//! The plates, placement, grab, traversal and dismissal are the session's and
//! are tested there (`userland/gui/session/src/menu_tests.rs`); nothing here
//! needs a screen.

use alloc::vec::Vec;

use tairix_abi::window_ipc::{AppMenuItemId, AppMenuRowView};
use tairix_input::{Key, Modifiers, NamedKey};

use super::{model, Command};
use crate::APP_NAME;

fn ctrl() -> Modifiers {
    Modifiers {
        ctrl: true,
        ..Modifiers::default()
    }
}

fn ctrl_shift() -> Modifiers {
    Modifiers {
        ctrl: true,
        shift: true,
        ..Modifiers::default()
    }
}

/// The keystroke an accelerator caption tells the user to type.
///
/// Parsing the caption is what lets a row's advertisement be checked against
/// the matcher rather than restated beside it.
fn typed(caption: &str) -> (Key, Modifiers) {
    let mut modifiers = Modifiers::default();
    let mut key = None;
    for word in caption.split(' ') {
        match word {
            "Ctrl" => modifiers.ctrl = true,
            "Shift" => modifiers.shift = true,
            other => {
                let mut chars = other.chars();
                let ch = chars.next().expect("a caption word is never empty");
                assert!(
                    chars.next().is_none(),
                    "{caption}: the key a caption names is one character"
                );
                key = Some(Key::Char(ch));
            }
        }
    }
    (key.expect("a caption names a key"), modifiers)
}

/// Every `Item` row of the built menu, in the order it declares them.
fn rows() -> Vec<(AppMenuItemId, &'static str, &'static str)> {
    // The menu owns its text, so the views borrow it; the fixed captions are
    // `'static` in the source, and each is copied out by matching on them.
    let built = model().expect("the fixed rows fit");
    let mut out = Vec::new();
    for (row, parent) in built.rows() {
        assert_eq!(parent, None, "the terminal declares one plate");
        if let AppMenuRowView::Item(item) = row {
            let command = Command::from_item(item.id).expect("a row the terminal declared");
            out.push((item.id, command.label(), command.shortcut()));
        }
    }
    out
}

// --- The row model ---------------------------------------------------------

#[test]
fn the_root_plate_is_titled_with_the_applications_name() {
    let built = model().expect("the fixed rows fit");
    assert_eq!(built.title(), APP_NAME);
}

#[test]
fn the_rows_are_every_command_in_order_and_nothing_else() {
    let built = model().expect("the fixed rows fit");
    let items: Vec<AppMenuItemId> = built
        .rows()
        .filter_map(|(row, _)| match row {
            AppMenuRowView::Item(item) => Some(item.id),
            _ => None,
        })
        .collect();
    assert_eq!(items.len(), Command::ALL.len());
    for (id, command) in items.into_iter().zip(Command::ALL) {
        assert_eq!(
            Command::from_item(id),
            Some(command),
            "a row reads back as the command that declared it"
        );
    }
}

#[test]
fn a_group_opener_declares_the_divider_above_it_and_no_other_row_does() {
    let built = model().expect("the fixed rows fit");
    let mut separator_pending = false;
    let mut seen = 0usize;
    for (row, _) in built.rows() {
        match row {
            AppMenuRowView::Separator => {
                assert!(seen > 0, "no divider is declared above the first row");
                assert!(!separator_pending, "no two dividers run together");
                separator_pending = true;
            }
            AppMenuRowView::Item(item) => {
                let command = Command::from_item(item.id).expect("a declared row");
                assert_eq!(
                    separator_pending,
                    command.opens_group(),
                    "{}: a divider is declared exactly for a group opener",
                    command.label()
                );
                separator_pending = false;
                seen += 1;
            }
            other => panic!("the terminal declares no {other:?} row"),
        }
    }
    assert!(!separator_pending, "no divider is left hanging at the end");
}

/// The chain this opens is one plate that hangs no window, which is why the
/// terminal keeps no attached-window bookkeeping of its own.
#[test]
fn no_row_declares_a_submenu_a_panel_or_an_information_row() {
    let built = model().expect("the fixed rows fit");
    for (row, _) in built.rows() {
        assert!(
            matches!(row, AppMenuRowView::Item(_) | AppMenuRowView::Separator),
            "unexpected row kind {row:?}"
        );
    }
}

#[test]
fn an_id_the_terminal_never_declared_names_no_command() {
    let highest = u16::try_from(Command::ALL.len()).expect("six rows");
    for raw in [highest + 1, highest + 2, u16::MAX] {
        assert_eq!(
            Command::from_item(AppMenuItemId::new(raw).expect("non-zero")),
            None,
            "id {raw} names no command"
        );
    }
}

#[test]
fn every_label_and_caption_is_non_empty_and_distinct() {
    let declared = rows();
    for (_, label, shortcut) in &declared {
        assert!(!label.is_empty());
        assert!(!shortcut.is_empty());
    }
    for (i, (_, label, shortcut)) in declared.iter().enumerate() {
        for (_, other_label, other_shortcut) in declared.iter().skip(i + 1) {
            assert_ne!(label, other_label, "labels must be distinct");
            assert_ne!(shortcut, other_shortcut, "captions must be distinct");
        }
    }
}

/// The property the caption's rustdoc claims, checked rather than asserted in
/// prose: a row never advertises a key combination that does nothing.
#[test]
fn every_row_advertises_a_shortcut_the_matcher_really_honours() {
    for (id, label, shortcut) in rows() {
        let command = Command::from_item(id).expect("a declared row");
        let (key, modifiers) = typed(shortcut);
        assert_eq!(
            Command::accelerator(key, modifiers),
            Some(command),
            "{label} advertises {shortcut}, which must reach it"
        );
    }
}

// --- The accelerators ------------------------------------------------------

#[test]
fn accelerator_maps_every_documented_shortcut_to_its_command() {
    assert_eq!(
        Command::accelerator(Key::Char(','), ctrl()),
        Some(Command::Settings)
    );
    assert_eq!(
        Command::accelerator(Key::Char('+'), ctrl()),
        Some(Command::Larger)
    );
    assert_eq!(
        Command::accelerator(Key::Char('='), ctrl()),
        Some(Command::Larger)
    );
    assert_eq!(
        Command::accelerator(Key::Char('-'), ctrl()),
        Some(Command::Smaller)
    );
    assert_eq!(
        Command::accelerator(Key::Char('_'), ctrl()),
        Some(Command::Smaller)
    );
    assert_eq!(
        Command::accelerator(Key::Char('0'), ctrl()),
        Some(Command::ActualSize)
    );
    assert_eq!(
        Command::accelerator(Key::Char('k'), ctrl_shift()),
        Some(Command::Clear)
    );
    assert_eq!(
        Command::accelerator(Key::Char('K'), ctrl_shift()),
        Some(Command::Clear)
    );
    assert_eq!(
        Command::accelerator(Key::Char('w'), ctrl_shift()),
        Some(Command::Close)
    );
    assert_eq!(
        Command::accelerator(Key::Char('W'), ctrl_shift()),
        Some(Command::Close)
    );
}

#[test]
fn accelerator_requires_ctrl() {
    assert_eq!(
        Command::accelerator(Key::Char(','), Modifiers::default()),
        None
    );
    assert_eq!(
        Command::accelerator(
            Key::Char('k'),
            Modifiers {
                shift: true,
                ..Modifiers::default()
            }
        ),
        None
    );
}

#[test]
fn accelerator_requires_shift_where_documented() {
    assert_eq!(Command::accelerator(Key::Char('k'), ctrl()), None);
    assert_eq!(Command::accelerator(Key::Char('w'), ctrl()), None);
}

#[test]
fn accelerator_returns_none_for_a_plain_character() {
    assert_eq!(Command::accelerator(Key::Char('a'), ctrl()), None);
}

#[test]
fn accelerator_returns_none_for_a_named_key() {
    assert_eq!(
        Command::accelerator(Key::Named(NamedKey::Enter), ctrl()),
        None
    );
    assert_eq!(
        Command::accelerator(Key::Named(NamedKey::Escape), ctrl_shift()),
        None
    );
}

#[test]
fn accelerator_returns_none_for_an_unrelated_ctrl_combination() {
    assert_eq!(Command::accelerator(Key::Char('z'), ctrl()), None);
    assert_eq!(Command::accelerator(Key::Char('q'), ctrl_shift()), None);
}
