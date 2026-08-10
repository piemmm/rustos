//! Unit tests for the decision-surface controls (spec §11.24, §11.32, §20).
//!
//! These cover the dialog (title/message, Action Warmth for a recommended
//! action vs a destructive one, the spec §13 denial rendering, the inline reason,
//! right-aligned action layout, and pointer/keyboard activation), the tooltip
//! (short anchored text, sizing), and the help tip (reason tone by role and the
//! one safe next-step action), across both themes, high contrast, and scale.

use alloc::string::String;
use alloc::vec;

use tairix_geometry::{Point, Rect, Scale};
use tairix_input::{InputEvent, Key, NamedKey, PointerButton};
use tairix_raster::{Color, Pixel, Surface};
use tairix_theme::{Rgba, Theme};

use crate::button::{Button, ButtonContent};
use crate::damage::sink;
use crate::decision::{Dialog, DialogAction, HelpTip, HelpTipAction, Tooltip};
use crate::state::{AuthorityState, ControlRole, ControlState};
use crate::testkit::high_contrast;

fn scale2() -> Scale {
    Scale::from_percent(200).expect("valid scale")
}

fn iv(v: u32) -> i32 {
    i32::try_from(v).expect("fits in i32")
}

fn premul(rgba: Rgba) -> Pixel {
    Color::from(rgba).premultiply()
}

fn has_pixel(surface: &Surface, want: Pixel) -> bool {
    surface.pixels().contains(&want)
}

fn region_has(surface: &Surface, xr: (u32, u32), yr: (u32, u32), want: Pixel) -> bool {
    (xr.0..xr.1)
        .flat_map(|x| (yr.0..yr.1).map(move |y| (x, y)))
        .any(|(x, y)| surface.get(x, y) == Some(want))
}

fn moved(x: i32, y: i32) -> InputEvent {
    InputEvent::PointerMoved {
        to: Point::new(x, y),
    }
}

const PRESS: InputEvent = InputEvent::PointerPressed {
    button: PointerButton::Primary,
};
const RELEASE: InputEvent = InputEvent::PointerReleased {
    button: PointerButton::Primary,
};

// --- Dialog (spec §11.24) ----------------------------------------------

const DW: u32 = 320;
const DH: u32 = 160;

fn dialog_surface(dialog: &Dialog, theme: &Theme) -> Surface {
    let mut s = Surface::new(DW, DH).expect("surface");
    dialog.render(&mut s, Rect::new(0, 0, DW, DH), Scale::ONE, theme);
    s
}

#[test]
fn dialog_renders_title_and_message_in_both_themes() {
    for theme in [Theme::dark(), Theme::light()] {
        let dialog = Dialog::new("Delete file?").with_message("This cannot be undone.");
        let s = dialog_surface(&dialog, &theme);
        assert!(has_pixel(&s, premul(theme.palette().on_surface)));
        assert!(has_pixel(&s, premul(theme.palette().on_surface_muted)));
        assert!(has_pixel(&s, premul(theme.palette().surface_raised)));
    }
}

#[test]
fn dialog_recommended_action_is_warm() {
    let theme = Theme::dark();
    let dialog = Dialog::new("Save?").with_actions(vec![Button::new(
        ButtonContent::Label(String::from("Save")),
        ControlRole::Recommended,
    )]);
    let s = dialog_surface(&dialog, &theme);
    // A recommended action takes the warm accent rim, in the action band.
    assert!(region_has(
        &s,
        (0, DW),
        (DH - 30, DH),
        premul(theme.palette().accent)
    ));
}

#[test]
fn dialog_destructive_action_uses_danger_posture() {
    let theme = Theme::dark();
    let mut delete = Button::new(
        ButtonContent::Label(String::from("Delete")),
        ControlRole::Destructive,
    );
    delete.set_state(ControlState::idle().with_authority(AuthorityState::NeedsConfirmation));
    let dialog = Dialog::new("Delete?").with_actions(vec![delete]);
    let s = dialog_surface(&dialog, &theme);
    assert!(has_pixel(&s, premul(theme.palette().danger)));
}

