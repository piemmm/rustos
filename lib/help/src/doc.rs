//! The fixed help-document model and its bounded structured-Markdown parser.
//!
//! One help document describes one command or topic. Its shape is fixed
//! (`plans/APPS.md`): a closed, ordered set of level-2 sections whose keys
//! are language-neutral (`## NAME`, `## SYNOPSIS`, …) while the prose under
//! them is localised. Fixing the keys is what lets one parser serve every
//! language, and fixing the order is what makes drift between a document
//! and this model detectable instead of silently tolerated.
//!
//! Parsing is total and bounded. Every dimension of the input carries a
//! hard, fixed validation bound (document size, line count/length, blocks,
//! list items, table rows/columns) and any violation, malformed structure,
//! control byte, or invalid UTF-8 is a typed [`HelpError`] — the document is
//! rejected whole, never partially applied or "fixed up".

use alloc::string::String;
use alloc::vec::Vec;
use core::fmt;

/// Maximum byte length of one document.
pub const MAX_DOC_LEN: usize = 64 * 1024;

/// Maximum byte length of one line.
pub const MAX_LINE_LEN: usize = 512;

/// Maximum number of lines in one document.
pub const MAX_LINES: usize = 4096;

/// Maximum number of blocks in one section.
pub const MAX_BLOCKS_PER_SECTION: usize = 256;

/// Maximum number of items in one list.
pub const MAX_LIST_ITEMS: usize = 128;

/// Maximum number of columns in one table.
pub const MAX_TABLE_COLUMNS: usize = 8;

/// Maximum number of data rows in one table.
pub const MAX_TABLE_ROWS: usize = 128;

/// The closed, ordered set of section keys a help document may contain.
///
/// The key is written in the document verbatim (`## NAME`), never
/// translated; only the prose under it is localised.
#[derive(Copy, Clone, Debug, Eq, PartialEq, Hash)]
pub enum SectionKind {
    /// Command name plus one-line summary. Required.
    Name,
    /// Usage line(s); option/argument grammar. Required.
    Synopsis,
    /// Full behaviour — the `man` body. Required.
    Description,
    /// One entry per command-line switch.
    Options,
    /// Worked examples.
    Examples,
    /// Meaning of exit codes.
    ExitStatus,
    /// Environment variables consulted.
    Environment,
    /// Related commands, by command name.
    SeeAlso,
}

impl SectionKind {
    /// Every section kind, in the canonical document order.
    pub const ALL: [SectionKind; 8] = [
        SectionKind::Name,
        SectionKind::Synopsis,
        SectionKind::Description,
        SectionKind::Options,
        SectionKind::Examples,
        SectionKind::ExitStatus,
        SectionKind::Environment,
        SectionKind::SeeAlso,
    ];

    /// The verbatim heading key (`"NAME"`, `"EXIT STATUS"`, …).
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            SectionKind::Name => "NAME",
            SectionKind::Synopsis => "SYNOPSIS",
            SectionKind::Description => "DESCRIPTION",
            SectionKind::Options => "OPTIONS",
            SectionKind::Examples => "EXAMPLES",
            SectionKind::ExitStatus => "EXIT STATUS",
            SectionKind::Environment => "ENVIRONMENT",
            SectionKind::SeeAlso => "SEE ALSO",
        }
    }

    /// Classify a heading key. Exact and case-sensitive.
    #[must_use]
    pub fn from_heading(key: &str) -> Option<Self> {
        Self::ALL.into_iter().find(|kind| kind.as_str() == key)
    }

    /// The section heading *displayed* to a reader, in `locale`'s language.
    ///
    /// The document key ([`Self::as_str`]) is language-neutral and never
    /// translated — that is what lets one parser serve every language — but a
    /// reader sees the heading in the language of the document they are shown.
    /// A served `fr-FR` page therefore shows `DESCRIPTION` as `DESCRIPTION` in
    /// French prose under a `NOM` heading, not an English `NAME`. Selection is
    /// by primary language subtag (`fr` of `fr-FR`); a language without a
    /// translation here degrades to the canonical English key, never to a
    /// blank or a fabricated word.
    #[must_use]
    pub fn heading_label(self, locale: &crate::locale::Locale) -> &'static str {
        let table: &[&'static str; 8] = match locale.language() {
            "fr" => &HEADINGS_FR,
            "de" => &HEADINGS_DE,
            "es" => &HEADINGS_ES,
            "uk" => &HEADINGS_UK,
            "it" => &HEADINGS_IT,
            "pt" => &HEADINGS_PT,
            "cy" => &HEADINGS_CY,
            "zh" => &HEADINGS_ZH,
            "ja" => &HEADINGS_JA,
            "ko" => &HEADINGS_KO,
            "ar" => &HEADINGS_AR,
            "he" => &HEADINGS_HE,
            _ => return self.as_str(),
        };
        table.get(self.rank()).copied().unwrap_or(self.as_str())
    }

    /// Whether every document must carry this section.
    #[must_use]
    pub const fn is_required(self) -> bool {
        matches!(
            self,
            SectionKind::Name | SectionKind::Synopsis | SectionKind::Description
        )
    }

    /// Position in the canonical order.
    fn rank(self) -> usize {
        Self::ALL
            .into_iter()
            .position(|kind| kind == self)
            .unwrap_or(Self::ALL.len())
    }
}

