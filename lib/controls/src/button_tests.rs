//! Unit tests for the button family (spec §20 checklist).
//!
//! These cover measurement (the plate paints within its bounds and rounds its
//! corners), state transitions (pointer press/release and keyboard
//! activation), theme switching, the spec §13 denied-vs-disabled distinction,
//! proportional progress seams, and the split button's two regions.

use tairix_font::BitmapFont;
use tairix_geometry::{Point, Rect, Scale};
use tairix_icon::IconKind;
use tairix_input::{InputEvent, Key, NamedKey, PointerButton};
use tairix_raster::{Color, Pixel, Surface};
use tairix_theme::{Rgba, Theme};

use crate::button::{
    Button, ButtonAction, ButtonContent, ContentAlign, IconButton, SplitAction, SplitButton,
};
use crate::damage::sink;
use crate::state::{
    ActivityState, AuthorityState, ControlRole, ControlState, PressureKind, PressureState,
    ProgressValue, RecoveryState,
};
use crate::testkit::{control_font, high_contrast};

const W: u32 = 140;
const H: u32 = 28;

fn font() -> BitmapFont {
    control_font(&Theme::dark(), Scale::ONE)
}

fn premul(rgba: Rgba) -> Pixel {
    Color::from(rgba).premultiply()
}

fn render(button: &Button, theme: &Theme) -> Surface {
    let mut surface = Surface::new(W, H).expect("surface");
    button.render(&mut surface, Rect::new(0, 0, W, H), Scale::ONE, theme);
    surface
}

fn has_pixel(surface: &Surface, want: Pixel) -> bool {
    surface.pixels().contains(&want)
}

/// The leftmost surface column holding `want`, if any.
fn leftmost_column(surface: &Surface, want: Pixel) -> Option<u32> {
    (0..W).find(|&x| (0..H).any(|y| surface.get(x, y) == Some(want)))
}

fn moved(x: i32, y: i32) -> InputEvent {
    InputEvent::PointerMoved {
        to: Point::new(x, y),
    }
}

fn iv(v: u32) -> i32 {
    i32::try_from(v).expect("small test dimension")
}

/// A focused `FocusState`, kept in one helper so the focus cases read clearly.
fn focused_state() -> crate::state::FocusState {
    crate::state::FocusState::FOCUSED
}

fn transparent() -> Pixel {
    Color::TRANSPARENT.premultiply()
}

/// Whether `want` appears anywhere inside the plate interior — the region
/// clear of the 1px outer Signal Rim, so an edge-only colour (a rim tint)
/// cannot masquerade as an interior signal (a Signal Bead).
fn interior_has(surface: &Surface, want: Pixel) -> bool {
    (2..W - 2)
        .flat_map(|x| (2..H - 2).map(move |y| (x, y)))
        .any(|(x, y)| surface.get(x, y) == Some(want))
}

const PRESS: InputEvent = InputEvent::PointerPressed {
    button: PointerButton::Primary,
};
const RELEASE: InputEvent = InputEvent::PointerReleased {
    button: PointerButton::Primary,
};

// --- Measurement --------------------------------------------------------

#[test]
fn idle_button_paints_plate_and_rounds_its_corners() {
    let theme = Theme::dark();
    let surface = render(&Button::labelled("OK"), &theme);
    // The Alloy Plate colour appears in the interior.
    assert!(has_pixel(&surface, premul(theme.palette().surface_raised)));
    // A mid-edge pixel is the Signal Rim; the extreme corner is rounded away
    // to transparent (proof the shared rounded-rect fill ran).
    assert_eq!(surface.get(0, H / 2), Some(premul(theme.palette().rim)));
    assert_eq!(surface.get(0, 0), Some(transparent()));
}

#[test]
fn primary_role_uses_the_accent_rim() {
    let theme = Theme::dark();
    let button = Button::new(ButtonContent::Label("Go".into()), ControlRole::Primary);
    let surface = render(&button, &theme);
    assert_eq!(surface.get(0, H / 2), Some(premul(theme.palette().accent)));
}

#[test]
fn destructive_role_uses_the_danger_rim() {
    let theme = Theme::dark();
    let button = Button::new(
        ButtonContent::Label("Delete".into()),
        ControlRole::Destructive,
    );
    let surface = render(&button, &theme);
    assert_eq!(surface.get(0, H / 2), Some(premul(theme.palette().danger)));
}

