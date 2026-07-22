//! Rendering the two help surfaces as `lib/vt` operations.
//!
//! Both renderers emit a `Vec<tairix_vt::Op>` the caller encodes with
//! `tairix_vt::encode_all_into` and writes to its own stdout stream. Emitting
//! typed operations rather than raw bytes keeps the escape vocabulary the
//! one `lib/vt` definition, and — because the parser already rejected every
//! control character — guarantees the output carries no control bytes
//! beyond the emitter's own well-formed sequences and line feeds.
//!
//! # Styling
//!
//! Colour and emphasis follow the one standard TAIRiX terminal colour scheme
//! (`tairix_vt::scheme`): section headings and sub-headings in the heading
//! role, `*emphasis*` in the emphasis role, inline code and fenced blocks in
//! the literal role, `**strong**` bold, and table rules in the border role.
//! Each styled run is emitted flat — the style's operations, the text, then a
//! single reset — so no run nests inside another and stripping every escape
//! leaves the exact same text (the information never rests on colour alone).
//!
//! The caller chooses one [`Styling`] for the whole render, in a [`RenderCtx`]
//! that also names the served [`Locale`] so headings display in the page's
//! language:
//!
//! * [`Styling::Plain`] — no escape sequences at all. This is what a
//!   redirected or piped consumer gets, so `man <cmd> | cat` sees clean text.
//! * [`Styling::Monochrome`] — the emphasis attributes (bold, italic) but no
//!   colour, for a terminal the colour scheme degrades to monochrome on.
//! * [`Styling::Colour`] — the full standard scheme.
//!
//! Lines end in a bare line feed; the consumer's terminal line discipline owns
//! carriage handling. Neither renderer invents content: an absent section is
//! simply not rendered.

use alloc::string::String;
use alloc::vec::Vec;

use tairix_curses::str_width;
use tairix_vt::{Color, Op, Role, Sgr, Style};

use crate::doc::{Align, Block, HelpDoc, ListItem, Section, SectionKind, Span, Table};
use crate::locale::Locale;

/// Indent for section bodies in the full view.
const BODY_INDENT: &str = "  ";

/// Indent for code-block lines, in both views.
const CODE_INDENT: &str = "    ";

/// How much of the standard colour scheme a render uses.
///
/// Colour and emphasis are presentation only; the same text survives with
/// every attribute stripped. The caller picks one level for the whole render
/// from the terminal's attested capability — plain for a non-terminal, the
/// scheme's monochrome degrade for a terminal without colour, and the full
/// scheme for a colour terminal.
#[derive(Copy, Clone, Debug, Eq, PartialEq)]
pub enum Styling {
    /// Emit no escape sequences at all — clean text for a redirected or piped
    /// consumer.
    Plain,
    /// Emit the emphasis attributes (bold, italic, underline) but no colour.
    Monochrome,
    /// Emit the full standard scheme, colour and attributes.
    Colour,
}

impl Styling {
    /// Resolve `style` to the operations to actually emit under this level, or
    /// `None` when nothing should be emitted (plain output, or a style that
    /// carries no attribute this level keeps).
    fn resolve(self, style: Style) -> Option<Style> {
        match self {
            Styling::Plain => None,
            Styling::Monochrome => {
                let mono = Style {
                    foreground: Color::Default,
                    ..style
                };
                (!mono.is_plain()).then_some(mono)
            }
            Styling::Colour => (!style.is_plain()).then_some(style),
        }
    }
}

/// The context one render runs under: the served locale (for the displayed
/// heading language) and the styling level.
#[derive(Copy, Clone, Debug)]
pub struct RenderCtx<'a> {
    /// The locale the shown document was served from, so headings display in
    /// its language while the document keys stay language-neutral.
    pub locale: &'a Locale,
    /// How much of the standard colour scheme to emit.
    pub styling: Styling,
}

impl<'a> RenderCtx<'a> {
    /// A context for `locale` at `styling`.
    #[must_use]
    pub fn new(locale: &'a Locale, styling: Styling) -> Self {
        Self { locale, styling }
    }
}

/// Render the short `-h`/`-?` view: the `NAME` and `SYNOPSIS` content plus
/// the `OPTIONS` list, compactly and without headings.
#[must_use]
pub fn render_short(doc: &HelpDoc, ctx: &RenderCtx<'_>) -> Vec<Op> {
    let mut out = Vec::new();
    if let Some(section) = doc.section(SectionKind::Name) {
        blocks_compact(&mut out, &section.blocks, ctx);
    }
    if let Some(section) = doc.section(SectionKind::Synopsis) {
        newline(&mut out);
        blocks_compact(&mut out, &section.blocks, ctx);
    }
    if let Some(section) = doc.section(SectionKind::Options) {
        newline(&mut out);
        blocks_compact(&mut out, &section.blocks, ctx);
    }
    out
}

/// Render the full `man` view: every section, in order, with its heading.
#[must_use]
pub fn render_full(doc: &HelpDoc, ctx: &RenderCtx<'_>) -> Vec<Op> {
    let mut out = Vec::new();
    let mut first = true;
    for section in doc.sections() {
        if !first {
            newline(&mut out);
        }
        first = false;
        heading(&mut out, section, ctx);
        for block in &section.blocks {
            newline(&mut out);
            render_block(&mut out, block, BODY_INDENT, ctx);
        }
    }
    out
}