// The displayed section headings per language, in [`SectionKind::ALL`] order:
// NAME, SYNOPSIS, DESCRIPTION, OPTIONS, EXAMPLES, EXIT STATUS, ENVIRONMENT,
// SEE ALSO. English is the canonical [`SectionKind::as_str`] key itself, so it
// needs no table. These are the display labels a reader sees, following the
// conventional section names of each language's manual pages; the document
// keys stay the untranslated English of `## NAME`.
const HEADINGS_FR: [&str; 8] = [
    "NOM",
    "SYNOPSIS",
    "DESCRIPTION",
    "OPTIONS",
    "EXEMPLES",
    "ÉTAT DE SORTIE",
    "ENVIRONNEMENT",
    "VOIR AUSSI",
];
const HEADINGS_DE: [&str; 8] = [
    "BEZEICHNUNG",
    "ÜBERSICHT",
    "BESCHREIBUNG",
    "OPTIONEN",
    "BEISPIELE",
    "EXIT-STATUS",
    "UMGEBUNG",
    "SIEHE AUCH",
];
const HEADINGS_ES: [&str; 8] = [
    "NOMBRE",
    "SINOPSIS",
    "DESCRIPCIÓN",
    "OPCIONES",
    "EJEMPLOS",
    "ESTADO DE SALIDA",
    "ENTORNO",
    "VÉASE TAMBIÉN",
];
const HEADINGS_UK: [&str; 8] = [
    "НАЗВА",
    "КОРОТКИЙ ОПИС",
    "ОПИС",
    "ПАРАМЕТРИ",
    "ПРИКЛАДИ",
    "СТАН ВИХОДУ",
    "СЕРЕДОВИЩЕ",
    "ДИВ. ТАКОЖ",
];
const HEADINGS_IT: [&str; 8] = [
    "NOME",
    "SINTASSI",
    "DESCRIZIONE",
    "OPZIONI",
    "ESEMPI",
    "STATO DI USCITA",
    "AMBIENTE",
    "VEDERE ANCHE",
];
const HEADINGS_PT: [&str; 8] = [
    "NOME",
    "SINOPSE",
    "DESCRIÇÃO",
    "OPÇÕES",
    "EXEMPLOS",
    "ESTADO DE SAÍDA",
    "AMBIENTE",
    "VEJA TAMBÉM",
];
const HEADINGS_CY: [&str; 8] = [
    "ENW",
    "CRYNODEB",
    "DISGRIFIAD",
    "DEWISIADAU",
    "ENGHREIFFTIAU",
    "STATWS GADAEL",
    "AMGYLCHEDD",
    "GWELER HEFYD",
];
const HEADINGS_ZH: [&str; 8] = [
    "名称",
    "总览",
    "描述",
    "选项",
    "示例",
    "退出状态",
    "环境",
    "参见",
];
const HEADINGS_JA: [&str; 8] = [
    "名前",
    "書式",
    "説明",
    "オプション",
    "例",
    "終了ステータス",
    "環境変数",
    "関連項目",
];
const HEADINGS_KO: [&str; 8] = [
    "이름",
    "요약",
    "설명",
    "옵션",
    "예제",
    "종료 상태",
    "환경",
    "관련 항목",
];
const HEADINGS_AR: [&str; 8] = [
    "الاسم",
    "ملخص",
    "الوصف",
    "الخيارات",
    "أمثلة",
    "حالة الخروج",
    "البيئة",
    "انظر أيضا",
];
const HEADINGS_HE: [&str; 8] = [
    "שם",
    "תקציר",
    "תיאור",
    "אפשרויות",
    "דוגמאות",
    "קוד יציאה",
    "סביבה",
    "ראה גם",
];