#[test]
fn dialog_denied_action_shows_lock_and_does_not_activate() {
    let theme = Theme::dark();
    let mut denied = Button::new(
        ButtonContent::Label(String::from("Apply")),
        ControlRole::Primary,
    );
    denied.set_state(ControlState::idle().with_authority(AuthorityState::Denied));
    let mut dialog = Dialog::new("Settings").with_actions(vec![denied]);
    let s = dialog_surface(&dialog, &theme);
    assert!(has_pixel(&s, premul(theme.palette().denied)));
    // Fail closed: clicking a denied action never activates it.
    let bounds = Rect::new(0, 0, DW, DH);
    assert_eq!(
        dialog.on_pointer(
            &moved(iv(DW) - 15, iv(DH) - 15),
            bounds,
            Scale::ONE,
            &theme,
            &mut sink()
        ),
        None
    );
    assert_eq!(
        dialog.on_pointer(&PRESS, bounds, Scale::ONE, &theme, &mut sink()),
        None
    );
    assert_eq!(
        dialog.on_pointer(&RELEASE, bounds, Scale::ONE, &theme, &mut sink()),
        None
    );
}

#[test]
fn dialog_reason_line_is_drawn() {
    let theme = Theme::dark();
    let dialog = Dialog::new("Blocked")
        .with_reason("requires system permission")
        .with_actions(vec![Button::labelled("OK")]);
    let s = dialog_surface(&dialog, &theme);
    assert!(has_pixel(&s, premul(theme.palette().warning)));
}

#[test]
fn dialog_action_activates_by_pointer() {
    let theme = Theme::dark();
    let mut dialog = Dialog::new("Confirm").with_actions(vec![Button::labelled("OK")]);
    let bounds = Rect::new(0, 0, DW, DH);
    let (cx, cy) = (iv(DW) - 15, iv(DH) - 15);
    assert_eq!(
        dialog.on_pointer(&moved(cx, cy), bounds, Scale::ONE, &theme, &mut sink()),
        None
    );
    assert_eq!(
        dialog.on_pointer(&PRESS, bounds, Scale::ONE, &theme, &mut sink()),
        None
    );
    assert_eq!(
        dialog.on_pointer(&RELEASE, bounds, Scale::ONE, &theme, &mut sink()),
        Some(DialogAction::ActionActivated { index: 0 })
    );
}

#[test]
fn dialog_action_activates_by_keyboard() {
    let mut dialog = Dialog::new("Confirm")
        .with_actions(vec![Button::labelled("Cancel"), Button::labelled("OK")]);
    dialog.actions_mut()[1].set_focused(true);
    assert_eq!(
        dialog.on_key(Key::Named(NamedKey::Enter)),
        Some(DialogAction::ActionActivated { index: 1 })
    );
}

#[test]
fn dialog_action_rects_match_the_pointer_hit_geometry() {
    let theme = Theme::dark();
    let bounds = Rect::new(0, 0, DW, DH);
    let mut dialog = Dialog::new("Confirm")
        .with_actions(vec![Button::labelled("Cancel"), Button::labelled("OK")]);
    let rects = dialog.action_rects(bounds, Scale::ONE, &theme);
    // One rect per action, in action order.
    assert_eq!(rects.len(), 2);
    // A move-then-press-then-release at the reported centre of action 1
    // activates action 1, proving `action_rects` reports the same geometry
    // `on_pointer` routes clicks through (one definition, no divergence).
    let target = rects[1];
    let cx = target.origin.x + iv(target.width) / 2;
    let cy = target.origin.y + iv(target.height) / 2;
    assert_eq!(
        dialog.on_pointer(&moved(cx, cy), bounds, Scale::ONE, &theme, &mut sink()),
        None
    );
    assert_eq!(
        dialog.on_pointer(&PRESS, bounds, Scale::ONE, &theme, &mut sink()),
        None
    );
    assert_eq!(
        dialog.on_pointer(&RELEASE, bounds, Scale::ONE, &theme, &mut sink()),
        Some(DialogAction::ActionActivated { index: 1 })
    );
}

#[test]
fn dialog_action_rects_are_empty_when_the_plate_has_no_interior() {
    let theme = Theme::dark();
    let dialog = Dialog::new("Confirm").with_actions(vec![Button::labelled("OK")]);
    // A zero-area plate has no drawable interior, so there are no button rects
    // (fail closed) rather than a phantom placement.
    let rects = dialog.action_rects(Rect::new(0, 0, 0, 0), Scale::ONE, &theme);
    assert!(rects.is_empty());
}

#[test]
fn dialog_actions_are_right_aligned() {
    let theme = Theme::dark();
    let dialog = Dialog::new("Q").with_actions(vec![Button::new(
        ButtonContent::Label(String::from("Go")),
        ControlRole::Recommended,
    )]);
    let s = dialog_surface(&dialog, &theme);
    // The single trailing action's accent rim is in the right half, not the
    // left half, of the action band.
    assert!(region_has(
        &s,
        (DW / 2, DW),
        (DH - 30, DH),
        premul(theme.palette().accent)
    ));
    assert!(!region_has(
        &s,
        (2, DW / 4),
        (DH - 30, DH),
        premul(theme.palette().accent)
    ));
}

