//! Host tests for the SLC-driven local line editor.

use alloc::vec::Vec;

use tairix_vt::control;

use super::{EditAction, Editor, MAX_LINE};
use crate::linemode::{mode, slc, sub, Linemode};

/// A LINEMODE state with `mask` in force, as the server would have set it.
fn linemode(mask: u8) -> Linemode {
    let mut lm = Linemode::new();
    lm.fold(&[sub::MODE, mask]);
    assert_eq!(lm.mask(), mask & mode::NEGOTIATED);
    lm
}

/// Feed `input` and return the action the last byte produced plus the echo.
fn feed(editor: &mut Editor, lm: &Linemode, input: &[u8]) -> (EditAction, Vec<u8>) {
    let mut echo = Vec::new();
    let mut action = EditAction::Pending;
    for &byte in input {
        action = editor.push(byte, lm, &mut echo);
    }
    (action, echo)
}

#[test]
fn typing_accumulates_a_line_and_echoes_it() {
    let lm = linemode(mode::EDIT);
    let mut editor = Editor::new();
    let (action, echo) = feed(&mut editor, &lm, b"hello");
    assert_eq!(action, EditAction::Pending);
    assert_eq!(editor.line(), b"hello");
    assert_eq!(echo, b"hello".to_vec());
}

#[test]
fn return_completes_the_line_and_echoes_a_new_line() {
    let lm = linemode(mode::EDIT);
    let mut editor = Editor::new();
    let (action, echo) = feed(&mut editor, &lm, b"hi\r");
    assert_eq!(action, EditAction::Line);
    assert_eq!(editor.take_line(), b"hi".to_vec());
    assert_eq!(echo, b"hi\r\n".to_vec());
    assert!(editor.is_empty());
}

#[test]
fn a_line_feed_ends_the_line_too() {
    let lm = linemode(mode::EDIT);
    let mut editor = Editor::new();
    assert_eq!(feed(&mut editor, &lm, b"hi\n").0, EditAction::Line);
}

#[test]
fn the_negotiated_erase_character_rubs_one_out() {
    let mut lm = linemode(mode::EDIT);
    // The server binds erase to `^H` rather than the default Delete.
    lm.slc_mut().set_local(slc::EC, control::BS);
    let mut editor = Editor::new();
    let (_, echo) = feed(&mut editor, &lm, &[b'a', b'b', control::BS]);
    assert_eq!(editor.line(), b"a");
    assert_eq!(
        echo,
        b"ab"
            .iter()
            .copied()
            .chain(control::ERASE_ECHO)
            .collect::<Vec<u8>>()
    );
}

#[test]
fn backspace_erases_even_when_the_server_bound_erase_elsewhere() {
    // A terminal whose Backspace key did nothing would be unusable, so the
    // shared single-byte erase always erases; `lib/vt` owns which bytes those
    // are.
    let mut lm = linemode(mode::EDIT);
    lm.slc_mut().set_local(slc::EC, b'#');
    let mut editor = Editor::new();
    feed(&mut editor, &lm, &[b'a', b'b', control::BS]);
    assert_eq!(editor.line(), b"a");
    feed(&mut editor, &lm, b"#");
    assert!(editor.is_empty(), "the negotiated character erases too");
}

#[test]
fn an_erase_on_an_empty_line_is_a_no_op() {
    let lm = linemode(mode::EDIT);
    let mut editor = Editor::new();
    let (action, echo) = feed(&mut editor, &lm, &[control::BS]);
    assert_eq!(action, EditAction::Pending);
    assert!(editor.is_empty());
    assert!(echo.is_empty(), "the cursor never walks back over a prompt");
}

#[test]
fn the_delete_key_sequence_erases_once_even_split_across_reads() {
    let lm = linemode(mode::EDIT);
    let delete = [control::ESC, control::CSI, b'3', control::TILDE];
    for split in 1..delete.len() {
        let mut editor = Editor::new();
        feed(&mut editor, &lm, b"ab");
        feed(&mut editor, &lm, &delete[..split]);
        feed(&mut editor, &lm, &delete[split..]);
        assert_eq!(editor.line(), b"a", "split at {split}");
    }
}

#[test]
fn the_kill_character_erases_the_whole_line() {
    let lm = linemode(mode::EDIT);
    let kill = lm
        .slc()
        .char_for(slc::EL)
        .expect("a default kill character");
    let mut editor = Editor::new();
    let (_, echo) = feed(&mut editor, &lm, b"hello");
    assert_eq!(echo.len(), 5);
    let (action, echo) = feed(&mut editor, &lm, &[kill]);
    assert_eq!(action, EditAction::Pending);
    assert!(editor.is_empty());
    assert_eq!(
        echo.len(),
        5 * control::ERASE_ECHO.len(),
        "one rub-out per painted column"
    );
}

