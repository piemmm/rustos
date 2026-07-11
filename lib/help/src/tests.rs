//! Unit tests: locale/name validation, the fallback chain, the bounded
//! parser (happy path and every rejection), and the two renderers.

use alloc::borrow::ToOwned;
use alloc::string::String;
use alloc::vec;
use alloc::vec::Vec;

use rustos_vt::{encode_all_into, Op};

/// Encode a sequence of operations into a fresh `Vec` over the sink API.
fn encode_all(ops: &[Op]) -> Vec<u8> {
    let mut out = Vec::new();
    encode_all_into(ops, &mut out);
    out
}

use crate::{
    load, load_raw, render_full, render_short, Align, Block, DocumentName, Fallback, HelpDoc,
    HelpError, HelpSource, LoadError, Locale, NameError, SectionKind, SourceError, Span, TagError,
    MAX_DOC_LEN, MAX_LINES, MAX_LIST_ITEMS, MAX_LOCALE_DIRS, MAX_TABLE_ROWS,
};

/// A minimal valid document.
const MINIMAL: &str = "## NAME\n\ntop — display tasks\n\n## SYNOPSIS\n\n`top [-d seconds]`\n\n## DESCRIPTION\n\nShows tasks.\n";

/// An in-memory `HelpSource`: `(locale_dir, file_name, bytes)` triples.
struct MapSource {
    entries: Vec<(String, String, Vec<u8>)>,
    fail: bool,
}

impl MapSource {
    fn new(entries: &[(&str, &str)]) -> Self {
        MapSource {
            entries: entries
                .iter()
                .map(|(dir, file)| ((*dir).to_owned(), (*file).to_owned(), MINIMAL.into()))
                .collect(),
            fail: false,
        }
    }
}

impl HelpSource for MapSource {
    fn locale_dirs(&self) -> Result<Vec<String>, SourceError> {
        if self.fail {
            return Err(SourceError);
        }
        let mut dirs: Vec<String> = self.entries.iter().map(|(dir, ..)| dir.clone()).collect();
        dirs.dedup();
        Ok(dirs)
    }

    fn read(&self, locale_dir: &str, file_name: &str) -> Result<Option<Vec<u8>>, SourceError> {
        if self.fail {
            return Err(SourceError);
        }
        Ok(self
            .entries
            .iter()
            .find(|(dir, file, _)| dir == locale_dir && file == file_name)
            .map(|(.., bytes)| bytes.clone()))
    }
}

fn name(text: &str) -> DocumentName {
    DocumentName::parse(text).expect("valid name")
}

fn locale(tag: &str) -> Locale {
    Locale::parse(tag).expect("valid locale")
}

/// Collect the plain text of an op sequence (prints and line feeds only).
fn plain(ops: &[Op]) -> String {
    let mut out = String::new();
    for op in ops {
        match op {
            Op::Print(ch) => out.push(*ch),
            Op::LineFeed => out.push('\n'),
            _ => {}
        }
    }
    out
}

#[test]
fn locale_accepts_the_grammar_and_nothing_else() {
    for tag in ["fr", "fr-FR", "es-419", "en-US", "haw-US"] {
        assert_eq!(locale(tag).as_str(), tag);
    }
    assert_eq!(locale("fr-FR").language(), "fr");
    assert_eq!(locale("es-419").language(), "es");
    assert!(locale("en-US").is_default());
    assert_eq!(Locale::default(), locale("en-US"));
    assert!(!locale("fr").is_default());
    assert!(!locale("en-GB").is_default());

    assert_eq!(Locale::parse(""), Err(TagError::Empty));
    assert_eq!(Locale::parse("verylonglocale"), Err(TagError::TooLong));
    for tag in [
        "f", "FR", "fr-fr", "fr_FR", "fr-FRA", "fr-41", "fr-4A9", "-FR", "fr-",
    ] {
        assert_eq!(Locale::parse(tag), Err(TagError::Malformed), "tag {tag}");
    }
}