// --- Content seating ----------------------------------------------------

#[test]
fn content_alignment_defaults_to_centred_and_records_the_choice() {
    let button = Button::labelled("Pause");
    assert_eq!(button.align(), ContentAlign::Center);
    let leading = button.clone().aligned(ContentAlign::Leading);
    assert_eq!(leading.align(), ContentAlign::Leading);
    // Seating is drawn, so it takes part in render equivalence.
    assert_ne!(button, leading);
}

#[test]
fn leading_alignment_seats_the_label_against_the_plate_inset() {
    let theme = Theme::dark();
    let button = Button::labelled("Pause");
    let label = premul(theme.palette().on_surface);

    let centred = leftmost_column(&render(&button, &theme), label).expect("centred label");
    let leading = leftmost_column(
        &render(&button.clone().aligned(ContentAlign::Leading), &theme),
        label,
    )
    .expect("leading label");

    assert!(leading < centred, "{leading} should precede {centred}");
    // Clear of the frame border and content inset, and within one glyph cell
    // of them — seated at the inset rather than adrift inside the plate.
    let inset = crate::paint::plate_border(&theme, Scale::ONE)
        + Scale::ONE.scale_length(theme.metrics().control_inset);
    assert!(leading >= inset, "{leading} must clear the inset {inset}");
    assert!(leading <= inset + font().text_width("P"));
}

#[test]
fn leading_alignment_seats_an_icon_and_label_group_together() {
    let theme = Theme::dark();
    let button = Button::new(
        ButtonContent::IconLabel {
            icon: IconKind::Pause,
            label: "Pause".into(),
        },
        ControlRole::Neutral,
    );
    let label = premul(theme.palette().on_surface);

    let centred = leftmost_column(&render(&button, &theme), label).expect("centred group");
    let leading = leftmost_column(
        &render(&button.aligned(ContentAlign::Leading), &theme),
        label,
    )
    .expect("leading group");

    assert!(leading < centred, "{leading} should precede {centred}");
}

// --- Theme switching ----------------------------------------------------

#[test]
fn theme_switch_repaints_the_rim() {
    let button = Button::labelled("OK");
    let dark = render(&button, &Theme::dark());
    let light = render(&button, &Theme::light());
    assert_ne!(dark.get(0, H / 2), light.get(0, H / 2));
}

// --- §13 authority rendering -------------------------------------------

#[test]
fn denied_and_disabled_render_distinctly() {
    let theme = Theme::dark();
    let mut disabled = Button::labelled("X");
    disabled.set_state(ControlState::disabled());
    let mut denied = Button::labelled("X");
    denied.set_state(ControlState::idle().with_authority(AuthorityState::Denied));

    let d = render(&disabled, &theme);
    let n = render(&denied, &theme);
    // A denial must not look like a plain disabled control.
    assert_eq!(d.get(0, H / 2), Some(premul(theme.palette().border)));
    assert_eq!(n.get(0, H / 2), Some(premul(theme.palette().denied)));
    assert_ne!(d.get(0, H / 2), n.get(0, H / 2));
}

#[test]
fn pressure_paints_a_leading_rail() {
    let theme = Theme::dark();
    let mut button = Button::labelled("Sleep");
    button
        .set_state(ControlState::idle().with_pressure(PressureState::Under(PressureKind::Memory)));
    let surface = render(&button, &theme);
    // The memory-pressure signal colour appears just inside the leading edge.
    assert!(has_pixel(&surface, premul(theme.palette().memory_pressure)));
}

// --- Progress seam (measurement) ---------------------------------------

#[test]
fn known_progress_paints_a_proportional_seam() {
    let theme = Theme::dark();
    let mut button = Button::labelled("Copy");
    button.set_state(
        ControlState::idle().with_activity(ActivityState::Progress(ProgressValue::new(500))),
    );
    let surface = render(&button, &theme);
    let seam = premul(theme.palette().accent);
    let seam_y = H - 2; // within the lower seam band
                        // The seam is present in the first half and absent in the last quarter.
    assert_eq!(surface.get(W / 4, seam_y), Some(seam));
    assert_ne!(surface.get(W - 4, seam_y), Some(seam));
}