/// Emit one section heading line, in the served locale's language and the
/// heading role.
fn heading(out: &mut Vec<Op>, section: &Section, ctx: &RenderCtx<'_>) {
    let label = section.kind.heading_label(ctx.locale);
    styled_text(out, Role::Heading.style(), ctx.styling, label);
    newline(out);
}

/// Emit the compact (short-view) form of a section body: paragraphs and
/// list items as plain lines, code blocks verbatim; tables and sub-headings
/// render as in the full view.
fn blocks_compact(out: &mut Vec<Op>, blocks: &[Block], ctx: &RenderCtx<'_>) {
    for block in blocks {
        match block {
            Block::Paragraph(spans) => {
                spans_styled(out, spans, ctx.styling, Style::plain());
                newline(out);
            }
            Block::List { items, ordered } => list(out, items, *ordered, BODY_INDENT, ctx),
            other => render_block(out, other, "", ctx),
        }
    }
}

/// Emit one block of the full view, indented by `indent`.
fn render_block(out: &mut Vec<Op>, block: &Block, indent: &str, ctx: &RenderCtx<'_>) {
    match block {
        Block::Paragraph(spans) => {
            text(out, indent);
            spans_styled(out, spans, ctx.styling, Style::plain());
            newline(out);
        }
        Block::SubHeading(spans) => {
            text(out, indent);
            // Fold the heading role onto each span so a sub-heading reads as a
            // heading without a nested style run.
            spans_styled(out, spans, ctx.styling, Role::Heading.style());
            newline(out);
        }
        Block::List { items, ordered } => list(out, items, *ordered, indent, ctx),
        Block::CodeBlock { lines, .. } => {
            for line in lines {
                text(out, CODE_INDENT);
                styled_text(out, Role::Literal.style(), ctx.styling, line);
                newline(out);
            }
        }
        Block::Table(table) => render_table(out, table, indent, ctx),
    }
}

/// Emit a list, one item per line.
fn list(out: &mut Vec<Op>, items: &[ListItem], ordered: bool, indent: &str, ctx: &RenderCtx<'_>) {
    for (index, item) in items.iter().enumerate() {
        text(out, indent);
        if ordered {
            decimal(out, index + 1);
            text(out, ". ");
        } else {
            text(out, "- ");
        }
        spans_styled(out, &item.spans, ctx.styling, Style::plain());
        newline(out);
    }
}

/// Emit a table with columns padded to their widest cell, honouring each
/// column's declared alignment, with a dashed rule under the header.
fn render_table(out: &mut Vec<Op>, table: &Table, indent: &str, ctx: &RenderCtx<'_>) {
    let widths = column_widths(table);
    table_row(
        out,
        &table.header,
        &widths,
        &table.alignments,
        indent,
        ctx,
        true,
    );
    text(out, indent);
    let mut rule = String::new();
    for (index, width) in widths.iter().enumerate() {
        if index > 0 {
            rule.push_str("  ");
        }
        for _ in 0..*width {
            rule.push('-');
        }
    }
    styled_text(out, Role::Border.style(), ctx.styling, &rule);
    newline(out);
    for row in &table.rows {
        table_row(out, row, &widths, &table.alignments, indent, ctx, false);
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
#[allow(clippy::too_many_arguments)]
fn table_row(
    out: &mut Vec<Op>,
    cells: &[Vec<Span>],
    widths: &[usize],
    alignments: &[Align],
    indent: &str,
    ctx: &RenderCtx<'_>,
    header: bool,
) {
    // The header row folds bold onto every cell; data rows carry no fold.
    let fold = if header {
        Style::plain().bold()
    } else {
        Style::plain()
    };
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
        spans_styled(out, cell, ctx.styling, fold);
        spaces(out, after);
    }
    newline(out);
}

/// Emit spans with the standard colour scheme, folding `fold`'s attributes
/// and colour onto every span (the identity [`Style::plain`] leaves each span
/// as-is).
fn spans_styled(out: &mut Vec<Op>, spans: &[Span], styling: Styling, fold: Style) {
    for span in spans {
        let (content, base) = match span {
            Span::Text(content) => (content, Style::plain()),
            Span::Code(content) => (content, Role::Literal.style()),
            Span::Strong(content) => (content, Style::plain().bold()),
            Span::Emphasis(content) => (content, Role::Emphasis.style()),
        };
        styled_text(out, fold_styles(base, fold), styling, content);
    }
}

/// Combine two styles: the base's colour wins over the fold's, and every
/// attribute is the logical OR of the two.
fn fold_styles(base: Style, fold: Style) -> Style {
    let foreground = if matches!(base.foreground, Color::Default) {
        fold.foreground
    } else {
        base.foreground
    };
    Style {
        foreground,
        bold: base.bold || fold.bold,
        dim: base.dim || fold.dim,
        italic: base.italic || fold.italic,
        underline: base.underline || fold.underline,
    }
}

/// Emit `content` under `style`, resolved through `styling`: the style's
/// operations, the text, then a single reset. A style that resolves to
/// nothing prints the text plainly.
fn styled_text(out: &mut Vec<Op>, style: Style, styling: Styling, content: &str) {
    match styling.resolve(style) {
        None => text(out, content),
        Some(style) => {
            let (sgrs, count) = style.open();
            for sgr in sgrs.get(..count).unwrap_or(&[]) {
                out.push(Op::Sgr(*sgr));
            }
            text(out, content);
            out.push(Op::Sgr(Sgr::Reset));
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