/// One inline span of styled text.
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum Span {
    /// Plain text.
    Text(String),
    /// An inline code span (`` `-d, --delay` ``) — the language-neutral
    /// switch keys live in these.
    Code(String),
    /// Strong emphasis (`**bold**`).
    Strong(String),
    /// Emphasis (`*italic*`).
    Emphasis(String),
}

/// One list item: its inline spans.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ListItem {
    /// The item's inline content.
    pub spans: Vec<Span>,
}

/// Column alignment declared by a table's delimiter row.
#[derive(Copy, Clone, Debug, Eq, PartialEq)]
pub enum Align {
    /// `---` or `:---`.
    Left,
    /// `:---:`.
    Center,
    /// `---:`.
    Right,
}

/// A parsed table: header cells, per-column alignment, and data rows, all
/// with the same column count.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct Table {
    /// Header cells, one per column.
    pub header: Vec<Vec<Span>>,
    /// Per-column alignment from the delimiter row.
    pub alignments: Vec<Align>,
    /// Data rows; every row has exactly `header.len()` cells.
    pub rows: Vec<Vec<Vec<Span>>>,
}

/// One block of section content.
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum Block {
    /// A paragraph: consecutive plain lines joined by single spaces.
    Paragraph(Vec<Span>),
    /// A level-3 sub-heading (`### …`) inside a section.
    SubHeading(Vec<Span>),
    /// A bullet (`- `) or ordered (`1. `) list.
    List {
        /// `true` for `1.`-numbered items, `false` for bullets.
        ordered: bool,
        /// The items, in document order.
        items: Vec<ListItem>,
    },
    /// A fenced code block. Lines are verbatim, never span-parsed.
    CodeBlock {
        /// The fence info string (`markdown` of ```` ```markdown ````),
        /// possibly empty.
        info: String,
        /// The verbatim lines between the fences.
        lines: Vec<String>,
    },
    /// A pipe table.
    Table(Table),
}

/// One parsed section: its kind and its blocks, in document order.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct Section {
    /// Which section this is.
    pub kind: SectionKind,
    /// The section's content blocks.
    pub blocks: Vec<Block>,
}

/// A parsed help document: its sections in canonical order.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct HelpDoc {
    sections: Vec<Section>,
}

impl HelpDoc {
    /// Parse one help document, whole and fail-closed.
    ///
    /// Every bound violation, structural defect, control byte, or invalid
    /// UTF-8 rejects the entire document with a typed [`HelpError`]; nothing
    /// is skipped, repaired, or partially applied.
    pub fn parse(bytes: &[u8]) -> Result<Self, HelpError> {
        if bytes.len() > MAX_DOC_LEN {
            return Err(HelpError::TooLarge);
        }
        let text = core::str::from_utf8(bytes).map_err(|_| HelpError::InvalidUtf8)?;
        if text.chars().any(|c| c.is_control() && c != '\n') {
            return Err(HelpError::ControlCharacter);
        }
        let lines = split_lines(text)?;
        let sections = parse_sections(&lines)?;
        for kind in SectionKind::ALL {
            if !kind.is_required() {
                continue;
            }
            match sections.iter().find(|section| section.kind == kind) {
                None => return Err(HelpError::MissingSection(kind)),
                Some(section) if section.blocks.is_empty() => {
                    return Err(HelpError::EmptySection(kind));
                }
                Some(_) => {}
            }
        }
        Ok(HelpDoc { sections })
    }

    /// The sections present, in canonical order.
    #[must_use]
    pub fn sections(&self) -> &[Section] {
        &self.sections
    }