#[test]
fn the_word_erase_character_removes_trailing_blanks_then_one_word() {
    let lm = linemode(mode::EDIT);
    let erase_word = lm.slc().char_for(slc::EW).expect("a default");
    let mut editor = Editor::new();
    feed(&mut editor, &lm, b"one two   ");
    feed(&mut editor, &lm, &[erase_word]);
    assert_eq!(editor.line(), b"one ");
    feed(&mut editor, &lm, &[erase_word]);
    assert_eq!(
        editor.line(),
        b"",
        "the blank and the word before it go too"
    );
}

#[test]
fn the_reprint_character_repaints_the_line_on_a_fresh_row() {
    let lm = linemode(mode::EDIT);
    let reprint = lm.slc().char_for(slc::RP).expect("a default");
    let mut editor = Editor::new();
    feed(&mut editor, &lm, b"abc");
    let (action, echo) = feed(&mut editor, &lm, &[reprint]);
    assert_eq!(action, EditAction::Pending);
    assert_eq!(echo, b"\r\nabc".to_vec());
    assert_eq!(editor.line(), b"abc", "the line itself is unchanged");
}

#[test]
fn the_literal_next_character_takes_the_following_byte_verbatim() {
    let lm = linemode(mode::EDIT);
    let lnext = lm.slc().char_for(slc::LNEXT).expect("a default");
    let kill = lm.slc().char_for(slc::EL).expect("a default");
    let mut editor = Editor::new();
    feed(&mut editor, &lm, b"ab");
    // The kill character, escaped, must land in the line rather than erase it.
    feed(&mut editor, &lm, &[lnext, kill]);
    assert_eq!(editor.line(), &[b'a', b'b', kill]);
}

#[test]
fn literal_next_also_escapes_the_line_terminator() {
    let lm = linemode(mode::EDIT);
    let lnext = lm.slc().char_for(slc::LNEXT).expect("a default");
    let mut editor = Editor::new();
    let (action, _) = feed(&mut editor, &lm, &[lnext, b'\r']);
    assert_eq!(action, EditAction::Pending, "the line does not end");
    assert_eq!(editor.line(), b"\r");
}

#[test]
fn a_control_byte_echoes_as_a_caret_pair_and_erases_two_columns() {
    let lm = linemode(mode::EDIT);
    let mut editor = Editor::new();
    // 0x01 is `^A`; it is bound to no SLC function by default, so it is data.
    let (_, echo) = feed(&mut editor, &lm, &[0x01]);
    assert_eq!(echo, b"^A".to_vec());
    let (_, echo) = feed(&mut editor, &lm, &[control::BS]);
    assert_eq!(
        echo.len(),
        2 * control::ERASE_ECHO.len(),
        "both painted columns are rubbed out"
    );
    assert!(editor.is_empty());
}

#[test]
fn lit_echo_paints_a_control_byte_verbatim() {
    let lm = linemode(mode::EDIT | mode::LIT_ECHO);
    let mut editor = Editor::new();
    let (_, echo) = feed(&mut editor, &lm, &[0x01]);
    assert_eq!(echo, alloc::vec![0x01]);
    let (_, echo) = feed(&mut editor, &lm, &[control::BS]);
    assert_eq!(echo.len(), control::ERASE_ECHO.len(), "one column now");
}

#[test]
fn soft_tab_expands_a_tab_to_the_next_stop() {
    let lm = linemode(mode::EDIT | mode::SOFT_TAB);
    let mut editor = Editor::new();
    let (_, echo) = feed(&mut editor, &lm, &[b'a', control::HT]);
    assert_eq!(editor.line(), b"a       ", "column 1 to column 8");
    assert_eq!(echo, b"a       ".to_vec());
}

#[test]
fn soft_tab_from_column_zero_fills_a_whole_stop() {
    let lm = linemode(mode::EDIT | mode::SOFT_TAB);
    let mut editor = Editor::new();
    feed(&mut editor, &lm, &[control::HT]);
    assert_eq!(editor.line().len(), 8);
}

#[test]
fn a_tab_without_soft_tab_is_stored_and_its_erase_repaints() {
    let lm = linemode(mode::EDIT);
    let mut editor = Editor::new();
    feed(&mut editor, &lm, &[b'a', control::HT, b'b']);
    assert_eq!(editor.line(), &[b'a', control::HT, b'b']);
    // The line still ends with `b`, whose width is known, so that erase is an
    // ordinary rub-out.
    let (_, echo) = feed(&mut editor, &lm, &[control::BS]);
    assert_eq!(echo.len(), control::ERASE_ECHO.len());
    // Erasing the Tab itself cannot be done a column at a time, because only
    // the terminal knows how far it advanced; the line is repainted instead.
    let (_, echo) = feed(&mut editor, &lm, &[control::BS]);
    assert_eq!(echo, b"\r\na".to_vec());
    assert_eq!(editor.line(), b"a");
}