#[test]
fn document_name_makes_traversal_unrepresentable() {
    assert_eq!(name("top").as_str(), "top");
    assert_eq!(name("disk-tool_2").file_name(), "disk-tool_2.md");

    assert_eq!(DocumentName::parse(""), Err(NameError::Empty));
    let long: String = core::iter::repeat_n('a', 65).collect();
    assert_eq!(DocumentName::parse(&long), Err(NameError::TooLong));
    for bad in ["../etc", ".hidden", "-flag", "_x", "a/b", "a.b", "a b", "é"] {
        assert_eq!(
            DocumentName::parse(bad),
            Err(NameError::Malformed),
            "name {bad}"
        );
    }
}

#[test]
fn load_serves_the_exact_locale_first() {
    let source = MapSource::new(&[("fr-FR", "top.md"), ("en-US", "top.md")]);
    let loaded = load(&source, &locale("fr-FR"), &name("top")).expect("loads");
    assert_eq!(loaded.selection.locale_dir, "fr-FR");
    assert_eq!(loaded.selection.fallback, Fallback::Exact);
}

#[test]
fn load_raw_serves_the_same_selection_with_unparsed_bytes() {
    // One locale walk serves both entry points: `load_raw` must pick the
    // same document `load` picks, returning its bytes untouched.
    let source = MapSource::new(&[("fr-FR", "top.md"), ("en-US", "top.md")]);
    let raw = load_raw(&source, &locale("fr-CH"), &name("top")).expect("loads");
    assert_eq!(raw.selection.locale_dir, "fr-FR");
    assert_eq!(raw.selection.fallback, Fallback::SameLanguage);
    assert_eq!(raw.bytes, MINIMAL.as_bytes());
}

#[test]
fn load_raw_does_not_parse_but_still_bounds_the_size() {
    // A document `load` would refuse as malformed passes `load_raw` whole
    // (the parse runs elsewhere, in the sandbox) — but the size bound is
    // enforced before any bytes are returned.
    let mut source = MapSource::new(&[]);
    source.entries.push((
        "en-US".to_owned(),
        "top.md".to_owned(),
        b"not a help document".to_vec(),
    ));
    let raw = load_raw(&source, &locale("en-US"), &name("top")).expect("loads");
    assert_eq!(raw.bytes, b"not a help document");

    let mut oversize = MapSource::new(&[]);
    oversize.entries.push((
        "en-US".to_owned(),
        "top.md".to_owned(),
        vec![b'a'; MAX_DOC_LEN + 1],
    ));
    assert_eq!(
        load_raw(&oversize, &locale("en-US"), &name("top")).unwrap_err(),
        LoadError::Document(HelpError::TooLarge)
    );
}

#[test]
fn load_falls_back_to_the_first_same_language_region_with_the_document() {
    // fr-BE lacks top.md, so the lexicographically next fr region serves it.
    let source = MapSource::new(&[
        ("fr-BE", "other.md"),
        ("fr-FR", "top.md"),
        ("fr-CA", "top.md"),
        ("en-US", "top.md"),
    ]);
    let loaded = load(&source, &locale("fr-CH"), &name("top")).expect("loads");
    assert_eq!(loaded.selection.locale_dir, "fr-CA");
    assert_eq!(loaded.selection.fallback, Fallback::SameLanguage);
}

#[test]
fn load_falls_back_to_default_and_reports_it() {
    let source = MapSource::new(&[("de-DE", "top.md"), ("en-US", "top.md")]);
    let loaded = load(&source, &locale("fr-FR"), &name("top")).expect("loads");
    assert_eq!(loaded.selection.locale_dir, "en-US");
    assert_eq!(loaded.selection.fallback, Fallback::Default);
}

#[test]
fn load_of_default_is_exact_not_a_fallback() {
    let source = MapSource::new(&[("en-US", "top.md")]);
    let loaded = load(&source, &locale("en-US"), &name("top")).expect("loads");
    assert_eq!(loaded.selection.fallback, Fallback::Exact);
}

#[test]
fn load_of_another_english_region_reports_the_canonical_fallback() {
    // en-GB falls through the same-language step (the canonical en-US/ is
    // deliberately excluded there) to the canonical document, reported as
    // Fallback::Default so a caller can surface the substitution.
    let source = MapSource::new(&[("en-US", "top.md"), ("de-DE", "top.md")]);
    let loaded = load(&source, &locale("en-GB"), &name("top")).expect("loads");
    assert_eq!(loaded.selection.locale_dir, "en-US");
    assert_eq!(loaded.selection.fallback, Fallback::Default);
}

