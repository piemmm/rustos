//! Unit tests for the System section's overview page.

use tairix_geometry::{Rect, Scale};
use tairix_raster::Surface;
use tairix_theme::Theme;

use crate::view::test_support::{bounds, font, has_ink, model};
use crate::view::{Section, Switchboard};

#[test]
fn overview_resource_cards_still_render_from_the_extended_model() {
    let theme = Theme::dark();
    let mut sb = Switchboard::new(model());
    sb.select_section(Section::Overview);
    let b = bounds();
    let mut surface = Surface::new(b.width, b.height).expect("surface");
    sb.render(&mut surface, b, Scale::ONE, &theme, font());
    let layout = sb.compute_layout(b, Scale::ONE, &theme, font());
    let pc = sb
        .panel
        .content_rect(layout.content, Scale::ONE, &theme)
        .expect("panel content");
    let card_h = Switchboard::card_item_height(Scale::ONE, &theme);
    let block = Rect::new(pc.left(), pc.top(), pc.width, card_h.saturating_mul(3));
    assert!(
        has_ink(&surface, block),
        "the resource card block must still paint"
    );
}