#[test]
fn a_signal_character_fires_only_when_the_server_asked_for_the_mapping() {
    let interrupt = Linemode::new()
        .slc()
        .char_for(slc::IP)
        .expect("a default interrupt character");

    // Without TRAPSIG the character is ordinary data.
    let lm = linemode(mode::EDIT);
    let mut editor = Editor::new();
    let (action, _) = feed(&mut editor, &lm, &[interrupt]);
    assert_eq!(action, EditAction::Pending);
    assert_eq!(editor.line(), &[interrupt]);

    // With it, the function is reported for the session to map to a command.
    let lm = linemode(mode::EDIT | mode::TRAPSIG);
    let mut editor = Editor::new();
    let (action, _) = feed(&mut editor, &lm, &[interrupt]);
    assert_eq!(action, EditAction::Signal(slc::IP));
}

#[test]
fn every_signal_function_the_server_binds_is_reported() {
    let lm = linemode(mode::EDIT | mode::TRAPSIG);
    for function in [slc::IP, slc::ABORT, slc::SUSP, slc::EOF, slc::AO, slc::AYT] {
        let byte = lm.slc().char_for(function).expect("a default");
        let mut editor = Editor::new();
        assert_eq!(
            feed(&mut editor, &lm, &[byte]).0,
            EditAction::Signal(function),
            "function {function}"
        );
    }
}

#[test]
fn a_forwarding_character_forwards_the_partial_line() {
    let mut lm = linemode(mode::EDIT);
    // Bit 7 of octet 0 is code 0, so `;` at 0x3B is octet 7, bit 4.
    let mut mask = alloc::vec![0u8; crate::linemode::ForwardMask::LEN];
    mask[7] = 1 << (7 - (0x3B & 0x07));
    let mut request = alloc::vec![crate::nvt::DO, sub::FORWARDMASK];
    request.extend_from_slice(&mask);
    lm.fold(&request);

    let mut editor = Editor::new();
    let (action, _) = feed(&mut editor, &lm, b"select 1;");
    assert_eq!(action, EditAction::Forward);
    assert_eq!(
        editor.take_line(),
        b"select 1;".to_vec(),
        "the forwarding character is part of what is forwarded"
    );
}

#[test]
fn a_full_buffer_forwards_rather_than_growing() {
    let lm = linemode(mode::EDIT);
    let mut editor = Editor::new();
    let mut action = EditAction::Pending;
    for _ in 0..MAX_LINE {
        let mut echo = Vec::new();
        action = editor.push(b'x', &lm, &mut echo);
    }
    assert_eq!(action, EditAction::Forward);
    assert_eq!(editor.line().len(), MAX_LINE);
}

#[test]
fn the_buffer_never_exceeds_its_bound_however_much_is_typed() {
    let lm = linemode(mode::EDIT | mode::SOFT_TAB);
    let mut editor = Editor::new();
    let mut echo = Vec::new();
    for _ in 0..MAX_LINE * 4 {
        editor.push(control::HT, &lm, &mut echo);
    }
    assert!(editor.line().len() <= MAX_LINE, "{}", editor.line().len());
}

#[test]
fn discard_drops_the_line_and_any_held_state() {
    let lm = linemode(mode::EDIT);
    let lnext = lm.slc().char_for(slc::LNEXT).expect("a default");
    let mut editor = Editor::new();
    feed(&mut editor, &lm, b"abc");
    feed(&mut editor, &lm, &[lnext]);
    editor.discard();
    assert!(editor.is_empty());
    // The held literal-next must be gone: a Return now ends the line.
    assert_eq!(feed(&mut editor, &lm, b"\r").0, EditAction::Line);
}

#[test]
fn arbitrary_input_never_panics_and_stays_bounded() {
    let lm = linemode(mode::EDIT | mode::TRAPSIG | mode::SOFT_TAB | mode::LIT_ECHO);
    let mut editor = Editor::new();
    let mut echo = Vec::new();
    // Every byte value, twice over, through one editor: an editor is fed local
    // keystrokes, but a paste can carry anything and must not be able to break
    // it.
    for round in 0..2 {
        for byte in 0u16..=255 {
            let byte = u8::try_from(byte).expect("0..=255 fits u8");
            if matches!(editor.push(byte, &lm, &mut echo), EditAction::Line) {
                let _ = editor.take_line();
            }
            assert!(
                editor.line().len() <= MAX_LINE,
                "round {round}, byte {byte}"
            );
        }
    }
}
