use super::{Document, Setting, Unparsed};
use crate::{ConfError, MAX_DOCUMENT_LEN, MAX_LINES, MAX_SETTINGS};
use alloc::string::String;
use alloc::vec::Vec;

/// The document a user might plausibly have hand-edited: aligned `=` signs,
/// a comment of their own, a blank line, an inline note, and a value that
/// needs quoting.
const HAND_EDITED: &str = concat!(
    "# my terminal, my rules\n",
    "scheme       = dark\n",
    "font.size    = 14   # a little bigger\n",
    "\n",
    "effects.blur = 500\n",
    "greeting     = \"  hello # world \\n \"\n",
);

#[test]
fn a_document_round_trips_byte_for_byte() {
    let doc = Document::parse(HAND_EDITED).expect("parses");
    assert_eq!(doc.render(), HAND_EDITED);
}

#[test]
fn every_setting_reads_back_decoded() {
    let doc = Document::parse(HAND_EDITED).expect("parses");
    assert_eq!(doc.get("scheme"), Some("dark"));
    assert_eq!(doc.u32("font.size"), Ok(Some(14)));
    assert_eq!(doc.permille("effects.blur"), Ok(Some(500)));
    assert_eq!(doc.get("greeting"), Some("  hello # world \n "));
    assert_eq!(doc.get("absent"), None);
}

/// The engine's hard requirement: a save rewrites the one line it must and
/// leaves every other byte — comments, blank lines, the user's alignment,
/// key order — exactly as it found them.
#[test]
fn a_write_changes_one_line_and_nothing_else() {
    let mut doc = Document::parse(HAND_EDITED).expect("parses");
    doc.set_u32("font.size", 16).expect("sets");
    assert_eq!(
        doc.render(),
        concat!(
            "# my terminal, my rules\n",
            "scheme       = dark\n",
            "font.size = 16   # a little bigger\n",
            "\n",
            "effects.blur = 500\n",
            "greeting     = \"  hello # world \\n \"\n",
        ),
        "only the touched line changes, and its inline comment survives"
    );
}

#[test]
fn a_new_key_is_appended_and_nothing_is_reordered() {
    let mut doc = Document::parse("scheme = dark\n").expect("parses");
    doc.set("font.face", "mono").expect("sets");
    assert_eq!(doc.render(), "scheme = dark\nfont.face = mono\n");
}

#[test]
fn a_first_write_to_an_empty_document_makes_a_file() {
    let mut doc = Document::new();
    assert_eq!(doc.render(), "");
    doc.set_bool("confirm.delete", true).expect("sets");
    assert_eq!(doc.render(), "confirm.delete = true\n");
    assert_eq!(doc.bool("confirm.delete"), Ok(Some(true)));
}

#[test]
fn unsetting_removes_the_line_and_leaves_the_rest() {
    let mut doc = Document::parse(HAND_EDITED).expect("parses");
    doc.unset("font.size");
    assert_eq!(doc.get("font.size"), None);
    assert_eq!(
        doc.render(),
        concat!(
            "# my terminal, my rules\n",
            "scheme       = dark\n",
            "\n",
            "effects.blur = 500\n",
            "greeting     = \"  hello # world \\n \"\n",
        )
    );
    // Unsetting a key the document never had changes nothing.
    let before = doc.render();
    doc.unset("never.set");
    assert_eq!(doc.render(), before);
}

/// A line the grammar cannot read costs only itself: every other setting is
/// still there, the line is still in the file, and a save puts it back.
#[test]
fn an_unreadable_line_is_kept_and_reported_and_costs_nothing_else() {
    let text = "scheme = dark\nthis is not a setting\nBadKey = 1\nfont.size = 14\n";
    let doc = Document::parse(text).expect("parses");
    assert_eq!(doc.get("scheme"), Some("dark"));
    assert_eq!(doc.u32("font.size"), Ok(Some(14)));
    let refused: Vec<Unparsed<'_>> = doc.unparsed().collect();
    assert_eq!(
        refused,
        [
            Unparsed {
                line: 2,
                text: "this is not a setting"
            },
            Unparsed {
                line: 3,
                text: "BadKey = 1"
            },
        ]
    );
    assert_eq!(doc.render(), text, "a save puts the refused lines back");
}

#[test]
fn comments_and_blank_lines_are_not_reported_as_problems() {
    let doc = Document::parse("# a note\n\n   \nscheme = dark\n").expect("parses");
    assert_eq!(doc.unparsed().count(), 0);
    assert_eq!(doc.settings().count(), 1);
}

