//! Rendering the two help surfaces as `lib/vt` operations.
//!
//! Both renderers emit a `Vec<rustos_vt::Op>` the caller encodes with
//! `rustos_vt::encode_all` and writes to its own stdout stream. Emitting
//! typed operations rather than raw bytes keeps the escape vocabulary the
//! one `lib/vt` definition, and — because the parser already rejected every
//! control character — guarantees the output carries no control bytes
//! beyond the emitter's own well-formed sequences and line feeds.
//!
//! Styling follows the historical `man` conventions: section headings and
//! strong/code text are bold, emphasis is underlined, code blocks are
//! verbatim and indented. Lines end in a bare line feed; the consumer's
//! terminal line discipline owns carriage handling, and a redirected
//! consumer gets clean text. Neither renderer invents content: an absent
//! section is simply not rendered.

use alloc::string::String;
use alloc::vec::Vec;

use rustos_curses::str_width;
use rustos_vt::{Op, Sgr};

use crate::doc::{Align, Block, HelpDoc, ListItem, Section, SectionKind, Span, Table};

/// Indent for section bodies in the full view.
const BODY_INDENT: &str = "  ";

/// Indent for code-block lines, in both views.
const CODE_INDENT: &str = "    ";

/// Render the short `-h`/`-?` view: the `NAME` and `SYNOPSIS` content plus
/// the `OPTIONS` list, compactly and without headings.
#[must_use]
pub fn render_short(doc: &HelpDoc) -> Vec<Op> {
    let mut out = Vec::new();
    if let Some(section) = doc.section(SectionKind::Name) {
        blocks_compact(&mut out, &section.blocks);
    }
    if let Some(section) = doc.section(SectionKind::Synopsis) {
        newline(&mut out);
        blocks_compact(&mut out, &section.blocks);
    }
    if let Some(section) = doc.section(SectionKind::Options) {
        newline(&mut out);
        blocks_compact(&mut out, &section.blocks);
    }
    out
}

/// Render the full `man` view: every section, in order, with its heading.
#[must_use]
pub fn render_full(doc: &HelpDoc) -> Vec<Op> {
    let mut out = Vec::new();
    let mut first = true;
    for section in doc.sections() {
        if !first {
            newline(&mut out);
        }
        first = false;
        heading(&mut out, section);
        for block in &section.blocks {
            newline(&mut out);
            render_block(&mut out, block, BODY_INDENT);
        }
    }
    out
}

/// Emit one bold section heading line.
fn heading(out: &mut Vec<Op>, section: &Section) {
    out.push(Op::Sgr(Sgr::Bold));
    text(out, section.kind.as_str());
    out.push(Op::Sgr(Sgr::ResetIntensity));
    newline(out);
}

/// Emit the compact (short-view) form of a section body: paragraphs and
/// list items as plain lines, code blocks verbatim; tables and sub-headings
/// render as in the full view.
fn blocks_compact(out: &mut Vec<Op>, blocks: &[Block]) {
    for block in blocks {
        match block {
            Block::Paragraph(spans) => {
                spans_styled(out, spans);
                newline(out);
            }
            Block::List { items, ordered } => list(out, items, *ordered, BODY_INDENT),
            other => render_block(out, other, ""),
        }
    }
}

/// Emit one block of the full view, indented by `indent`.
fn render_block(out: &mut Vec<Op>, block: &Block, indent: &str) {
    match block {
        Block::Paragraph(spans) => {
            text(out, indent);
            spans_styled(out, spans);
            newline(out);
        }
        Block::SubHeading(spans) => {
            text(out, indent);
            out.push(Op::Sgr(Sgr::Bold));
            spans_styled(out, spans);
            out.push(Op::Sgr(Sgr::ResetIntensity));
            newline(out);
        }
        Block::List { items, ordered } => list(out, items, *ordered, indent),
        Block::CodeBlock { lines, .. } => {
            for line in lines {
                text(out, CODE_INDENT);
                text(out, line);
                newline(out);
            }
        }
        Block::Table(table) => render_table(out, table, indent),
    }
}

/// Emit a list, one item per line.
fn list(out: &mut Vec<Op>, items: &[ListItem], ordered: bool, indent: &str) {
    for (index, item) in items.iter().enumerate() {
        text(out, indent);
        if ordered {
            decimal(out, index + 1);
            text(out, ". ");
        } else {
            text(out, "- ");
        }
        spans_styled(out, &item.spans);
        newline(out);
    }
}

