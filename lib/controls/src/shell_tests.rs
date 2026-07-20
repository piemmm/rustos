//! Unit tests for the shell-surface controls (spec §11.25–§11.27, §20).
//!
//! These cover the notification (card composition, source caption, the
//! informational/background/warning/recovery/denied semantics, action
//! routing), the taskbar item (identity, active accent seam, a minimized
//! recess with a non-colour mark, activity seam, attention/recovery/denied
//! beads, and
//! fail-closed activation), and the tray signal (calm capsule, pressure rail,
//! Heat Seam, severity-ordered stacked beads, and the hover/focus readout with
//! its primary action), across both themes, high contrast, and scale.

use alloc::string::String;
use alloc::vec;

use tairix_font::BitmapFont;
use tairix_geometry::{Point, Rect, Scale};
use tairix_icon::IconKind;
use tairix_input::{InputEvent, Key, NamedKey, PointerButton};
use tairix_raster::{Color, Pixel, Surface};
use tairix_theme::{Contrast, Rgba, Theme};

use crate::button::{Button, ButtonContent};
use crate::shell::{
    Notification, NotificationAction, TaskVisibility, TaskbarItem, TaskbarItemAction, TraySignal,
    TraySignalAction,
};
use crate::state::{
    ActivityState, AuthorityState, ControlRole, ControlState, PressureKind, PressureState,
    ProgressValue, RecoveryState, ValidationState,
};

fn font() -> BitmapFont {
    BitmapFont::inconsolata()
}

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

