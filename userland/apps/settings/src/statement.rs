//! The one renderer for a pane that has no controls to draw, and the height
//! it needs.
//!
//! Both stated [`PaneBacking`] variants draw through here, because both are
//! the same shape to a reader: a heading saying what the pane is about, a
//! sentence saying how this system actually stands, and a second sentence
//! saying what would change that or where the setting is reached instead.
//! Two renderers would let the two absences drift into reading like the same
//! thing, and they are not: one is a category this system cannot serve at
//! all, the other a category it can, whose controls this surface does not yet
//! compose. A pane that *does* compose controls has no statement to make and
//! draws its form instead.

use alloc::vec::Vec;

use tairix_font::BitmapFont;
use tairix_geometry::{to_i32, Rect, Scale};
use tairix_raster::{Color, Surface};
use tairix_theme::{TextRole, Theme};

use crate::registry::{PaneBacking, PaneRow};

/// What a statement pane says, in the order it is drawn.
struct Statement<'a> {
    /// The heading: what the category is about.
    heading: &'a str,
    /// How this system stands, or what the pane will show.
    body: &'a str,
    /// What would change it, or where the setting is reached instead.
    tail: &'a str,
    /// Whether the tail names what is needed (as opposed to where the setting
    /// is), which is what decides the label above it.
    needs: bool,
}

/// The two labels above the trailing sentence. They are different facts, so a
/// reader is told which one they are looking at rather than inferring it.
const NEEDS_LABEL: &str = "WHAT WOULD BE NEEDED";
/// The label above a tail that names where the setting is reached today.
const ELSEWHERE_LABEL: &str = "WHERE IT IS SET";

impl<'a> Statement<'a> {
    /// The statement `pane` makes, or `None` for a pane that composes
    /// controls and so has no absence to state.
    fn of(pane: &'a PaneRow) -> Option<Self> {
        match pane.backing {
            PaneBacking::None { missing, needs } => Some(Self {
                heading: pane.title,
                body: missing,
                tail: needs,
                needs: true,
            }),
            PaneBacking::Elsewhere { shows, elsewhere } => Some(Self {
                heading: pane.title,
                body: shows,
                tail: elsewhere,
                needs: false,
            }),
            PaneBacking::Composed => None,
        }
    }

    /// The label above the trailing sentence.
    fn tail_label(&self) -> &'static str {
        if self.needs {
            NEEDS_LABEL
        } else {
            ELSEWHERE_LABEL
        }
    }
}

/// The physical height `pane`'s statement needs in a column `width` pixels
/// wide, so the shell's scroll model measures exactly what the paint draws.
///
/// Zero for a pane that composes controls: the form is what it draws, and
/// the form measures itself.
#[must_use]
pub fn measured_height(pane: &PaneRow, width: u32, scale: Scale, theme: &Theme) -> u32 {
    let Some(statement) = Statement::of(pane) else {
        return 0;
    };
    Metrics::resolve(width, scale, theme).height(&statement)
}

/// Draw `pane`'s statement into `surface` at `bounds`.
///
/// Quiet, and on the surface behind it with no plate — the same shape every
/// other stated absence in the desktop takes, because a plate would read as
/// something to interact with. Nothing here is actionable: a pane with no
/// controls offers none rather than a button that would do nothing.
///
/// `bounds` is the column's *content* rectangle, whose top may sit above the
/// surface while the column is scrolled; the caller clips to what is on
/// screen.
pub fn render(surface: &mut Surface, pane: &PaneRow, bounds: Rect, scale: Scale, theme: &Theme) {
    let Some(statement) = Statement::of(pane) else {
        return;
    };
    let metrics = Metrics::resolve(bounds.width, scale, theme);
    if metrics.text_w == 0 {
        return;
    }
    let x = bounds.left().saturating_add(to_i32(metrics.pad));
    let mut y = bounds.top().saturating_add(to_i32(metrics.pad));
    let palette = theme.palette();

    metrics.heading.draw_text(
        surface,
        x,
        y,
        metrics
            .heading
            .truncate_to_width(statement.heading, metrics.text_w),
        Color::from(palette.on_surface),
    );
    y = y.saturating_add(to_i32(
        metrics.heading.line_height().saturating_add(metrics.gap),
    ));

    y = metrics.draw_paragraph(
        surface,
        x,
        y,
        statement.body,
        Color::from(palette.on_surface),
    );
    y = y.saturating_add(to_i32(metrics.gap));

    metrics.caption.draw_text(
        surface,
        x,
        y,
        metrics
            .caption
            .truncate_to_width(statement.tail_label(), metrics.text_w),
        Color::from(palette.on_surface_muted),
    );
    y = y.saturating_add(to_i32(metrics.caption.line_height()));
    metrics.draw_paragraph(
        surface,
        x,
        y,
        statement.tail,
        Color::from(palette.on_surface_muted),
    );
}