#[test]
fn dialog_high_contrast_and_scale_render() {
    let hc = high_contrast();
    let mut s = Surface::new(DW * 2, DH * 2).expect("surface");
    Dialog::new("Scaled")
        .with_message("body")
        .with_actions(vec![Button::labelled("OK")])
        .render(&mut s, Rect::new(0, 0, DW * 2, DH * 2), scale2(), &hc);
    assert!(has_pixel(&s, premul(hc.palette().on_surface)));
}

// --- Tooltip (spec §11.32) ---------------------------------------------

#[test]
fn tooltip_renders_text_in_both_themes() {
    for theme in [Theme::dark(), Theme::light()] {
        let tip = Tooltip::new("Save the document");
        let (w, h) = tip.preferred_size(Scale::ONE, &theme);
        let mut s = Surface::new(w, h).expect("surface");
        tip.render(&mut s, Rect::new(0, 0, w, h), Scale::ONE, &theme);
        assert!(has_pixel(&s, premul(theme.palette().on_surface)));
        assert!(has_pixel(&s, premul(theme.palette().surface_raised)));
        assert_eq!(tip.text(), "Save the document");
    }
}

#[test]
fn tooltip_preferred_size_is_positive() {
    let theme = Theme::dark();
    let (w, h) = Tooltip::new("hi").preferred_size(Scale::ONE, &theme);
    assert!(w > 0 && h > 0);
}

#[test]
fn tooltip_renders_in_tight_bounds_without_panic() {
    let theme = Theme::dark();
    let mut s = Surface::new(12, 8).expect("surface");
    Tooltip::new("a long tooltip that will not fit").render(
        &mut s,
        Rect::new(0, 0, 12, 8),
        Scale::ONE,
        &theme,
    );
}

// --- HelpTip (spec §11.32) ---------------------------------------------

const HW: u32 = 200;
const HH: u32 = 90;

#[test]
fn helptip_neutral_reason_is_cautionary() {
    let theme = Theme::dark();
    let tip = HelpTip::new("This action is not available yet.");
    let mut s = Surface::new(HW, HH).expect("surface");
    tip.render(&mut s, Rect::new(0, 0, HW, HH), Scale::ONE, &theme);
    assert!(has_pixel(&s, premul(theme.palette().warning)));
}

#[test]
fn helptip_recommended_reason_is_accent() {
    let theme = Theme::dark();
    let tip = HelpTip::new("Recommended for security.").with_role(ControlRole::Recommended);
    let mut s = Surface::new(HW, HH).expect("surface");
    tip.render(&mut s, Rect::new(0, 0, HW, HH), Scale::ONE, &theme);
    assert!(has_pixel(&s, premul(theme.palette().accent)));
}

#[test]
fn helptip_step_activates_by_pointer() {
    let theme = Theme::dark();
    let mut tip = HelpTip::new("Blocked.").with_step(Button::labelled("Grant"));
    let bounds = Rect::new(0, 0, HW, HH);
    let (cx, cy) = (30, iv(HH) - 15);
    assert_eq!(
        tip.on_pointer(&moved(cx, cy), bounds, Scale::ONE, &theme, &mut sink()),
        None
    );
    assert_eq!(
        tip.on_pointer(&PRESS, bounds, Scale::ONE, &theme, &mut sink()),
        None
    );
    assert_eq!(
        tip.on_pointer(&RELEASE, bounds, Scale::ONE, &theme, &mut sink()),
        Some(HelpTipAction::NextStep)
    );
}

#[test]
fn helptip_step_activates_by_keyboard() {
    let mut step = Button::labelled("Grant");
    step.set_focused(true);
    let mut tip = HelpTip::new("Blocked.").with_step(step);
    assert_eq!(tip.on_key(Key::Char(' ')), Some(HelpTipAction::NextStep));
}

#[test]
fn helptip_high_contrast_and_scale_render() {
    let hc = high_contrast();
    let mut s = Surface::new(HW * 2, HH * 2).expect("surface");
    HelpTip::new("Reason")
        .with_step(Button::labelled("Fix"))
        .render(&mut s, Rect::new(0, 0, HW * 2, HH * 2), scale2(), &hc);
    assert!(has_pixel(&s, premul(hc.palette().warning)));
}