#[test]
fn load_fails_closed() {
    let source = MapSource::new(&[("en-US", "other.md")]);
    assert_eq!(
        load(&source, &locale("fr-FR"), &name("top")).unwrap_err(),
        LoadError::NotFound
    );

    let mut failing = MapSource::new(&[("en-US", "top.md")]);
    failing.fail = true;
    assert_eq!(
        load(&failing, &locale("fr-FR"), &name("top")).unwrap_err(),
        LoadError::Source(SourceError)
    );

    let mut source = MapSource::new(&[]);
    for index in 0..=MAX_LOCALE_DIRS {
        source.entries.push((
            alloc::format!("xx-{index:03}"),
            "other.md".to_owned(),
            Vec::new(),
        ));
    }
    assert_eq!(
        load(&source, &locale("fr-FR"), &name("top")).unwrap_err(),
        LoadError::TooManyLocales
    );

    let mut oversize = MapSource::new(&[]);
    oversize.entries.push((
        "en-US".to_owned(),
        "top.md".to_owned(),
        vec![b'a'; MAX_DOC_LEN + 1],
    ));
    assert_eq!(
        load(&oversize, &locale("en-US"), &name("top")).unwrap_err(),
        LoadError::Document(HelpError::TooLarge)
    );
}

#[test]
fn load_ignores_alien_locale_directory_names() {
    let source = MapSource::new(&[("..", "top.md"), ("fr-FR", "top.md"), ("en-US", "top.md")]);
    let loaded = load(&source, &locale("fr-CH"), &name("top")).expect("loads");
    assert_eq!(loaded.selection.locale_dir, "fr-FR");
}

#[test]
fn parse_accepts_a_full_document() {
    let text = "## NAME\n\ntop — display tasks\n\n\
                ## SYNOPSIS\n\n```\ntop [-d seconds]\n```\n\n\
                ## DESCRIPTION\n\nFirst line\nsecond line.\n\n### Refresh\n\nMore *detail* here.\n\n\
                ## OPTIONS\n\n- `-d, --delay <seconds>` — refresh delay\n  continued description\n- `-h, -?` — short help\n\n\
                ## EXAMPLES\n\n1. run it\n2. read it\n\n\
                ## EXIT STATUS\n\n| Code | Meaning |\n|------|---------|\n| 0 | ok |\n\n\
                ## ENVIRONMENT\n\nNone.\n\n\
                ## SEE ALSO\n\n`ps`\n";
    let doc = HelpDoc::parse(text.as_bytes()).expect("parses");
    assert_eq!(doc.sections().len(), 8);

    let description = doc.section(SectionKind::Description).expect("description");
    assert_eq!(
        description.blocks.first(),
        Some(&Block::Paragraph(vec![Span::Text(
            "First line second line.".into()
        )]))
    );

    let options = doc.section(SectionKind::Options).expect("options");
    let Some(Block::List { ordered, items }) = options.blocks.first() else {
        panic!("options list expected");
    };
    assert!(!ordered);
    assert_eq!(items.len(), 2);
    assert_eq!(
        items.first().map(|item| item.spans.clone()),
        Some(vec![
            Span::Code("-d, --delay <seconds>".into()),
            Span::Text(" — refresh delay continued description".into()),
        ])
    );

    let examples = doc.section(SectionKind::Examples).expect("examples");
    assert!(matches!(
        examples.blocks.first(),
        Some(Block::List { ordered: true, items }) if items.len() == 2
    ));

    let exit = doc.section(SectionKind::ExitStatus).expect("exit status");
    let Some(Block::Table(table)) = exit.blocks.first() else {
        panic!("table expected");
    };
    assert_eq!(table.header.len(), 2);
    assert_eq!(table.alignments, vec![Align::Left, Align::Left]);
    assert_eq!(table.rows.len(), 1);
}