#[test]
fn indeterminate_activity_paints_a_full_width_seam() {
    let theme = Theme::dark();
    let mut button = Button::labelled("Sync");
    button.set_state(ControlState::idle().with_activity(ActivityState::Indeterminate));
    let surface = render(&button, &theme);
    let seam = premul(theme.palette().accent);
    let seam_y = H - 2;
    assert_eq!(surface.get(W / 4, seam_y), Some(seam));
    assert_eq!(surface.get(3 * W / 4, seam_y), Some(seam));
}

// --- Pointer interaction ------------------------------------------------

#[test]
fn press_then_release_inside_activates() {
    let mut button = Button::labelled("OK");
    let bounds = Rect::new(0, 0, W, H);
    assert_eq!(button.on_pointer(&moved(10, 10), bounds, &mut sink()), None);
    assert_eq!(button.on_pointer(&PRESS, bounds, &mut sink()), None);
    assert_eq!(
        button.on_pointer(&RELEASE, bounds, &mut sink()),
        Some(ButtonAction::Activated)
    );
}

#[test]
fn release_outside_cancels() {
    let mut button = Button::labelled("OK");
    let bounds = Rect::new(0, 0, W, H);
    assert_eq!(button.on_pointer(&moved(10, 10), bounds, &mut sink()), None);
    assert_eq!(button.on_pointer(&PRESS, bounds, &mut sink()), None);
    // Pointer leaves the button before release.
    assert_eq!(
        button.on_pointer(&moved(1000, 1000), bounds, &mut sink()),
        None
    );
    assert_eq!(button.on_pointer(&RELEASE, bounds, &mut sink()), None);
}

#[test]
fn disabled_button_never_activates() {
    let mut button = Button::labelled("OK");
    button.set_state(ControlState::disabled());
    let bounds = Rect::new(0, 0, W, H);
    button.on_pointer(&moved(10, 10), bounds, &mut sink());
    button.on_pointer(&PRESS, bounds, &mut sink());
    assert_eq!(button.on_pointer(&RELEASE, bounds, &mut sink()), None);
    // A denied button likewise never activates.
    button.set_state(ControlState::idle().with_authority(AuthorityState::Denied));
    button.on_pointer(&moved(10, 10), bounds, &mut sink());
    button.on_pointer(&PRESS, bounds, &mut sink());
    assert_eq!(button.on_pointer(&RELEASE, bounds, &mut sink()), None);
}

// --- Keyboard interaction ----------------------------------------------

#[test]
fn focused_button_activates_on_space_and_enter() {
    let mut button = Button::labelled("OK");
    button.set_focused(true);
    assert_eq!(button.on_key(Key::Char(' ')), Some(ButtonAction::Activated));
    assert_eq!(
        button.on_key(Key::Named(NamedKey::Enter)),
        Some(ButtonAction::Activated)
    );
    // A non-activation key does nothing.
    assert_eq!(button.on_key(Key::Char('x')), None);
}

#[test]
fn unfocused_or_disabled_button_ignores_keys() {
    let mut button = Button::labelled("OK");
    assert_eq!(button.on_key(Key::Char(' ')), None);
    button.set_focused(true);
    button.set_state(
        button
            .state()
            .with_enabled(false)
            .with_focus(focused_state()),
    );
    assert_eq!(button.on_key(Key::Char(' ')), None);
}

// --- IconButton ---------------------------------------------------------

#[test]
fn icon_button_paints_and_activates() {
    let theme = Theme::dark();
    let button = IconButton::new(IconKind::Bell, ControlRole::Neutral);
    let mut surface = Surface::new(H, H).expect("surface");
    button.render(
        &mut surface,
        Rect::new(0, 0, H, H),
        Scale::ONE,
        &theme,
        None,
    );
    assert!(has_pixel(&surface, premul(theme.palette().surface_raised)));

    let mut button = button;
    let bounds = Rect::new(0, 0, H, H);
    button.on_pointer(&moved(5, 5), bounds, &mut sink());
    button.on_pointer(&PRESS, bounds, &mut sink());
    assert_eq!(
        button.on_pointer(&RELEASE, bounds, &mut sink()),
        Some(ButtonAction::Activated)
    );
}

