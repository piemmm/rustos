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

use tairix_controls::paint_run;
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

/// The most lines one of a statement's two paragraphs takes. A pane states
/// an absence in a sentence or two; past this the pane is a document.
const MAX_PARAGRAPH_LINES: usize = 6;

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
            PaneBacking::Composed(_) => None,
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
    if let Some(statement) = Statement::of(pane) {
        statement.render(surface, bounds, scale, theme);
    }
}

impl Statement<'_> {
    /// Draw this statement into `surface` at `bounds`, as [`render`] states.
    fn render(&self, surface: &mut Surface, bounds: Rect, scale: Scale, theme: &Theme) {
        let metrics = Metrics::resolve(bounds.width, scale, theme);
        if metrics.text_w == 0 {
            return;
        }
        let x = bounds.left().saturating_add(to_i32(metrics.pad));
        let mut y = bounds.top().saturating_add(to_i32(metrics.pad));
        let palette = theme.palette();

        paint_run(
            surface,
            metrics.heading,
            metrics.heading.elide_to_width(self.heading, metrics.text_w),
            (x, y),
            Color::from(palette.on_surface),
            None,
        );
        y = y.saturating_add(to_i32(
            metrics.heading.line_height().saturating_add(metrics.gap),
        ));

        y = metrics.draw_paragraph(surface, x, y, self.body, Color::from(palette.on_surface));
        y = y.saturating_add(to_i32(metrics.gap));

        paint_run(
            surface,
            metrics.caption,
            metrics
                .caption
                .elide_to_width(self.tail_label(), metrics.text_w),
            (x, y),
            Color::from(palette.on_surface_muted),
            None,
        );
        y = y.saturating_add(to_i32(metrics.caption.line_height()));
        metrics.draw_paragraph(
            surface,
            x,
            y,
            self.tail,
            Color::from(palette.on_surface_muted),
        );
    }
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
        self.heading
            .line_height()
            .saturating_add(self.gap)
            .saturating_add(self.paragraph_height(statement.body))
            .saturating_add(self.gap)
            .saturating_add(self.caption.line_height())
            .saturating_add(self.paragraph_height(statement.tail))
            .saturating_add(self.pad.saturating_mul(2))
    }

    /// The height one wrapped paragraph takes in this column.
    fn paragraph_height(&self, text: &str) -> u32 {
        self.body
            .line_height()
            .saturating_mul(u32::try_from(self.lines(text)).unwrap_or(1))
    }

    /// How many lines `text` wraps into in this column.
    ///
    /// The shared fitter's answer, counted from the same lazy layout the
    /// paint draws, so the height the shell scrolls over is the height the
    /// pane puts on screen.
    fn lines(&self, text: &str) -> usize {
        self.body
            .wrap_to_width(text, self.text_w, MAX_PARAGRAPH_LINES)
            .count()
    }

    /// Draw `text` wrapped to the column, answering the y it ended at.
    fn draw_paragraph(
        &self,
        surface: &mut Surface,
        x: i32,
        y: i32,
        text: &str,
        color: Color,
    ) -> i32 {
        let mut pen = y;
        for line in self
            .body
            .wrap_to_width(text, self.text_w, MAX_PARAGRAPH_LINES)
        {
            let run = (line.text, line.elided);
            paint_run(surface, self.body, run, (x, pen), color, None);
            pen = pen.saturating_add(to_i32(self.body.line_height()));
        }
        pen
    }
}

#[cfg(test)]
mod tests {
    use tairix_controls::testkit::marks_elision;
    use tairix_geometry::{Rect, Scale};
    use tairix_raster::Surface;

    use super::Statement;

    /// A heading too long for the column is elided with the shared mark
    /// rather than cut where the column ran out.
    #[test]
    fn a_heading_too_long_for_the_column_is_elided_with_the_mark() {
        let theme = crate::test_support::theme();
        let bounds = Rect::new(0, 0, 240, 200);
        assert!(marks_elision(|heading| {
            let mut surface = Surface::new(bounds.width, bounds.height).expect("surface");
            let statement = Statement {
                heading,
                body: "",
                tail: "",
                needs: true,
            };
            statement.render(&mut surface, bounds, Scale::ONE, &theme);
            surface
        }));
    }
}