    /// The section of `kind`, if the document carries it.
    #[must_use]
    pub fn section(&self, kind: SectionKind) -> Option<&Section> {
        self.sections.iter().find(|section| section.kind == kind)
    }
}

/// Why a document was rejected.
#[derive(Copy, Clone, Debug, Eq, PartialEq)]
pub enum HelpError {
    /// The document exceeds [`MAX_DOC_LEN`] bytes.
    TooLarge,
    /// The document is not valid UTF-8.
    InvalidUtf8,
    /// A line exceeds [`MAX_LINE_LEN`] bytes.
    LineTooLong,
    /// The document exceeds [`MAX_LINES`] lines.
    TooManyLines,
    /// A control character other than `\n` appears in the document.
    ControlCharacter,
    /// Non-blank content appears before the first `## ` section heading.
    ContentBeforeFirstSection,
    /// A `## ` heading key is not one of [`SectionKind::ALL`], or a heading
    /// of another level (`# `, `#### `) appears.
    UnknownHeading,
    /// A section key appears twice.
    DuplicateSection,
    /// Sections appear out of the canonical order.
    SectionOutOfOrder,
    /// A required section (`NAME`, `SYNOPSIS`, `DESCRIPTION`) is absent.
    MissingSection(SectionKind),
    /// A required section is present but has no content.
    EmptySection(SectionKind),
    /// A fenced code block is never closed.
    UnterminatedFence,
    /// A table has no delimiter row, mismatched column counts, or a
    /// malformed delimiter cell.
    MalformedTable,
    /// A section exceeds [`MAX_BLOCKS_PER_SECTION`] blocks.
    TooManyBlocks,
    /// A list exceeds [`MAX_LIST_ITEMS`] items.
    TooManyItems,
    /// A table exceeds [`MAX_TABLE_COLUMNS`] columns or [`MAX_TABLE_ROWS`]
    /// rows.
    TableTooLarge,
    /// A list continuation line (indented) appears with no open list item.
    OrphanContinuation,
}

impl fmt::Display for HelpError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            HelpError::TooLarge => f.write_str("document is too large"),
            HelpError::InvalidUtf8 => f.write_str("document is not valid UTF-8"),
            HelpError::LineTooLong => f.write_str("line is too long"),
            HelpError::TooManyLines => f.write_str("document has too many lines"),
            HelpError::ControlCharacter => f.write_str("document contains a control character"),
            HelpError::ContentBeforeFirstSection => {
                f.write_str("content appears before the first section heading")
            }
            HelpError::UnknownHeading => f.write_str("heading is not a known section key"),
            HelpError::DuplicateSection => f.write_str("section appears twice"),
            HelpError::SectionOutOfOrder => f.write_str("sections are out of canonical order"),
            HelpError::MissingSection(kind) => {
                f.write_str("required section is missing: ")?;
                f.write_str(kind.as_str())
            }
            HelpError::EmptySection(kind) => {
                f.write_str("required section is empty: ")?;
                f.write_str(kind.as_str())
            }
            HelpError::UnterminatedFence => f.write_str("code fence is never closed"),
            HelpError::MalformedTable => f.write_str("table is malformed"),
            HelpError::TooManyBlocks => f.write_str("section has too many blocks"),
            HelpError::TooManyItems => f.write_str("list has too many items"),
            HelpError::TableTooLarge => f.write_str("table is too large"),
            HelpError::OrphanContinuation => {
                f.write_str("indented continuation line has no open list item")
            }
        }
    }
}

/// Split into lines, enforcing the line-count and line-length bounds.
fn split_lines(text: &str) -> Result<Vec<&str>, HelpError> {
    let mut lines = Vec::new();
    for line in text.split('\n') {
        if line.len() > MAX_LINE_LEN {
            return Err(HelpError::LineTooLong);
        }
        if lines.len() == MAX_LINES {
            return Err(HelpError::TooManyLines);
        }
        lines.push(line);
    }
    Ok(lines)
}