/// A hand-edit that appends a second line for a key means the later one; a
/// save then says it once.
#[test]
fn a_duplicate_key_reads_last_and_collapses_on_write() {
    let mut doc = Document::parse("scheme = dark\nscheme = light\n").expect("parses");
    assert_eq!(doc.get("scheme"), Some("light"));
    let listed: Vec<Setting<'_>> = doc.settings().collect();
    assert_eq!(listed.len(), 2, "a listing shows the file as written");

    doc.set("scheme", "solarized").expect("sets");
    assert_eq!(doc.render(), "scheme = solarized\n");
    assert_eq!(doc.get("scheme"), Some("solarized"));
}

#[test]
fn a_value_that_needs_quoting_gets_it_and_reads_back_identically() {
    let mut doc = Document::new();
    for (key, value) in [
        ("plain", "dark"),
        ("empty", ""),
        ("spaced", "  padded  "),
        ("hashed", "a # b"),
        ("quoted", "say \"hi\""),
        ("escaped", "back\\slash"),
        ("multiline", "one\ntwo"),
        ("tabbed", "a\tb"),
    ] {
        doc.set(key, value).expect("sets");
        assert_eq!(doc.get(key), Some(value), "`{key}` must read back exactly");
    }
    let reparsed = Document::parse(&doc.render()).expect("a rendered document parses");
    for (key, value) in [
        ("plain", "dark"),
        ("empty", ""),
        ("spaced", "  padded  "),
        ("hashed", "a # b"),
        ("quoted", "say \"hi\""),
        ("escaped", "back\\slash"),
        ("multiline", "one\ntwo"),
        ("tabbed", "a\tb"),
    ] {
        assert_eq!(reparsed.get(key), Some(value), "`{key}` survives a save");
    }
}

#[test]
fn typed_reads_report_a_malformed_value_rather_than_a_default() {
    let doc = Document::parse(concat!(
        "flag = maybe\n",
        "count = twelve\n",
        "signed = 1.5\n",
        "blur = 1001\n",
        "negative = -1\n",
    ))
    .expect("parses");
    assert_eq!(doc.bool("flag"), Err(ConfError::ValueMalformed));
    assert_eq!(doc.u32("count"), Err(ConfError::ValueMalformed));
    assert_eq!(doc.i64("signed"), Err(ConfError::ValueMalformed));
    assert_eq!(doc.permille("blur"), Err(ConfError::ValueMalformed));
    assert_eq!(doc.u32("negative"), Err(ConfError::ValueMalformed));
    assert_eq!(doc.i64("negative"), Ok(Some(-1)));
    // Absent is distinct from malformed, so an app can tell them apart.
    assert_eq!(doc.bool("nothing"), Ok(None));
}

#[test]
fn both_boolean_spellings_are_accepted_and_one_is_written() {
    let doc = Document::parse("a = true\nb = on\nc = false\nd = off\n").expect("parses");
    assert_eq!(doc.bool("a"), Ok(Some(true)));
    assert_eq!(doc.bool("b"), Ok(Some(true)));
    assert_eq!(doc.bool("c"), Ok(Some(false)));
    assert_eq!(doc.bool("d"), Ok(Some(false)));

    let mut written = Document::new();
    written.set_bool("a", true).expect("sets");
    written.set_bool("b", false).expect("sets");
    assert_eq!(written.render(), "a = true\nb = false\n");
}

#[test]
fn signed_and_permille_writes_round_trip() {
    let mut doc = Document::new();
    doc.set_i64("offset", -42).expect("sets");
    doc.set_permille("blur", 750).expect("sets");
    assert_eq!(doc.i64("offset"), Ok(Some(-42)));
    assert_eq!(doc.permille("blur"), Ok(Some(750)));
    assert_eq!(
        doc.set_permille("blur", 1001),
        Err(ConfError::ValueMalformed),
        "a fraction beyond full is refused, not clamped"
    );
}

#[test]
fn a_write_refuses_a_key_or_value_outside_the_grammar() {
    let mut doc = Document::new();
    assert_eq!(doc.set("Bad Key", "x"), Err(ConfError::KeyInvalid));
    assert_eq!(doc.set("", "x"), Err(ConfError::KeyInvalid));
    let long: String = core::iter::repeat_n('x', crate::MAX_VALUE_LEN + 1).collect();
    assert_eq!(doc.set("key", &long), Err(ConfError::ValueInvalid));
    assert_eq!(doc.set("key", "nul\0byte"), Err(ConfError::ValueInvalid));
    assert_eq!(
        doc.render(),
        "",
        "a refused write leaves the document alone"
    );
}