#[test]
fn icon_only_glyph_fills_the_plate_not_the_text_inset() {
    // An icon-only button gives its whole face to the glyph. Regression: it
    // was sized off the text inset (`control_inset`, tuned for a line of type),
    // which on the default 28px plate left only a ~6px glyph adrift in the
    // button — the "tiny icon" defect. It must instead fill most of the plate.
    let theme = Theme::dark();
    let border = theme.metrics().border_thickness;
    let inset = theme.metrics().control_inset;
    let plate = theme.metrics().control_height; // the toolbar icon plate is square

    let side = crate::button::icon_content_side(plate, plate, border);

    // Comfortably larger than the old text-inset content box would have been.
    let text_box = plate.saturating_sub((border + inset).saturating_mul(2));
    assert!(
        side > text_box.saturating_mul(2),
        "icon side {side} is not clearly larger than the old text box {text_box}",
    );
    // A generous fill (at least ~60%) yet clear of the frame (never overflows).
    assert!(
        side >= plate * 3 / 5,
        "icon side {side} under-fills the plate"
    );
    assert!(
        side <= plate,
        "icon side {side} overflows the plate {plate}"
    );
}

/// A solid square of `color` — the stand-in for an owner's rasterised icon
/// artwork, which a control blits without ever decoding image bytes.
fn artwork(side: u32, color: Color) -> Surface {
    let mut art = Surface::new(side, side).expect("artwork surface");
    art.fill(color);
    art
}

/// A colour no theme palette uses, so finding it in a render can only mean the
/// supplied artwork reached the surface.
const ART: Color = Color::rgb(255, 0, 255);

/// The bounding box `(min_x, min_y, max_x, max_y)` of `want` in `surface`.
fn bbox(surface: &Surface, want: Pixel) -> Option<(u32, u32, u32, u32)> {
    let mut found: Option<(u32, u32, u32, u32)> = None;
    for y in 0..surface.height() {
        for x in 0..surface.width() {
            if surface.get(x, y) == Some(want) {
                found = Some(match found {
                    None => (x, y, x, y),
                    Some((x0, y0, x1, y1)) => (x0.min(x), y0.min(y), x1.max(x), y1.max(y)),
                });
            }
        }
    }
    found
}

#[test]
fn icon_button_blits_supplied_artwork_and_falls_back_to_the_glyph_without_it() {
    let theme = Theme::dark();
    let button = IconButton::new(IconKind::Library, ControlRole::Primary);
    let bounds = Rect::new(0, 0, H, H);
    let side = button.icon_side(bounds, Scale::ONE, &theme);
    assert!(side > 0, "the button reserves an icon slot");

    // Slot-sized artwork fills exactly the slot the button advertised.
    let mut with_art = Surface::new(H, H).expect("surface");
    button.render(
        &mut with_art,
        bounds,
        Scale::ONE,
        &theme,
        Some(&artwork(side, ART)),
    );
    let drawn = bbox(&with_art, ART.premultiply()).expect("artwork drawn");
    assert_eq!(drawn.2 + 1 - drawn.0, side);
    assert_eq!(drawn.3 + 1 - drawn.1, side);

    // Without artwork the same button draws its built-in glyph instead — the
    // supplied colour never appears, so a missing icon can never blank it.
    let mut glyph = Surface::new(H, H).expect("surface");
    button.render(&mut glyph, bounds, Scale::ONE, &theme, None);
    assert!(!has_pixel(&glyph, ART.premultiply()));
    assert!(has_pixel(&glyph, premul(theme.palette().on_accent)));
}

// --- SplitButton --------------------------------------------------------

#[test]
fn split_button_reports_the_activated_region() {
    let theme = Theme::dark();
    let mut split = SplitButton::new(ButtonContent::Label("Run".into()), ControlRole::Primary);
    let bounds = Rect::new(0, 0, W, H);
    // A click on the far left lands in the primary region.
    split.on_pointer(
        &moved(5, iv(H) / 2),
        bounds,
        Scale::ONE,
        &theme,
        &mut sink(),
    );
    split.on_pointer(&PRESS, bounds, Scale::ONE, &theme, &mut sink());
    assert_eq!(
        split.on_pointer(&RELEASE, bounds, Scale::ONE, &theme, &mut sink()),
        Some(SplitAction::Primary)
    );

    // A click on the far right lands in the disclosure region.
    split.on_pointer(
        &moved(iv(W) - 5, iv(H) / 2),
        bounds,
        Scale::ONE,
        &theme,
        &mut sink(),
    );
    split.on_pointer(&PRESS, bounds, Scale::ONE, &theme, &mut sink());
    assert_eq!(
        split.on_pointer(&RELEASE, bounds, Scale::ONE, &theme, &mut sink()),
        Some(SplitAction::Disclosure)
    );
}

