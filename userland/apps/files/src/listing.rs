//! What a round moved in the listing view, as the rectangles it repainted.
//!
//! # Why a mark rather than a control report
//!
//! The listing has no retained control tree: every row and tile is built
//! afresh from the browser's own state each frame ([`tairix_browse::render`]),
//! so there is no control to report its own bounds when a highlight moves.
//! What *did* move is therefore the difference between two readings of that
//! state — the focused entry and the scroll offset — resolved back to
//! rectangles through the renderer's own geometry, so the reported rectangle
//! and the painted one are the same fact.
//!
//! Everything else the view draws (the entries themselves, the toolbar's
//! enable states, the view mode) changes only when the listing is replaced,
//! which is a whole-window repaint the caller concludes for itself.

use tairix_browse::render::{entry_rect, item_area, scrollbar_bounds};
use tairix_browse::{Browser, DirectorySource, ToolbarBand};
use tairix_controls::damage;
use tairix_geometry::{Rect, Region, Scale};
use tairix_theme::Theme;

/// The listing's drawn interaction state: the marks a round can move while
/// the entries beneath them stay the same.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct ViewMark {
    focus: Option<usize>,
    offset: u64,
}

impl ViewMark {
    /// The listing's drawn state right now.
    #[must_use]
    pub fn of<S: DirectorySource>(browser: &Browser<S>) -> Self {
        Self {
            focus: browser.selected_index(),
            offset: browser.scroll_offset(),
        }
    }

    /// Report what moving from `self` to the browser's current state
    /// repainted, answering whether anything did.
    ///
    /// A scroll draws *every* entry somewhere new and moves the bar's thumb
    /// with them, so it marks the whole item area and the gutter; a focus that
    /// moved within one offset marks only the entry it left and the entry it
    /// arrived on.
    pub fn report<S: DirectorySource>(
        self,
        browser: &Browser<S>,
        scale: Scale,
        theme: &Theme,
        viewport: Rect,
        toolbar: ToolbarBand,
        damage: &mut Region,
    ) -> bool {
        let now = Self::of(browser);
        if now == self {
            return false;
        }
        if now.offset != self.offset {
            scrolled(scale, theme, viewport, toolbar, damage);
            return true;
        }
        damage::move_mark(
            self.focus,
            now.focus,
            |index| entry_rect(browser, scale, theme, viewport, toolbar, index),
            damage,
        );
        true
    }
}

/// Report what a scroll of the listing repainted: every entry the view draws,
/// and the bar whose thumb moved with them.
pub fn scrolled(
    scale: Scale,
    theme: &Theme,
    viewport: Rect,
    toolbar: ToolbarBand,
    damage: &mut Region,
) {
    damage.add(item_area(scale, theme, viewport));
    if let Some(bar) = scrollbar_bounds(scale, theme, viewport, toolbar) {
        damage.add(bar);
    }
}

#[cfg(test)]
#[path = "listing_tests.rs"]
mod tests;