/// The document-level bounds are fixed bounds on untrusted input: they fail
/// closed rather than reading a hostile store in part.
#[test]
fn the_document_bounds_fail_closed() {
    let too_long: String = core::iter::repeat_n('x', MAX_DOCUMENT_LEN + 1).collect();
    assert_eq!(
        Document::parse(&too_long).err(),
        Some(ConfError::DocumentTooLarge)
    );

    let mut many_lines = String::new();
    for _ in 0..=MAX_LINES {
        many_lines.push('\n');
    }
    assert_eq!(
        Document::parse(&many_lines).err(),
        Some(ConfError::TooManyLines)
    );

    let mut many_settings = String::new();
    for index in 0..=MAX_SETTINGS {
        let _ = core::fmt::Write::write_fmt(&mut many_settings, format_args!("k{index} = v\n"));
    }
    assert_eq!(
        Document::parse(&many_settings).err(),
        Some(ConfError::TooManySettings)
    );
}

#[test]
fn a_write_refuses_to_grow_past_the_settings_bound() {
    let mut doc = Document::new();
    for index in 0..MAX_SETTINGS {
        let mut key = String::from("k");
        let _ = core::fmt::Write::write_fmt(&mut key, format_args!("{index}"));
        doc.set(&key, "v").expect("within the bound");
    }
    assert_eq!(
        doc.set("one.too.many", "v"),
        Err(ConfError::TooManySettings)
    );
    // Rewriting a key the document already holds is always allowed: it adds
    // no setting.
    assert_eq!(doc.set("k0", "w"), Ok(()));
    assert_eq!(doc.get("k0"), Some("w"));
}

#[test]
fn a_document_without_a_final_newline_keeps_it_that_way() {
    let doc = Document::parse("scheme = dark").expect("parses");
    assert_eq!(doc.render(), "scheme = dark");
    let mut doc = Document::parse("scheme = dark").expect("parses");
    doc.set("scheme", "light").expect("sets");
    assert_eq!(doc.render(), "scheme = light");
}

/// Appending a line does terminate it: the new last line is the engine's own,
/// and a file it wrote ends with a newline.
#[test]
fn appending_to_an_unterminated_document_terminates_it() {
    let mut doc = Document::parse("scheme = dark").expect("parses");
    doc.set("font.size", "14").expect("sets");
    assert_eq!(
        doc.render(),
        "scheme = dark
font.size = 14
"
    );
}

#[test]
fn an_empty_and_a_newline_only_document_both_round_trip() {
    for text in ["", "\n", "\n\n"] {
        let doc = Document::parse(text).expect("parses");
        assert_eq!(doc.render(), text, "`{text:?}` must round-trip");
    }
}

/// A line the engine discards may be a sealed-scope secret, so its bytes are
/// overwritten rather than merely dropped. `Drop` calls this, which is what
/// makes every discard path — an overwrite, a collapsed duplicate, an `unset`,
/// a whole document going out of scope — covered without a call site
/// remembering to.
#[test]
fn wiping_a_line_overwrites_every_byte_it_holds() {
    let mut line = super::render_setting("imap.password", "hunter2", " # work account");
    assert!(line.text.contains("hunter2"));
    line.wipe();
    assert!(line.text.is_empty(), "the rendered text is gone");
    match &line.kind {
        super::Kind::Setting {
            key,
            value,
            comment,
        } => {
            assert!(key.is_empty() && value.is_empty() && comment.is_empty());
        }
        _ => panic!("a rendered setting stays a setting"),
    }
}

/// A wipe is idempotent and safe on a line carrying no setting, so `Drop`
/// after an explicit wipe — and a wipe of an inert or unparsed line — cannot
/// misbehave.
#[test]
fn wiping_is_idempotent_and_covers_every_line_kind() {
    let mut doc = Document::parse("# a comment\nnot a setting\nscheme = dark\n").expect("parses");
    doc.unset("scheme");
    assert_eq!(doc.render(), "# a comment\nnot a setting\n");
    let mut line = super::render_setting("k", "v", "");
    line.wipe();
    line.wipe();
    assert!(line.text.is_empty());
}