#[test]
fn split_button_keyboard_follows_region_focus() {
    let mut split = SplitButton::new(ButtonContent::Label("Run".into()), ControlRole::Neutral);
    split.set_primary_state(ControlState::idle().with_focus(focused_state()));
    assert_eq!(split.on_key(Key::Char(' ')), Some(SplitAction::Primary));

    split.set_primary_state(ControlState::idle());
    split.set_disclosure_state(ControlState::idle().with_focus(focused_state()));
    assert_eq!(
        split.on_key(Key::Named(NamedKey::Enter)),
        Some(SplitAction::Disclosure)
    );
}

#[test]
fn split_button_paints_a_disclosure_chevron() {
    let theme = Theme::dark();
    let split = SplitButton::new(ButtonContent::Label("Run".into()), ControlRole::Neutral);
    let mut surface = Surface::new(W, H).expect("surface");
    split.render(&mut surface, Rect::new(0, 0, W, H), Scale::ONE, &theme);
    // The chevron paints the label colour in the disclosure (right) region.
    let label = premul(theme.palette().on_surface);
    let right = (W - H / 2)..W;
    assert!(right
        .flat_map(|x| (0..H).map(move |y| (x, y)))
        .any(|(x, y)| surface.get(x, y) == Some(label)));
}

// --- Scale --------------------------------------------------------------

#[test]
fn renders_at_a_larger_scale_without_panicking() {
    let theme = Theme::dark();
    let button = Button::labelled("OK");
    let scale = Scale::from_percent(200).expect("valid scale");
    let mut surface = Surface::new(W * 2, H * 2).expect("surface");
    button.render(&mut surface, Rect::new(0, 0, W * 2, H * 2), scale, &theme);
    assert!(has_pixel(&surface, premul(theme.palette().surface_raised)));
}

// --- Accessibility: high contrast (§15) --------------------------------

#[test]
fn high_contrast_thickens_the_signal_rim() {
    // At normal contrast the rim is one pixel: the pixel one in from the edge
    // is already the Alloy Plate. High contrast doubles the rim thickness
    // before adding any glow, so that same pixel is still the rim.
    let rim = premul(Theme::dark().palette().rim);
    let normal = render(&Button::labelled("OK"), &Theme::dark());
    let heavy = render(&Button::labelled("OK"), &high_contrast());
    assert_ne!(normal.get(1, H / 2), Some(rim));
    assert_eq!(heavy.get(1, H / 2), Some(rim));
}

// --- Accessibility: shape-coded Signal Beads (§13, §15) ----------------

#[test]
fn completion_paints_a_success_bead() {
    // A finished job shows the success bead — a colour that appears nowhere
    // else on a resting neutral plate, so its presence proves the mark.
    let theme = Theme::dark();
    let mut button = Button::labelled("Copy");
    button.set_state(ControlState::idle().with_activity(ActivityState::Complete));
    let surface = render(&button, &theme);
    assert!(interior_has(&surface, premul(theme.palette().success)));
}

#[test]
fn recovery_paints_a_recovery_bead() {
    // A recoverable control shows the recovery bead in the interior; the
    // neutral role keeps the rim off the recovery colour, so the interior
    // hit is unambiguously the bead.
    let theme = Theme::dark();
    let mut button = Button::labelled("Reconnect");
    button.set_state(ControlState::idle().with_recovery(RecoveryState::Recoverable));
    let surface = render(&button, &theme);
    assert!(interior_has(&surface, premul(theme.palette().recovery)));
}