#[test]
fn parse_keeps_heading_lines_inside_fences_as_code() {
    let text = "## NAME\n\nx — y\n\n## SYNOPSIS\n\nusage\n\n## DESCRIPTION\n\n\
                ```markdown\n## NAME\n# anything\n```\n";
    let doc = HelpDoc::parse(text.as_bytes()).expect("parses");
    let description = doc.section(SectionKind::Description).expect("description");
    assert_eq!(
        description.blocks.first(),
        Some(&Block::CodeBlock {
            info: "markdown".into(),
            lines: vec!["## NAME".into(), "# anything".into()],
        })
    );
}

#[test]
fn parse_rejects_size_and_shape_violations() {
    assert_eq!(
        HelpDoc::parse(&vec![b'a'; MAX_DOC_LEN + 1]).unwrap_err(),
        HelpError::TooLarge
    );
    assert_eq!(
        HelpDoc::parse(&[0xFF, 0xFE]).unwrap_err(),
        HelpError::InvalidUtf8
    );
    let long_line = core::iter::repeat_n('a', 513).collect::<String>();
    assert_eq!(
        HelpDoc::parse(long_line.as_bytes()).unwrap_err(),
        HelpError::LineTooLong
    );
    let many_lines = core::iter::repeat_n('\n', MAX_LINES).collect::<String>();
    assert_eq!(
        HelpDoc::parse(many_lines.as_bytes()).unwrap_err(),
        HelpError::TooManyLines
    );
    assert_eq!(
        HelpDoc::parse(b"## NAME\r\n").unwrap_err(),
        HelpError::ControlCharacter
    );
    assert_eq!(
        HelpDoc::parse(b"text before\n## NAME\n").unwrap_err(),
        HelpError::ContentBeforeFirstSection
    );
    assert_eq!(
        HelpDoc::parse(b"```\nfence before\n```\n## NAME\n").unwrap_err(),
        HelpError::ContentBeforeFirstSection
    );
}

#[test]
fn parse_rejects_heading_defects() {
    assert_eq!(
        HelpDoc::parse(b"## WRONG\n").unwrap_err(),
        HelpError::UnknownHeading
    );
    assert_eq!(
        HelpDoc::parse(b"## NAME\n# title\n").unwrap_err(),
        HelpError::UnknownHeading
    );
    assert_eq!(
        HelpDoc::parse(b"## NAME\n#### deep\n").unwrap_err(),
        HelpError::UnknownHeading
    );
    assert_eq!(
        HelpDoc::parse(b"## NAME\nx\n## NAME\nx\n").unwrap_err(),
        HelpError::DuplicateSection
    );
    assert_eq!(
        HelpDoc::parse(b"## SYNOPSIS\nx\n## NAME\nx\n").unwrap_err(),
        HelpError::SectionOutOfOrder
    );
    assert_eq!(
        HelpDoc::parse(b"## NAME\nx\n## SYNOPSIS\nx\n").unwrap_err(),
        HelpError::MissingSection(SectionKind::Description)
    );
    assert_eq!(
        HelpDoc::parse(b"## NAME\n\n## SYNOPSIS\nx\n## DESCRIPTION\nx\n").unwrap_err(),
        HelpError::EmptySection(SectionKind::Name)
    );
}

#[test]
fn parse_rejects_malformed_blocks() {
    let prefix = "## NAME\nx\n## SYNOPSIS\nx\n## DESCRIPTION\n";
    let parse = |body: &str| {
        let mut text = String::from(prefix);
        text.push_str(body);
        HelpDoc::parse(text.as_bytes()).unwrap_err()
    };

    assert_eq!(parse("```\nnever closed\n"), HelpError::UnterminatedFence);
    assert_eq!(parse("| lone row |\n"), HelpError::MalformedTable);
    assert_eq!(parse("| a |\n| missing pipe\n"), HelpError::MalformedTable);
    assert_eq!(parse("| a | b |\n|---|\n"), HelpError::MalformedTable);
    assert_eq!(parse("| a |\n|--|\n"), HelpError::MalformedTable);
    assert_eq!(
        parse("| a |\n|---|\n| b | c |\n"),
        HelpError::MalformedTable
    );
    assert_eq!(
        parse("  orphan continuation\n"),
        HelpError::OrphanContinuation
    );

    let mut items = String::new();
    for _ in 0..=MAX_LIST_ITEMS {
        items.push_str("- item\n");
    }
    assert_eq!(parse(&items), HelpError::TooManyItems);

    let wide = "| a | b | c | d | e | f | g | h | i |\n";
    assert_eq!(parse(wide), HelpError::TableTooLarge);

    let mut tall = String::from("| a |\n|---|\n");
    for _ in 0..=MAX_TABLE_ROWS {
        tall.push_str("| r |\n");
    }
    assert_eq!(parse(&tall), HelpError::TableTooLarge);

    let mut blocks = String::new();
    for _ in 0..=256 {
        blocks.push_str("para\n\n");
    }
    assert_eq!(parse(&blocks), HelpError::TooManyBlocks);
}