/// The faces, paddings and text width one statement is laid out with.
///
/// Resolved once and read by both the measurement and the paint, so the
/// height the shell scrolls over is the height the pane actually draws.
struct Metrics {
    heading: BitmapFont,
    body: BitmapFont,
    caption: BitmapFont,
    pad: u32,
    gap: u32,
    text_w: u32,
}

impl Metrics {
    fn resolve(width: u32, scale: Scale, theme: &Theme) -> Self {
        let pad = scale.scale_length(theme.metrics().control_inset).max(1);
        Self {
            heading: BitmapFont::for_role(theme.fonts(), TextRole::SectionHeader, scale),
            body: BitmapFont::for_role(theme.fonts(), TextRole::Body, scale),
            caption: BitmapFont::for_role(theme.fonts(), TextRole::Caption, scale),
            pad,
            gap: scale.scale_length(theme.metrics().control_gap).max(1),
            text_w: width.saturating_sub(pad.saturating_mul(2)),
        }
    }

    /// The plate height the statement needs, or zero when the column is too
    /// narrow to draw a word of it.
    fn height(&self, statement: &Statement<'_>) -> u32 {
        if self.text_w == 0 {
            return 0;
        }
        let body_lines = self.wrap(statement.body).len();
        let tail_lines = self.wrap(statement.tail).len();
        let lines = self
            .heading
            .line_height()
            .saturating_add(self.gap)
            .saturating_add(
                self.body
                    .line_height()
                    .saturating_mul(u32::try_from(body_lines).unwrap_or(1)),
            )
            .saturating_add(self.gap)
            .saturating_add(self.caption.line_height())
            .saturating_add(
                self.body
                    .line_height()
                    .saturating_mul(u32::try_from(tail_lines).unwrap_or(1)),
            );
        lines.saturating_add(self.pad.saturating_mul(2))
    }

    /// Draw `text` wrapped to the column, answering the y it ended at.
    fn draw_paragraph(
        &self,
        surface: &mut Surface,
        x: i32,
        mut y: i32,
        text: &str,
        color: Color,
    ) -> i32 {
        for line in self.wrap(text) {
            self.body.draw_text(surface, x, y, line, color);
            y = y.saturating_add(to_i32(self.body.line_height()));
        }
        y
    }

    /// `text` broken into lines that fit the column, on word boundaries.
    ///
    /// A single word longer than the column is drawn elided rather than split
    /// mid-word: the statement is prose, and a hyphenless break reads as a
    /// different word.
    fn wrap<'t>(&self, text: &'t str) -> Vec<&'t str> {
        let mut lines = Vec::new();
        let mut rest = text;
        while !rest.is_empty() {
            let take = self.line_break(rest);
            let (line, tail) = rest.split_at(take);
            lines.push(line.trim_end());
            rest = tail.trim_start();
        }
        if lines.is_empty() {
            lines.push("");
        }
        lines
    }

    /// How many bytes of `text` fit on one line: the longest run of whole
    /// words within the column, or the whole first word when even that is too
    /// wide.
    fn line_break(&self, text: &str) -> usize {
        if self.body.text_width(text) <= self.text_w {
            return text.len();
        }
        let mut fits = 0;
        for (offset, _) in text.char_indices().filter(|(_, c)| *c == ' ') {
            if self.body.text_width(&text[..offset]) > self.text_w {
                break;
            }
            fits = offset;
        }
        if fits == 0 {
            text.char_indices()
                .find(|(_, c)| *c == ' ')
                .map_or(text.len(), |(offset, _)| offset)
        } else {
            fits
        }
    }
}