/// Walk the lines, opening a section at each `## ` heading and handing the
/// lines between headings to the block parser.
///
/// The walk tracks fence state so a `## ` line *inside* a fenced code block
/// stays code and never opens a section.
fn parse_sections(lines: &[&str]) -> Result<Vec<Section>, HelpError> {
    let mut sections: Vec<Section> = Vec::new();
    let mut current: Option<(SectionKind, usize)> = None;
    let mut in_fence = false;

    for (index, line) in lines.iter().enumerate() {
        if in_fence {
            if *line == "```" {
                in_fence = false;
            }
            continue;
        }
        if line.starts_with("```") {
            if current.is_none() {
                return Err(HelpError::ContentBeforeFirstSection);
            }
            in_fence = true;
            continue;
        }
        let Some(key) = line.strip_prefix("## ") else {
            if current.is_none() && !line.trim().is_empty() {
                return Err(HelpError::ContentBeforeFirstSection);
            }
            continue;
        };
        let kind = SectionKind::from_heading(key).ok_or(HelpError::UnknownHeading)?;
        if let Some((open_kind, _)) = current {
            match kind.rank() {
                rank if rank == open_kind.rank() => return Err(HelpError::DuplicateSection),
                rank if rank < open_kind.rank() => return Err(HelpError::SectionOutOfOrder),
                _ => {}
            }
        }
        close_section(&mut sections, current.take(), index, lines)?;
        current = Some((kind, index + 1));
    }
    close_section(&mut sections, current.take(), lines.len(), lines)?;
    Ok(sections)
}

/// Close the open section, if any, parsing its body lines into blocks.
fn close_section(
    sections: &mut Vec<Section>,
    current: Option<(SectionKind, usize)>,
    end: usize,
    lines: &[&str],
) -> Result<(), HelpError> {
    if let Some((kind, start)) = current {
        let body = lines.get(start..end).unwrap_or(&[]);
        sections.push(Section {
            kind,
            blocks: parse_blocks(body)?,
        });
    }
    Ok(())
}