#[test]
fn spans_parse_markdown_inline_markers() {
    let doc = HelpDoc::parse(
        "## NAME\n\na `code` **strong** *em* \\*escaped\\* un`closed and lone ** stars\n\n\
         ## SYNOPSIS\nx\n## DESCRIPTION\nx\n"
            .as_bytes(),
    )
    .expect("parses");
    let name_section = doc.section(SectionKind::Name).expect("name");
    assert_eq!(
        name_section.blocks.first(),
        Some(&Block::Paragraph(vec![
            Span::Text("a ".into()),
            Span::Code("code".into()),
            Span::Text(" ".into()),
            Span::Strong("strong".into()),
            Span::Text(" ".into()),
            Span::Emphasis("em".into()),
            Span::Text(" *escaped* un`closed and lone ** stars".into()),
        ]))
    );
}

#[test]
fn short_render_is_compact_and_full_render_is_complete() {
    let doc = HelpDoc::parse(
        "## NAME\n\ntop — display tasks\n\n## SYNOPSIS\n\n```\ntop [-d seconds]\n```\n\n\
         ## DESCRIPTION\n\nLong body.\n\n## OPTIONS\n\n- `-h` — short help\n"
            .as_bytes(),
    )
    .expect("parses");

    let short = plain(&render_short(&doc));
    assert!(short.contains("top — display tasks"));
    assert!(short.contains("top [-d seconds]"));
    assert!(short.contains("-h"));
    assert!(!short.contains("Long body."));
    assert!(!short.contains("DESCRIPTION"));

    let full = plain(&render_full(&doc));
    for heading in ["NAME", "SYNOPSIS", "DESCRIPTION", "OPTIONS"] {
        assert!(full.contains(heading), "missing {heading}");
    }
    assert!(full.contains("Long body."));
}

#[test]
fn full_render_pads_tables_and_numbers_ordered_lists() {
    let doc = HelpDoc::parse(
        "## NAME\nx\n## SYNOPSIS\nx\n## DESCRIPTION\n\n\
         1. first\n2. second\n\n\
         | Code | Meaning |\n|-----:|---------|\n| 0 | ok |\n"
            .as_bytes(),
    )
    .expect("parses");
    let full = plain(&render_full(&doc));
    assert!(full.contains("  1. first\n  2. second\n"));
    assert!(full.contains("  Code  Meaning\n"));
    assert!(full.contains("  ----  -------\n"));
    assert!(full.contains("     0  ok"));
}

#[test]
fn rendered_output_contains_no_stray_control_bytes() {
    let doc = HelpDoc::parse(MINIMAL.as_bytes()).expect("parses");
    for ops in [render_short(&doc), render_full(&doc)] {
        let bytes = encode_all(&ops);
        let mut rest = bytes.as_slice();
        while let Some((byte, tail)) = rest.split_first() {
            match byte {
                0x1B => {
                    // A well-formed CSI sequence ends at its final byte.
                    let end = tail
                        .iter()
                        .position(|b| (0x40..=0x7E).contains(b))
                        .expect("terminated escape");
                    rest = tail.get(end + 1..).unwrap_or_default();
                }
                b'\n' => rest = tail,
                byte => {
                    assert!(!byte.is_ascii_control(), "stray control byte {byte:#x}");
                    rest = tail;
                }
            }
        }
    }
}