#[test]
fn failed_closed_paints_a_recovery_bead_and_rim() {
    let theme = Theme::dark();
    let mut button = Button::labelled("Retry");
    button.set_state(ControlState::idle().with_authority(AuthorityState::FailedClosed));
    let surface = render(&button, &theme);
    let recovery = premul(theme.palette().recovery);
    // The rim signals the failed-closed disposition, and the interior bead
    // repeats it as a shape so it reads without colour.
    assert_eq!(surface.get(0, H / 2), Some(recovery));
    assert!(interior_has(&surface, recovery));
}

#[test]
fn denied_paints_a_lock_bead_in_the_interior() {
    // A denial marks the interior with a lock bead, distinct from the plain
    // disabled look which carries no bead at all.
    let theme = Theme::dark();
    let mut denied = Button::labelled("Save");
    denied.set_state(ControlState::idle().with_authority(AuthorityState::Denied));
    let mut disabled = Button::labelled("Save");
    disabled.set_state(ControlState::disabled());
    let denied_surface = render(&denied, &theme);
    let disabled_surface = render(&disabled, &theme);
    let lock = premul(theme.palette().denied);
    assert!(interior_has(&denied_surface, lock));
    assert!(!interior_has(&disabled_surface, lock));
}

// --- Focus and pressed appearance --------------------------------------

/// A translucent plate *composited* over the ground beneath it comes back
/// more opaque than the theme authored, and the control then frosts nothing
/// while every colour still looks plausible. It has to be laid down.
#[test]
fn a_button_on_floating_chrome_lays_its_plate_down_rather_than_over_the_ground() {
    for theme in [Theme::dark(), Theme::light()] {
        let palette = theme.palette();
        let want = premul(
            palette
                .surface_raised
                .with_alpha(palette.chrome_plate_alpha),
        );
        let mut surface =
            Surface::filled(W, H, premul(palette.surface)).expect("an opaque ground to draw on");
        Button::labelled("OK").render(
            &mut surface,
            Rect::new(0, 0, W, H),
            Scale::ONE,
            &theme.clone().floating(),
        );

        // Just inside the top border, clear of the centred label.
        let plate = surface.get(W / 2, 2).expect("in bounds");
        assert_eq!(plate, want, "{}: the plate covers", theme.name());
        assert!(plate.a < 255, "{}: the plate covers", theme.name());
        // The rim is a mark, not a background: it stays the edge it was.
        assert_eq!(
            surface.get(0, H / 2),
            Some(premul(palette.rim)),
            "{}: the laid plate ate the rim",
            theme.name()
        );

        // An ordinary button on a window is untouched by the option existing.
        assert_eq!(
            render(&Button::labelled("OK"), &theme).get(W / 2, 2),
            Some(premul(palette.surface_raised)),
            "{}: an ordinary button changed",
            theme.name()
        );
    }
}

#[test]
fn focus_draws_exactly_one_accent_line_and_leaves_the_rim_quiet() {
    let theme = Theme::dark();
    let quiet = premul(theme.palette().rim);
    let accent = premul(theme.palette().rim_active);
    let mut button = Button::labelled("OK");
    let resting = render(&button, &theme);
    button.set_focused(true);
    let focused = render(&button, &theme);

    assert_eq!(resting.get(0, H / 2), Some(quiet));
    assert_eq!(
        focused.get(0, H / 2),
        Some(quiet),
        "the ring is the focus mark, so the perimeter stays the resting edge"
    );
    // One line per side: the ring, a border inside the plate. Counting the
    // row is what tells a single ring from the ring-plus-lifted-rim pair a
    // reader sees as a doubled border.
    let line = |surface: &Surface| {
        (0..W)
            .filter(|x| surface.get(*x, H / 2) == Some(accent))
            .count()
    };
    assert_eq!(line(&resting), 0, "a resting button states nothing");
    assert_eq!(line(&focused), 2, "one accent line down each side");
}

#[test]
fn pressing_darkens_the_plate() {
    let theme = Theme::dark();
    let mut button = Button::labelled("OK");
    let bounds = Rect::new(0, 0, W, H);
    button.on_pointer(&moved(iv(W) / 2, iv(H) / 2), bounds, &mut sink());
    button.on_pointer(&PRESS, bounds, &mut sink());
    let surface = render(&button, &theme);
    assert!(interior_has(
        &surface,
        premul(theme.palette().surface_pressed)
    ));
    assert!(!interior_has(
        &surface,
        premul(theme.palette().surface_raised)
    ));
}