/// The block a line begins, before any multi-line grouping.
enum LineStart<'a> {
    Blank,
    SubHeading(&'a str),
    OtherHeading,
    Fence(&'a str),
    Bullet(&'a str),
    Ordered(&'a str),
    TableRow,
    Continuation(&'a str),
    Text,
}

/// Classify how `line` opens (or continues) a block.
fn classify(line: &str) -> LineStart<'_> {
    if line.trim().is_empty() {
        return LineStart::Blank;
    }
    if let Some(rest) = line.strip_prefix("### ") {
        return LineStart::SubHeading(rest);
    }
    if line.starts_with('#') {
        return LineStart::OtherHeading;
    }
    if let Some(info) = line.strip_prefix("```") {
        return LineStart::Fence(info);
    }
    if let Some(rest) = line.strip_prefix("- ") {
        return LineStart::Bullet(rest);
    }
    if let Some(rest) = ordered_item(line) {
        return LineStart::Ordered(rest);
    }
    if line.starts_with('|') {
        return LineStart::TableRow;
    }
    if let Some(rest) = line.strip_prefix("  ") {
        return LineStart::Continuation(rest.trim_start());
    }
    LineStart::Text
}

/// The text after an ordered-list marker (`1. ` … `999. `), if `line`
/// carries one.
fn ordered_item(line: &str) -> Option<&str> {
    let (digits, rest) = line.split_once(". ")?;
    if (1..=3).contains(&digits.len()) && digits.bytes().all(|b| b.is_ascii_digit()) {
        Some(rest)
    } else {
        None
    }
}

/// Parse one section body into blocks.
fn parse_blocks(lines: &[&str]) -> Result<Vec<Block>, HelpError> {
    let mut blocks: Vec<Block> = Vec::new();
    let mut cursor = 0;
    while cursor < lines.len() {
        let line = lines.get(cursor).copied().unwrap_or_default();
        let (block, consumed) = match classify(line) {
            LineStart::Blank => {
                cursor += 1;
                continue;
            }
            LineStart::SubHeading(rest) => (Block::SubHeading(parse_spans(rest)), 1),
            LineStart::OtherHeading => return Err(HelpError::UnknownHeading),
            LineStart::Fence(info) => parse_fence(lines, cursor, info)?,
            LineStart::Bullet(_) | LineStart::Ordered(_) => parse_list(lines, cursor)?,
            LineStart::TableRow => parse_table(lines, cursor)?,
            LineStart::Continuation(_) => return Err(HelpError::OrphanContinuation),
            LineStart::Text => parse_paragraph(lines, cursor),
        };
        if blocks.len() == MAX_BLOCKS_PER_SECTION {
            return Err(HelpError::TooManyBlocks);
        }
        blocks.push(block);
        cursor += consumed;
    }
    Ok(blocks)
}

/// Parse a fenced code block opened at `start`; the closing fence is a line
/// that is exactly ```` ``` ````.
fn parse_fence(lines: &[&str], start: usize, info: &str) -> Result<(Block, usize), HelpError> {
    let mut body = Vec::new();
    for (offset, line) in lines.iter().enumerate().skip(start + 1) {
        if *line == "```" {
            return Ok((
                Block::CodeBlock {
                    info: String::from(info.trim()),
                    lines: body,
                },
                offset - start + 1,
            ));
        }
        body.push(String::from(*line));
    }
    Err(HelpError::UnterminatedFence)
}

/// Parse a list opened at `start`: items of one marker kind, each optionally
/// followed by two-space-indented continuation lines.
fn parse_list(lines: &[&str], start: usize) -> Result<(Block, usize), HelpError> {
    let ordered = matches!(
        classify(lines.get(start).copied().unwrap_or_default()),
        LineStart::Ordered(_)
    );
    let mut items: Vec<ListItem> = Vec::new();
    let mut text: Option<String> = None;
    let mut consumed = 0;
    for line in lines.iter().skip(start) {
        let item_text = match classify(line) {
            LineStart::Bullet(rest) if !ordered => Some(rest),
            LineStart::Ordered(rest) if ordered => Some(rest),
            LineStart::Continuation(rest) => {
                match text.as_mut() {
                    Some(text) => {
                        text.push(' ');
                        text.push_str(rest);
                    }
                    None => return Err(HelpError::OrphanContinuation),
                }
                consumed += 1;
                continue;
            }
            _ => break,
        };
        if let Some(done) = text.take() {
            items.push(ListItem {
                spans: parse_spans(&done),
            });
        }
        if items.len() == MAX_LIST_ITEMS {
            return Err(HelpError::TooManyItems);
        }
        text = item_text.map(String::from);
        consumed += 1;
    }
    if let Some(done) = text.take() {
        items.push(ListItem {
            spans: parse_spans(&done),
        });
    }
    Ok((Block::List { ordered, items }, consumed))
}

/// Parse a pipe table opened at `start`: a header row, a delimiter row, and
/// zero or more data rows, all with the same column count.
fn parse_table(lines: &[&str], start: usize) -> Result<(Block, usize), HelpError> {
    let mut raw: Vec<&str> = Vec::new();
    for line in lines.iter().skip(start) {
        if matches!(classify(line), LineStart::TableRow) {
            raw.push(line);
        } else {
            break;
        }
    }
    let mut rows = raw.iter();
    let header_cells = split_row(rows.next().ok_or(HelpError::MalformedTable)?)?;
    if header_cells.len() > MAX_TABLE_COLUMNS {
        return Err(HelpError::TableTooLarge);
    }
    let delimiter_cells = split_row(rows.next().ok_or(HelpError::MalformedTable)?)?;
    if delimiter_cells.len() != header_cells.len() {
        return Err(HelpError::MalformedTable);
    }
    let alignments = delimiter_cells
        .iter()
        .map(|cell| parse_alignment(cell))
        .collect::<Result<Vec<Align>, HelpError>>()?;
    let header = header_cells.iter().map(|cell| parse_spans(cell)).collect();
    let mut data: Vec<Vec<Vec<Span>>> = Vec::new();
    for row in rows {
        let cells = split_row(row)?;
        if cells.len() != header_cells.len() {
            return Err(HelpError::MalformedTable);
        }
        if data.len() == MAX_TABLE_ROWS {
            return Err(HelpError::TableTooLarge);
        }
        data.push(cells.iter().map(|cell| parse_spans(cell)).collect());
    }
    Ok((
        Block::Table(Table {
            header,
            alignments,
            rows: data,
        }),
        raw.len(),
    ))
}

/// Split one `| a | b |` row into trimmed cell texts. The row must both
/// start and end with `|`; `|` is always a cell separator (there is no
/// escaped pipe — a literal `|` belongs in a code block).
fn split_row(row: &str) -> Result<Vec<&str>, HelpError> {
    let inner = row
        .strip_prefix('|')
        .and_then(|rest| rest.trim_end().strip_suffix('|'))
        .ok_or(HelpError::MalformedTable)?;
    let cells: Vec<&str> = inner.split('|').map(str::trim).collect();
    if cells.is_empty() {
        return Err(HelpError::MalformedTable);
    }
    Ok(cells)
}

/// Parse one delimiter-row cell (`---`, `:---`, `:---:`, `---:`).
fn parse_alignment(cell: &str) -> Result<Align, HelpError> {
    let (leading, rest) = match cell.strip_prefix(':') {
        Some(rest) => (true, rest),
        None => (false, cell),
    };
    let (trailing, dashes) = match rest.strip_suffix(':') {
        Some(dashes) => (true, dashes),
        None => (false, rest),
    };
    if dashes.len() < 3 || !dashes.bytes().all(|b| b == b'-') {
        return Err(HelpError::MalformedTable);
    }
    Ok(match (leading, trailing) {
        (true, true) => Align::Center,
        (false, true) => Align::Right,
        _ => Align::Left,
    })
}

/// Parse a paragraph opened at `start`: consecutive plain-text lines joined
/// by single spaces.
fn parse_paragraph(lines: &[&str], start: usize) -> (Block, usize) {
    let mut text = String::new();
    let mut consumed = 0;
    for line in lines.iter().skip(start) {
        if !matches!(classify(line), LineStart::Text) {
            break;
        }
        if !text.is_empty() {
            text.push(' ');
        }
        text.push_str(line.trim_end());
        consumed += 1;
    }
    (Block::Paragraph(parse_spans(&text)), consumed)
}

/// Parse inline spans: `` `code` ``, `**strong**`, `*emphasis*`, and `\`
/// escapes. An unmatched marker is literal text, exactly as Markdown treats
/// it — never an error and never dropped.
pub(crate) fn parse_spans(text: &str) -> Vec<Span> {
    let mut spans: Vec<Span> = Vec::new();
    let mut plain = String::new();
    let mut rest = text;
    while let Some(ch) = rest.chars().next() {
        let after = rest.get(ch.len_utf8()..).unwrap_or_default();
        match ch {
            '\\' => {
                if let Some(escaped) = after.chars().next() {
                    plain.push(escaped);
                    rest = after.get(escaped.len_utf8()..).unwrap_or_default();
                } else {
                    plain.push('\\');
                    rest = after;
                }
            }
            '`' => {
                if let Some((content, remainder)) = take_delimited(after, "`") {
                    flush(&mut spans, &mut plain);
                    spans.push(Span::Code(String::from(content)));
                    rest = remainder;
                } else {
                    plain.push('`');
                    rest = after;
                }
            }
            '*' if after.starts_with('*') => {
                let after_marker = after.get(1..).unwrap_or_default();
                if let Some((content, remainder)) = take_delimited(after_marker, "**") {
                    flush(&mut spans, &mut plain);
                    spans.push(Span::Strong(String::from(content)));
                    rest = remainder;
                } else {
                    plain.push_str("**");
                    rest = after_marker;
                }
            }
            '*' => {
                if let Some((content, remainder)) = take_delimited(after, "*") {
                    flush(&mut spans, &mut plain);
                    spans.push(Span::Emphasis(String::from(content)));
                    rest = remainder;
                } else {
                    plain.push('*');
                    rest = after;
                }
            }
            _ => {
                plain.push(ch);
                rest = after;
            }
        }
    }
    flush(&mut spans, &mut plain);
    spans
}

/// Take the text up to the next `delimiter`, if there is one and the
/// content is non-empty; returns the content and the text after the
/// delimiter.
fn take_delimited<'a>(text: &'a str, delimiter: &str) -> Option<(&'a str, &'a str)> {
    let (content, remainder) = text.split_once(delimiter)?;
    if content.is_empty() {
        return None;
    }
    Some((content, remainder))
}

/// Move any accumulated plain text into a [`Span::Text`].
fn flush(spans: &mut Vec<Span>, plain: &mut String) {
    if !plain.is_empty() {
        spans.push(Span::Text(core::mem::take(plain)));
    }
}
