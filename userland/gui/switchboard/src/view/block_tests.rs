//! Unit tests for the surface's shared titled-block anatomy.
//!
//! Each rule the boards fix is asserted against the pixels the paint actually
//! lays down: the plate's rim and its lift off the section ground, the title's
//! role and colour, and which title form draws the hairline rule.

use super::*;

use tairix_theme::Rgba;

fn theme() -> Theme {
    Theme::dark()
}

/// A surface pre-filled with the section ground, so a plate's own ground is
/// distinguishable from the surface it stands on.
fn ground(width: u32, height: u32, theme: &Theme) -> Surface {
    let mut surface = Surface::new(width, height).expect("a test surface");
    surface.fill_rect(0, 0, width, height, Color::from(theme.palette().surface));
    surface
}

fn pixel(surface: &Surface, x: u32, y: u32) -> Rgba {
    let p = surface.get(x, y).expect("a pixel inside the surface");
    Rgba::new(p.r, p.g, p.b, p.a)
}

/// A perceptual weight, so "lighter than the ground" is an assertion rather
/// than three separate channel comparisons.
fn luma(colour: Rgba) -> u32 {
    u32::from(colour.r) + u32::from(colour.g) + u32::from(colour.b)
}

#[test]
fn a_plate_lifts_off_the_section_ground_and_draws_its_rim() {
    let theme = theme();
    let mut surface = ground(120, 60, &theme);
    let bounds = Rect::new(0, 0, 120, 60);

    let content = plate(&mut surface, bounds, Scale::ONE, &theme).expect("a plate this size");

    let palette = theme.palette();
    assert_eq!(
        pixel(&surface, 60, 30),
        palette.surface_raised,
        "a block's ground is the raised fill, not the section's own"
    );
    assert!(
        luma(palette.surface_raised) > luma(palette.surface),
        "the boards draw a block a step lighter than the section behind it"
    );
    let margin = super::plate_margin(Scale::ONE, &theme);
    assert!(
        margin > 0,
        "a plate with no margin shares its neighbour's edge"
    );
    assert_eq!(
        pixel(&surface, 60, margin),
        palette.rim,
        "the plate draws no rim, so nothing separates one block from the next"
    );
    // The margin is what makes the gap: the slot's own edge is still the
    // section behind it, so two blocks in adjacent slots cannot touch.
    assert_eq!(
        pixel(&surface, 60, 0),
        palette.surface,
        "the plate ran to the edge of its slot, leaving no gap for a neighbour"
    );
    assert!(
        content.width < bounds.width && content.height < bounds.height,
        "content was reported at the plate's own edge"
    );
}

/// The content rectangle the paint reports and the padding the pane's flow
/// lays its rows out with are the same figure, so a row cannot land outside
/// the plate that was drawn for it.
#[test]
fn the_reported_content_matches_the_padding_the_flow_lays_rows_out_with() {
    let theme = theme();
    let mut surface = ground(120, 60, &theme);
    let bounds = Rect::new(0, 0, 120, 60);
    let pad = content_inset(Scale::ONE, &theme);

    let content = plate(&mut surface, bounds, Scale::ONE, &theme).expect("a plate this size");

    assert_eq!(content.left(), bounds.left() + to_i32(pad));
    assert_eq!(content.top(), bounds.top() + to_i32(pad));
    assert_eq!(content.width, bounds.width - pad * 2);
}

/// A plate needs room for its rim and its padding on both sides; asked for
/// less it draws nothing rather than reporting a content rectangle that is
/// really its own border.
#[test]
fn a_plate_too_small_to_seat_content_draws_none() {
    let theme = theme();
    let pad = content_inset(Scale::ONE, &theme);
    let mut surface = ground(pad * 2, pad * 2, &theme);
    assert!(plate(
        &mut surface,
        Rect::new(0, 0, pad * 2, pad * 2),
        Scale::ONE,
        &theme
    )
    .is_none());
}

#[test]
fn a_title_is_set_in_the_section_header_role_and_the_accent() {
    let theme = theme();
    let mut surface = ground(200, 40, &theme);
    let rect = Rect::new(0, 0, 200, 40);

    let below = title(&mut surface, rect, Scale::ONE, &theme, "PROCESSOR");

    let face = BitmapFont::for_role(theme.fonts(), TextRole::SectionHeader, Scale::ONE);
    let line = face.line_height();
    let accent = theme.palette().accent;
    assert!(
        (0..line).any(|y| (0..200).any(|x| pixel(&surface, x, y) == accent)),
        "a block's name is drawn in the accent, so it reads as a label"
    );
    assert!(
        below > to_i32(line),
        "the reported baseline is not clear of the title's own line"
    );
}

#[test]
fn a_ruled_title_draws_the_hairline_and_reports_the_baseline_below_it() {
    let theme = theme();
    let mut surface = ground(200, 40, &theme);
    let rect = Rect::new(0, 0, 200, 40);

    let below = title(&mut surface, rect, Scale::ONE, &theme, "PROCESSOR");

    let border = theme.palette().border;
    let rule = (0..u32::try_from(below).unwrap_or(0))
        .find(|y| pixel(&surface, 100, *y) == border)
        .expect("the hairline rule under the title");
    assert!(
        to_i32(rule) < below,
        "the content would be drawn over the rule"
    );
}

/// A block whose body brings its own plates draws no rule: the cells' own
/// rims already separate the title from the readings, and the boards draw no
/// second line there.
#[test]
fn a_bare_title_draws_no_rule() {
    let theme = theme();
    let mut ruled = ground(200, 40, &theme);
    let mut bare = ground(200, 40, &theme);
    let rect = Rect::new(0, 0, 200, 40);

    let ruled_below = title(&mut ruled, rect, Scale::ONE, &theme, "PER-CORE BUSY");
    let bare_below = bare_title(&mut bare, rect, Scale::ONE, &theme, "PER-CORE BUSY");

    let border = theme.palette().border;
    assert!(
        (0..40).any(|y| pixel(&ruled, 100, y) == border),
        "the ruled form drew no rule, so this proves nothing about the bare one"
    );
    assert!(
        !(0..40).any(|y| pixel(&bare, 100, y) == border),
        "a self-plating block's title drew a rule the boards do not show"
    );
    assert!(
        bare_below < ruled_below,
        "the bare form reserved room for a rule it never drew"
    );
}