fn high_contrast() -> Theme {
    let base = Theme::dark();
    Theme::new(
        base.id(),
        "Test High Contrast",
        base.appearance(),
        *base.palette(),
        *base.metrics(),
        base.fonts().clone(),
        base.cursors().clone(),
        base.motion(),
        base.density(),
        Contrast::High,
    )
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

// --- Notification (spec §11.25) ----------------------------------------

const NW: u32 = 220;
const NH: u32 = 140;

fn note_surface(note: &Notification, theme: &Theme) -> Surface {
    let mut s = Surface::new(NW, NH).expect("surface");
    note.render(&mut s, Rect::new(0, 0, NW, NH), Scale::ONE, theme, font());
    s
}

#[test]
fn notification_renders_title_in_both_themes() {
    for theme in [Theme::dark(), Theme::light()] {
        let s = note_surface(&Notification::new("Update ready"), &theme);
        assert!(has_pixel(&s, premul(theme.palette().on_surface)));
        assert!(has_pixel(&s, premul(theme.palette().surface_raised)));
    }
}

#[test]
fn notification_source_caption_is_drawn() {
    let theme = Theme::dark();
    let note = Notification::new("Backup finished").with_source("Backup Service");
    let s = note_surface(&note, &theme);
    // The source attribution is drawn in the muted foreground at the top.
    assert!(region_has(
        &s,
        (0, NW),
        (0, 16),
        premul(theme.palette().on_surface_muted)
    ));
    assert_eq!(note.source(), Some("Backup Service"));
}

#[test]
fn notification_warning_shows_warning_rail() {
    let theme = Theme::dark();
    let note = Notification::new("Low space")
        .with_state(ControlState::idle().with_validation(ValidationState::Warning));
    let s = note_surface(&note, &theme);
    assert!(region_has(
        &s,
        (1, 5),
        (20, NH - 20),
        premul(theme.palette().warning)
    ));
}

#[test]
fn notification_background_job_shows_heat_seam() {
    let theme = Theme::dark();
    let note = Notification::new("Copying").with_state(
        ControlState::idle().with_activity(ActivityState::Progress(ProgressValue::FULL)),
    );
    let s = note_surface(&note, &theme);
    assert!(region_has(
        &s,
        (40, NW - 40),
        (NH - 4, NH - 1),
        premul(theme.palette().accent)
    ));
}

#[test]
fn notification_recovery_shows_recovery_bead() {
    let theme = Theme::dark();
    let note = Notification::new("Task hung")
        .with_state(ControlState::idle().with_recovery(RecoveryState::Hung));
    let s = note_surface(&note, &theme);
    assert!(region_has(
        &s,
        (NW / 2, NW),
        (0, 40),
        premul(theme.palette().recovery)
    ));
}

#[test]
fn notification_denied_shows_authority_mark_with_source() {
    let theme = Theme::dark();
    let note = Notification::new("Blocked")
        .with_source("Firewall")
        .with_state(ControlState::idle().with_authority(AuthorityState::Denied));
    let s = note_surface(&note, &theme);
    // The Authority Mark (denied bead) is present alongside the source name.
    assert!(has_pixel(&s, premul(theme.palette().denied)));
    assert!(region_has(
        &s,
        (0, NW),
        (0, 16),
        premul(theme.palette().on_surface_muted)
    ));
}

#[test]
fn notification_action_activates_by_pointer() {
    let theme = Theme::dark();
    let mut note = Notification::new("Job").with_actions(vec![Button::labelled("Clear")]);
    let bounds = Rect::new(0, 0, NW, NH);
    assert_eq!(
        note.on_pointer(&moved(110, 112), bounds, Scale::ONE, &theme, font()),
        None
    );
    assert_eq!(
        note.on_pointer(&PRESS, bounds, Scale::ONE, &theme, font()),
        None
    );
    assert_eq!(
        note.on_pointer(&RELEASE, bounds, Scale::ONE, &theme, font()),
        Some(NotificationAction::ActionActivated { index: 0 })
    );
}

#[test]
fn notification_action_activates_by_keyboard() {
    let mut note = Notification::new("Job").with_actions(vec![Button::new(
        ButtonContent::Label(String::from("Recover")),
        ControlRole::Recovery,
    )]);
    note.actions_mut()[0].set_focused(true);
    assert_eq!(
        note.on_key(Key::Named(NamedKey::Enter)),
        Some(NotificationAction::ActionActivated { index: 0 })
    );
}

#[test]
fn notification_count_badge_renders() {
    let theme = Theme::dark();
    let s = note_surface(&Notification::new("Inbox").with_count(3), &theme);
    assert!(has_pixel(&s, premul(theme.palette().on_accent)));
}

#[test]
fn notification_renders_at_scale_without_panic() {
    let theme = Theme::dark();
    let mut s = Surface::new(NW * 2, NH * 2).expect("surface");
    Notification::new("Scaled")
        .with_source("svc")
        .with_message("body")
        .render(
            &mut s,
            Rect::new(0, 0, NW * 2, NH * 2),
            scale2(),
            &theme,
            font(),
        );
    assert!(has_pixel(&s, premul(theme.palette().on_surface)));
}

// --- TaskbarItem (spec §11.26) -----------------------------------------

const TW: u32 = 160;
const TH: u32 = 32;

fn task_surface(item: &TaskbarItem, theme: &Theme, scale: Scale) -> Surface {
    let (w, h) = (
        TW * scale.scale_length(1000) / 1000,
        TH * scale.scale_length(1000) / 1000,
    );
    let (w, h) = (w.max(TW), h.max(TH));
    let mut s = Surface::new(w, h).expect("surface");
    item.render(&mut s, Rect::new(0, 0, w, h), scale, theme, font());
    s
}

#[test]
fn taskbar_item_renders_identity_in_both_themes() {
    for theme in [Theme::dark(), Theme::light()] {
        let item = TaskbarItem::new("Editor", IconKind::Generic);
        let s = task_surface(&item, &theme, Scale::ONE);
        assert!(has_pixel(&s, premul(theme.palette().on_surface)));
        assert!(has_pixel(&s, premul(theme.palette().surface_raised)));
    }
}

#[test]
fn taskbar_item_active_shows_lower_accent_seam() {
    let theme = Theme::dark();
    let item =
        TaskbarItem::new("Editor", IconKind::Generic).with_visibility(TaskVisibility::Active);
    let s = task_surface(&item, &theme, Scale::ONE);
    assert!(region_has(
        &s,
        (TW / 3, TW - TW / 3),
        (TH - 3, TH - 1),
        premul(theme.palette().accent)
    ));
}

#[test]
fn taskbar_item_minimized_recesses_and_marks() {
    let theme = Theme::dark();
    let item =
        TaskbarItem::new("Editor", IconKind::Generic).with_visibility(TaskVisibility::Minimized);
    let s = task_surface(&item, &theme, Scale::ONE);
    // Recessed plate (flat surface, not raised) and a leading non-colour tick.
    assert!(has_pixel(&s, premul(theme.palette().surface)));
    assert!(has_pixel(&s, premul(theme.palette().on_surface_muted)));
}

#[test]
fn taskbar_item_attention_shows_bead() {
    let theme = Theme::dark();
    let item = TaskbarItem::new("Chat", IconKind::Bell).with_attention(true);
    let s = task_surface(&item, &theme, Scale::ONE);
    assert!(region_has(
        &s,
        (TW / 2, TW),
        (0, TH / 2),
        premul(theme.palette().accent)
    ));
}

#[test]
fn taskbar_item_recovery_bead_takes_priority_over_attention() {
    let theme = Theme::dark();
    let item = TaskbarItem::new("Hung", IconKind::Generic)
        .with_attention(true)
        .with_state(ControlState::idle().with_recovery(RecoveryState::Hung));
    let s = task_surface(&item, &theme, Scale::ONE);
    assert!(has_pixel(&s, premul(theme.palette().recovery)));
}

#[test]
fn taskbar_item_denied_shows_lock_and_does_not_activate() {
    let theme = Theme::dark();
    let mut item = TaskbarItem::new("Locked", IconKind::Generic)
        .with_state(ControlState::idle().with_authority(AuthorityState::Denied));
    let s = task_surface(&item, &theme, Scale::ONE);
    assert!(has_pixel(&s, premul(theme.palette().denied)));
    let bounds = Rect::new(0, 0, TW, TH);
    assert_eq!(item.on_pointer(&moved(80, 16), bounds), None);
    assert_eq!(item.on_pointer(&PRESS, bounds), None);
    // Fail closed: a denied item never activates.
    assert_eq!(item.on_pointer(&RELEASE, bounds), None);
}

#[test]
fn taskbar_item_activates_by_pointer_and_keyboard() {
    let mut item = TaskbarItem::new("Editor", IconKind::Generic);
    let bounds = Rect::new(0, 0, TW, TH);
    assert_eq!(item.on_pointer(&moved(80, 16), bounds), None);
    assert_eq!(item.on_pointer(&PRESS, bounds), None);
    assert_eq!(
        item.on_pointer(&RELEASE, bounds),
        Some(TaskbarItemAction::Activated)
    );
    item.set_focused(true);
    assert_eq!(
        item.on_key(Key::Char(' ')),
        Some(TaskbarItemAction::Activated)
    );
}

#[test]
fn taskbar_item_high_contrast_and_scale_render() {
    let hc = high_contrast();
    let item =
        TaskbarItem::new("Editor", IconKind::Generic).with_visibility(TaskVisibility::Active);
    let s = task_surface(&item, &hc, scale2());
    assert!(has_pixel(&s, premul(hc.palette().on_surface)));
}

// --- TraySignal (spec §11.27) ------------------------------------------

const SS: u32 = 32;

fn tray_surface(sig: &TraySignal, theme: &Theme) -> Surface {
    let mut s = Surface::new(SS, SS).expect("surface");
    sig.render(&mut s, Rect::new(0, 0, SS, SS), Scale::ONE, theme, font());
    s
}

#[test]
fn tray_signal_renders_calm_capsule_in_both_themes() {
    for theme in [Theme::dark(), Theme::light()] {
        let sig = TraySignal::new(IconKind::Network, "Network");
        let s = tray_surface(&sig, &theme);
        assert!(has_pixel(&s, premul(theme.palette().rim)));
    }
}

#[test]
fn tray_signal_pressure_shows_leading_rail() {
    let theme = Theme::dark();
    let sig = TraySignal::new(IconKind::Generic, "CPU")
        .with_state(ControlState::idle().with_pressure(PressureState::Under(PressureKind::Cpu)));
    let s = tray_surface(&sig, &theme);
    assert!(region_has(
        &s,
        (1, 5),
        (4, SS - 4),
        premul(theme.palette().cpu_pressure)
    ));
}

#[test]
fn tray_signal_background_work_shows_lower_seam() {
    let theme = Theme::dark();
    let sig = TraySignal::new(IconKind::Network, "Sync")
        .with_state(ControlState::idle().with_activity(ActivityState::Working));
    let s = tray_surface(&sig, &theme);
    assert!(region_has(
        &s,
        (2, SS - 2),
        (SS - 4, SS - 1),
        premul(theme.palette().accent)
    ));
}

#[test]
fn tray_signal_stacks_severity_ordered_beads() {
    let theme = Theme::dark();
    // A denied + warning signal stacks both beads (denied is highest severity).
    let sig = TraySignal::new(IconKind::Generic, "Multi").with_state(
        ControlState::idle()
            .with_authority(AuthorityState::Denied)
            .with_validation(ValidationState::Warning),
    );
    let s = tray_surface(&sig, &theme);
    assert!(has_pixel(&s, premul(theme.palette().denied)));
    assert!(has_pixel(&s, premul(theme.palette().warning)));
}

#[test]
fn tray_signal_expands_on_hover_and_focus() {
    let theme = Theme::dark();
    let mut sig = TraySignal::new(IconKind::Battery, "Battery").with_value("82%");
    assert!(!sig.is_expanded());
    let capsule = Rect::new(0, 0, SS, SS);
    let readout = Rect::new(0, iv(SS), 120, 60);
    // Hovering the capsule expands the readout.
    let _ = sig.on_pointer(&moved(10, 10), capsule, readout, Scale::ONE, &theme);
    assert!(sig.is_expanded());
    // Focus alone also expands.
    let _ = sig.on_pointer(&moved(500, 500), capsule, readout, Scale::ONE, &theme);
    assert!(!sig.is_expanded());
    sig.set_focused(true);
    assert!(sig.is_expanded());
}

#[test]
fn tray_signal_readout_renders_name_value_and_action() {
    let theme = Theme::dark();
    let sig = TraySignal::new(IconKind::Battery, "Battery")
        .with_value("82%")
        .with_action(Button::labelled("Details"));
    let (w, h) = sig.readout_size(Scale::ONE, &theme, font());
    let mut s = Surface::new(w, h).expect("surface");
    sig.render_readout(&mut s, Rect::new(0, 0, w, h), Scale::ONE, &theme, font());
    assert!(has_pixel(&s, premul(theme.palette().on_surface)));
    assert!(has_pixel(&s, premul(theme.palette().on_surface_muted)));
    assert!(has_pixel(&s, premul(theme.palette().surface_raised)));
}

#[test]
fn tray_signal_readout_action_activates() {
    let theme = Theme::dark();
    let mut sig =
        TraySignal::new(IconKind::Battery, "Battery").with_action(Button::labelled("Fix"));
    sig.set_focused(true);
    let capsule = Rect::new(0, 0, SS, SS);
    let (w, h) = sig.readout_size(Scale::ONE, &theme, font());
    let rh = h.max(48);
    let readout = Rect::new(0, iv(SS), w.max(80), rh);
    let m = theme.metrics();
    // Click the vertical centre of the readout's bottom action band.
    let by = SS + rh - m.control_inset - m.control_height / 2;
    assert_eq!(
        sig.on_pointer(&moved(20, iv(by)), capsule, readout, Scale::ONE, &theme),
        None
    );
    assert_eq!(
        sig.on_pointer(&PRESS, capsule, readout, Scale::ONE, &theme),
        None
    );
    assert_eq!(
        sig.on_pointer(&RELEASE, capsule, readout, Scale::ONE, &theme),
        Some(TraySignalAction::Activated)
    );
}

#[test]
fn tray_signal_readout_action_activates_by_keyboard() {
    let mut sig =
        TraySignal::new(IconKind::Battery, "Battery").with_action(Button::labelled("Fix"));
    sig.set_focused(true);
    assert_eq!(
        sig.on_key(Key::Named(NamedKey::Enter)),
        Some(TraySignalAction::Activated)
    );
}