/// Emit a table with columns padded to their widest cell, honouring each
/// column's declared alignment, with a dashed rule under the header.
fn render_table(out: &mut Vec<Op>, table: &Table, indent: &str) {
    let widths = column_widths(table);
    table_row(out, &table.header, &widths, &table.alignments, indent, true);
    text(out, indent);
    for (index, width) in widths.iter().enumerate() {
        if index > 0 {
            text(out, "  ");
        }
        for _ in 0..*width {
            text(out, "-");
        }
    }
    newline(out);
    for row in &table.rows {
        table_row(out, row, &widths, &table.alignments, indent, false);
    }
}

/// The display width of each column: its widest header or data cell.
fn column_widths(table: &Table) -> Vec<usize> {
    let mut widths: Vec<usize> = table
        .header
        .iter()
        .map(|cell| str_width(&plain_text(cell)))
        .collect();
    for row in &table.rows {
        for (width, cell) in widths.iter_mut().zip(row) {
            *width = (*width).max(str_width(&plain_text(cell)));
        }
    }
    widths
}

/// Emit one table row, cells padded and separated by two spaces.
fn table_row(
    out: &mut Vec<Op>,
    cells: &[Vec<Span>],
    widths: &[usize],
    alignments: &[Align],
    indent: &str,
    bold: bool,
) {
    text(out, indent);
    for (index, ((cell, width), align)) in cells.iter().zip(widths).zip(alignments).enumerate() {
        if index > 0 {
            text(out, "  ");
        }
        let pad = width.saturating_sub(str_width(&plain_text(cell)));
        let (before, after) = match align {
            Align::Left => (0, pad),
            Align::Right => (pad, 0),
            Align::Center => (pad / 2, pad - pad / 2),
        };
        spaces(out, before);
        if bold {
            out.push(Op::Sgr(Sgr::Bold));
        }
        spans_styled(out, cell);
        if bold {
            out.push(Op::Sgr(Sgr::ResetIntensity));
        }
        spaces(out, after);
    }
    newline(out);
}

/// Emit spans with the `man` styling conventions: code and strong bold,
/// emphasis underlined.
fn spans_styled(out: &mut Vec<Op>, spans: &[Span]) {
    for span in spans {
        match span {
            Span::Text(content) => text(out, content),
            Span::Code(content) | Span::Strong(content) => {
                out.push(Op::Sgr(Sgr::Bold));
                text(out, content);
                out.push(Op::Sgr(Sgr::ResetIntensity));
            }
            Span::Emphasis(content) => {
                out.push(Op::Sgr(Sgr::Underline));
                text(out, content);
                out.push(Op::Sgr(Sgr::ResetUnderline));
            }
        }
    }
}

/// The unstyled text of a span sequence, for width measurement.
fn plain_text(spans: &[Span]) -> String {
    let mut plain = String::new();
    for span in spans {
        match span {
            Span::Text(content)
            | Span::Code(content)
            | Span::Strong(content)
            | Span::Emphasis(content) => plain.push_str(content),
        }
    }
    plain
}

/// Emit a string as print operations.
fn text(out: &mut Vec<Op>, content: &str) {
    out.extend(content.chars().map(Op::Print));
}

/// Emit `count` spaces.
fn spaces(out: &mut Vec<Op>, count: usize) {
    for _ in 0..count {
        out.push(Op::Print(' '));
    }
}

/// Emit a decimal number.
fn decimal(out: &mut Vec<Op>, mut value: usize) {
    let mut digits = [0u8; 20];
    let mut used = 0;
    loop {
        let digit = u8::try_from(value % 10).unwrap_or(0);
        if let Some(slot) = digits.get_mut(used) {
            *slot = b'0' + digit;
        }
        used += 1;
        value /= 10;
        if value == 0 || used == digits.len() {
            break;
        }
    }
    for digit in digits.get(..used).unwrap_or(&[]).iter().rev() {
        out.push(Op::Print(char::from(*digit)));
    }
}

/// Emit a line ending.
fn newline(out: &mut Vec<Op>) {
    out.push(Op::LineFeed);
}