// --- Render-equivalence equality (the host's repaint gate) ----------------

/// The bounds every render-equivalence case draws into.
fn plate() -> Rect {
    Rect::new(0, 0, W, H)
}

/// Paint `draw` into a fresh surface and hand back its pixels.
fn pixels_of(draw: impl FnOnce(&mut Surface)) -> alloc::vec::Vec<Pixel> {
    let mut surface = Surface::new(W, H).expect("surface");
    draw(&mut surface);
    surface.pixels().to_vec()
}

#[test]
fn pointer_position_alone_never_changes_a_button_family_render() {
    let theme = Theme::dark();
    // A move that stays outside the control changes only the recorded
    // coordinate: it leaves the resting hover state exactly as it was.
    let outside = moved(iv(W) + 40, iv(H) + 40);
    let farther = moved(iv(W) + 90, iv(H) + 12);

    let mut a = Button::labelled("OK");
    let mut b = a.clone();
    a.on_pointer(&outside, plate(), &mut sink());
    b.on_pointer(&farther, plate(), &mut sink());
    assert_eq!(a, b, "a coordinate off the plate is not a drawn property");
    assert_eq!(
        pixels_of(|s| a.render(s, plate(), Scale::ONE, &theme)),
        pixels_of(|s| b.render(s, plate(), Scale::ONE, &theme)),
        "…and the two must therefore paint identically"
    );

    let mut a = IconButton::new(IconKind::Bell, ControlRole::Neutral);
    let mut b = a.clone();
    a.on_pointer(&outside, plate(), &mut sink());
    b.on_pointer(&farther, plate(), &mut sink());
    assert_eq!(a, b);
    assert_eq!(
        pixels_of(|s| a.render(s, plate(), Scale::ONE, &theme, None)),
        pixels_of(|s| b.render(s, plate(), Scale::ONE, &theme, None)),
    );

    let mut a = SplitButton::new(
        ButtonContent::Label(alloc::string::String::from("Run")),
        ControlRole::Primary,
    );
    let mut b = a.clone();
    a.on_pointer(&outside, plate(), Scale::ONE, &theme, &mut sink());
    b.on_pointer(&farther, plate(), Scale::ONE, &theme, &mut sink());
    assert_eq!(a, b);
    assert_eq!(
        pixels_of(|s| a.render(s, plate(), Scale::ONE, &theme)),
        pixels_of(|s| b.render(s, plate(), Scale::ONE, &theme)),
    );
}

#[test]
fn press_latch_alone_never_changes_a_button_render() {
    let theme = Theme::dark();
    // One button holds a real press latch; the other is merely *shown*
    // pressed. Only the latch differs, and a latch is not drawn.
    let mut latched = Button::labelled("OK");
    latched.on_pointer(&PRESS, plate(), &mut sink());

    let mut shown = Button::labelled("OK");
    let mut pressed = ControlState::idle();
    pressed.pointer = crate::state::PointerState::Pressed;
    shown.set_state(pressed);

    assert_eq!(latched, shown, "the press latch is not a drawn property");
    assert_eq!(
        pixels_of(|s| latched.render(s, plate(), Scale::ONE, &theme)),
        pixels_of(|s| shown.render(s, plate(), Scale::ONE, &theme)),
        "…and the two must therefore paint identically"
    );
    assert_eq!(
        latched.on_pointer(&RELEASE, plate(), &mut sink()),
        Some(ButtonAction::Activated),
        "the latch still governs activation, it is only invisible"
    );
}

#[test]
fn hover_press_and_focus_each_change_a_button_render() {
    let resting = Button::labelled("OK");

    let mut hovered = resting.clone();
    hovered.on_pointer(&moved(iv(W) / 2, iv(H) / 2), plate(), &mut sink());
    assert_ne!(resting, hovered, "a hover highlight is visible");

    let mut held = hovered.clone();
    held.on_pointer(&PRESS, plate(), &mut sink());
    assert_ne!(hovered, held, "a press is visible");

    let mut focused = resting.clone();
    let mut state = ControlState::idle();
    state.focus = focused_state();
    focused.set_state(state);
    assert_ne!(resting, focused, "a focus ring is visible");
}
