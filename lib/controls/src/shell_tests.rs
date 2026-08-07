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

use tairix_geometry::{Point, Rect, Scale};
use tairix_icon::IconKind;
use tairix_input::{InputEvent, Key, NamedKey, PointerButton};
use tairix_raster::{Color, Pixel, Surface};
use tairix_theme::{Rgba, Theme};

use crate::button::{Button, ButtonContent};
use crate::shell::{
    Notification, NotificationAction, TaskVisibility, TaskbarItem, TaskbarItemAction,
    TaskbarPresentation, TrayBadge, TrayBadgeContent, TrayBadgeTone, TraySignal, TraySignalAction,
};
use crate::state::{
    ActivityState, AuthorityState, ControlRole, ControlState, PointerState, PressureKind,
    PressureState, ProgressValue, RecoveryState, ValidationState,
};
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

// --- Notification (spec §11.25) ----------------------------------------

const NW: u32 = 220;
const NH: u32 = 140;

fn note_surface(note: &Notification, theme: &Theme) -> Surface {
    let mut s = Surface::new(NW, NH).expect("surface");
    note.render(&mut s, Rect::new(0, 0, NW, NH), Scale::ONE, theme);
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
        note.on_pointer(&moved(110, 112), bounds, Scale::ONE, &theme),
        None
    );
    assert_eq!(note.on_pointer(&PRESS, bounds, Scale::ONE, &theme), None);
    assert_eq!(
        note.on_pointer(&RELEASE, bounds, Scale::ONE, &theme),
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
        .render(&mut s, Rect::new(0, 0, NW * 2, NH * 2), scale2(), &theme);
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
    item.render(&mut s, Rect::new(0, 0, w, h), scale, theme, None);
    s
}

#[test]
fn taskbar_item_rests_bare_on_the_bar_and_marks_its_presence() {
    for theme in [Theme::dark(), Theme::light()] {
        let item = TaskbarItem::new("Editor", IconKind::Generic);
        let s = task_surface(&item, &theme, Scale::ONE);
        // Its identity is drawn...
        assert!(has_pixel(&s, premul(theme.palette().on_surface)));
        // ...and nothing else is. A resting item is seated *in* the bar, so it
        // paints neither a plate nor a rim and the bar shows through untouched
        // (this surface starts empty, so an unpainted pixel is a transparent
        // one).
        assert!(
            !has_pixel(&s, premul(theme.palette().surface_raised)),
            "{}: a resting item must not plate itself",
            theme.name()
        );
        assert!(
            !has_pixel(&s, premul(theme.palette().rim)),
            "{}: a bar-seated item wears no rim",
            theme.name()
        );
        // A running window states itself with a short muted mark on the lower
        // edge instead — the only thing that tells it from a closed pin.
        assert!(
            region_has(
                &s,
                (TW / 3, TW - TW / 3),
                (TH - 3, TH - 1),
                premul(theme.palette().on_surface_muted)
            ),
            "{}: a running item shows its presence mark",
            theme.name()
        );
    }
}

#[test]
fn taskbar_item_presence_mark_tells_running_from_closed_and_active() {
    let theme = Theme::dark();
    let palette = theme.palette();
    let lower = (TH - 3, TH - 1);
    let leading = (0, TW / 4);

    // A closed pin marks nothing at all: no window, no presence.
    let closed = task_surface(
        &TaskbarItem::new("Editor", IconKind::Generic).with_visibility(TaskVisibility::Closed),
        &theme,
        Scale::ONE,
    );
    assert!(!region_has(
        &closed,
        (0, TW),
        lower,
        premul(palette.on_surface_muted)
    ));
    assert!(!region_has(&closed, (0, TW), lower, premul(palette.accent)));

    // A running window's mark is short and centred, so the leading end of the
    // lower edge stays clear — the active window's full-width seam does not.
    let running = task_surface(
        &TaskbarItem::new("Editor", IconKind::Generic).with_visibility(TaskVisibility::Running),
        &theme,
        Scale::ONE,
    );
    assert!(!region_has(
        &running,
        leading,
        lower,
        premul(palette.on_surface_muted)
    ));
    let active = task_surface(
        &TaskbarItem::new("Editor", IconKind::Generic).with_visibility(TaskVisibility::Active),
        &theme,
        Scale::ONE,
    );
    assert!(region_has(&active, leading, lower, premul(palette.accent)));
}

#[test]
fn taskbar_item_washes_its_plate_under_the_pointer_without_an_edge() {
    for theme in [Theme::dark(), Theme::light()] {
        let palette = theme.palette();
        let hovered = task_surface(
            &TaskbarItem::new("Editor", IconKind::Generic)
                .with_state(ControlState::idle().with_pointer(PointerState::Hover)),
            &theme,
            Scale::ONE,
        );
        assert!(
            has_pixel(&hovered, premul(palette.surface_hover)),
            "{}: hover raises the plate as a wash",
            theme.name()
        );
        assert!(
            !has_pixel(&hovered, premul(palette.rim_active)),
            "{}: and never as an edge",
            theme.name()
        );
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

/// A square pinned-shortcut slot.
const PS: u32 = 40;

fn pin_item() -> TaskbarItem {
    TaskbarItem::new("Editor", IconKind::AppBundle).with_presentation(TaskbarPresentation::Icon)
}

#[test]
fn taskbar_item_icon_presentation_centres_a_plate_sized_glyph() {
    let theme = Theme::dark();
    let bounds = Rect::new(0, 0, PS, PS);
    let wide = Rect::new(0, 0, PS * 2, PS * 2);
    let labelled = TaskbarItem::new("Editor", IconKind::AppBundle);
    let icon_only = pin_item();
    // The compact look sizes the glyph off the plate (it grows with the
    // slot); the labelled look stays bound to the text line regardless.
    assert!(
        icon_only.icon_side(wide, Scale::ONE, &theme)
            > icon_only.icon_side(bounds, Scale::ONE, &theme)
    );
    assert_eq!(
        labelled.icon_side(wide, Scale::ONE, &theme),
        labelled.icon_side(bounds, Scale::ONE, &theme)
    );
    let mut s = Surface::new(PS, PS).expect("surface");
    icon_only.render(&mut s, bounds, Scale::ONE, &theme, None);
    // Glyph ink sits in the centre of the plate.
    assert!(region_has(
        &s,
        (PS / 3, PS - PS / 3),
        (PS / 3, PS - PS / 3),
        premul(theme.palette().on_surface)
    ));
}

#[test]
fn taskbar_item_icon_side_is_zero_for_degenerate_bounds() {
    let theme = Theme::dark();
    assert_eq!(
        pin_item().icon_side(Rect::new(-4, -4, 0, 0), Scale::ONE, &theme),
        0
    );
}

#[test]
fn taskbar_item_artwork_replaces_the_builtin_glyph_in_both_presentations() {
    let theme = Theme::dark();
    let magenta = Color::rgb(255, 0, 255).premultiply();
    // A compact square pin slot and a wide labelled task slot both paint the
    // owner-supplied artwork in place of the built-in class glyph.
    let cases = [
        (pin_item(), Rect::new(0, 0, PS, PS), PS, PS),
        (
            TaskbarItem::new("Editor", IconKind::AppBundle),
            Rect::new(0, 0, TW, TH),
            TW,
            TH,
        ),
    ];
    for (item, bounds, w, h) in cases {
        let side = item.icon_side(bounds, Scale::ONE, &theme);
        assert!(side > 0);
        let art = Surface::filled(side, side, magenta).expect("artwork");
        let mut s = Surface::new(w, h).expect("surface");
        item.render(&mut s, bounds, Scale::ONE, &theme, Some(&art));
        assert!(has_pixel(&s, magenta));
    }
}

#[test]
fn taskbar_item_closed_rests_quiet_and_plates_on_hover() {
    let theme = Theme::dark();
    let bounds = Rect::new(0, 0, PS, PS);
    let closed = pin_item().with_visibility(TaskVisibility::Closed);
    let mut rest = Surface::new(PS, PS).expect("surface");
    closed.render(&mut rest, bounds, Scale::ONE, &theme, None);
    // At rest a closed pin shows no plate or rim — only the glyph sits on
    // the bar — so it never masquerades as a running task.
    assert!(!has_pixel(&rest, premul(theme.palette().surface_raised)));
    assert!(!has_pixel(&rest, premul(theme.palette().rim)));
    assert!(has_pixel(&rest, premul(theme.palette().on_surface)));
    // Hover raises the plate like any other slot.
    let hovered = closed.with_state(ControlState::idle().with_pointer(PointerState::Hover));
    let mut hover = Surface::new(PS, PS).expect("surface");
    hovered.render(&mut hover, bounds, Scale::ONE, &theme, None);
    assert_ne!(rest.pixels(), hover.pixels());
    // A denied closed pin still shows its plate and lock bead (a marked
    // state is never hidden by the quiet resting look).
    let denied = pin_item()
        .with_visibility(TaskVisibility::Closed)
        .with_state(ControlState::idle().with_authority(AuthorityState::Denied));
    let mut d = Surface::new(PS, PS).expect("surface");
    denied.render(&mut d, bounds, Scale::ONE, &theme, None);
    assert!(has_pixel(&d, premul(theme.palette().denied)));
}

#[test]
fn taskbar_item_icon_presentation_keeps_status_furniture() {
    let theme = Theme::dark();
    let bounds = Rect::new(0, 0, PS, PS);
    // The active seam still paints along the bottom edge of a compact slot.
    let active = pin_item().with_visibility(TaskVisibility::Active);
    let mut s = Surface::new(PS, PS).expect("surface");
    active.render(&mut s, bounds, Scale::ONE, &theme, None);
    assert!(region_has(
        &s,
        (PS / 3, PS - PS / 3),
        (PS - 3, PS - 1),
        premul(theme.palette().accent)
    ));
    // A denied compact slot still shows the lock bead and refuses to act.
    let mut denied =
        pin_item().with_state(ControlState::idle().with_authority(AuthorityState::Denied));
    let mut d = Surface::new(PS, PS).expect("surface");
    denied.render(&mut d, bounds, Scale::ONE, &theme, None);
    assert!(has_pixel(&d, premul(theme.palette().denied)));
    assert_eq!(denied.on_pointer(&moved(20, 20), bounds), None);
    assert_eq!(denied.on_pointer(&PRESS, bounds), None);
    assert_eq!(denied.on_pointer(&RELEASE, bounds), None);
}

// --- TraySignal (spec §11.27) ------------------------------------------

const SS: u32 = 32;

fn tray_surface(sig: &TraySignal, theme: &Theme) -> Surface {
    let mut s = Surface::new(SS, SS).expect("surface");
    sig.render(&mut s, Rect::new(0, 0, SS, SS), Scale::ONE, theme);
    s
}

#[test]
fn tray_signal_rests_bare_on_the_bar_in_both_themes() {
    for theme in [Theme::dark(), Theme::light()] {
        let sig = TraySignal::new(IconKind::Network, "Network");
        let s = tray_surface(&sig, &theme);
        // The capsule is seated in the bar like every other icon on it: a calm
        // signal is its glyph alone, with no perimeter and no plate of its own.
        assert!(has_pixel(&s, premul(theme.palette().on_surface)));
        assert!(
            !has_pixel(&s, premul(theme.palette().rim)),
            "{}: the capsule wears no rim",
            theme.name()
        );
        assert!(
            !has_pixel(&s, premul(theme.palette().surface_raised)),
            "{}: nor a plate while it is calm",
            theme.name()
        );
    }
}

#[test]
fn tray_signal_washes_its_plate_under_the_pointer_without_an_edge() {
    for theme in [Theme::dark(), Theme::light()] {
        let sig = TraySignal::new(IconKind::Network, "Network")
            .with_state(ControlState::idle().with_pointer(PointerState::Hover));
        let s = tray_surface(&sig, &theme);
        assert!(
            has_pixel(&s, premul(theme.palette().surface_hover)),
            "{}: hover raises the plate as a wash",
            theme.name()
        );
        assert!(
            !has_pixel(&s, premul(theme.palette().rim_active)),
            "{}: and never as an edge",
            theme.name()
        );
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
fn tray_signal_badge_paints_each_tone() {
    let theme = Theme::dark();
    let palette = theme.palette();
    let tones = [
        (TrayBadgeTone::Accent, palette.accent),
        (TrayBadgeTone::Warning, palette.warning),
        (TrayBadgeTone::Danger, palette.danger),
        (TrayBadgeTone::Recovery, palette.recovery),
    ];
    for (tone, fill) in tones {
        let sig = TraySignal::new(IconKind::Network, "Network")
            .with_badge(TrayBadge::new(TrayBadgeContent::Count(2), tone));
        let s = tray_surface(&sig, &theme);
        // The badge sits on the capsule's top-trailing corner and its fill
        // differs from the plain plate/rim colours.
        assert!(region_has(&s, (SS / 2, SS), (0, SS / 2), premul(fill)));
        assert_ne!(fill, palette.surface_raised);
        assert_ne!(fill, palette.rim);
    }
}

#[test]
fn tray_signal_badge_content_count_differs_from_alert() {
    let theme = Theme::dark();
    // The host glyph transport paints every non-space scalar as one solid
    // block (glyph fidelity is fontd's own tested contract), so a digit and
    // an exclamation mark are pixel-identical here. Distinguishability is
    // proven at the seam this crate owns — each content commissions a
    // distinct scalar — plus the on-accent ink showing the commissioned text
    // reaches the badge.
    assert_eq!(TrayBadgeContent::Count(3).text(), "3");
    assert_eq!(TrayBadgeContent::Alert.text(), "!");
    for content in [TrayBadgeContent::Count(3), TrayBadgeContent::Alert] {
        let sig = TraySignal::new(IconKind::Network, "Network")
            .with_badge(TrayBadge::new(content, TrayBadgeTone::Accent));
        let s = tray_surface(&sig, &theme);
        assert!(region_has(
            &s,
            (SS / 2, SS),
            (0, SS / 2),
            premul(theme.palette().on_accent)
        ));
    }
}

#[test]
fn tray_signal_badge_count_caps_at_nine_plus() {
    let theme = Theme::dark();
    let nine = tray_surface(
        &TraySignal::new(IconKind::Network, "Network").with_badge(TrayBadge::new(
            TrayBadgeContent::Count(9),
            TrayBadgeTone::Accent,
        )),
        &theme,
    );
    let ten = tray_surface(
        &TraySignal::new(IconKind::Network, "Network").with_badge(TrayBadge::new(
            TrayBadgeContent::Count(10),
            TrayBadgeTone::Accent,
        )),
        &theme,
    );
    let large = tray_surface(
        &TraySignal::new(IconKind::Network, "Network").with_badge(TrayBadge::new(
            TrayBadgeContent::Count(999),
            TrayBadgeTone::Accent,
        )),
        &theme,
    );
    // A single digit and the wider "9+" overflow badge differ...
    assert_ne!(nine.pixels(), ten.pixels());
    // ...but every count once it overflows renders the same capped "9+".
    assert_eq!(ten.pixels(), large.pixels());
    assert_eq!(TrayBadgeContent::Count(9).text(), "9");
    assert_eq!(TrayBadgeContent::Count(10).text(), "9+");
    assert_eq!(TrayBadgeContent::Count(999).text(), "9+");
}

#[test]
fn tray_signal_badge_coexists_with_bead_stack() {
    // A capsule wide enough for the badge and the mini beads to both fit.
    const W: u32 = 96;
    let theme = Theme::dark();
    let sig = TraySignal::new(IconKind::Generic, "Multi")
        .with_badge(TrayBadge::new(
            TrayBadgeContent::Count(4),
            TrayBadgeTone::Accent,
        ))
        .with_state(
            ControlState::idle()
                .with_authority(AuthorityState::Denied)
                .with_validation(ValidationState::Warning),
        );
    let mut s = Surface::new(W, SS).expect("surface");
    sig.render(&mut s, Rect::new(0, 0, W, SS), Scale::ONE, &theme);
    // The badge and both severity beads are all visible; none hides another.
    assert!(has_pixel(&s, premul(theme.palette().accent)));
    assert!(has_pixel(&s, premul(theme.palette().denied)));
    assert!(has_pixel(&s, premul(theme.palette().warning)));
}

#[test]
fn tray_signal_without_badge_keeps_prior_rendering() {
    let theme = Theme::dark();
    let base = TraySignal::new(IconKind::Network, "Network");
    assert_eq!(base.badge(), None);
    let baseline = tray_surface(&base, &theme);
    // Setting then clearing a badge must leave rendering unchanged.
    let mut cleared = base.with_badge(TrayBadge::new(
        TrayBadgeContent::Alert,
        TrayBadgeTone::Danger,
    ));
    cleared.set_badge(None);
    assert_eq!(cleared.badge(), None);
    let after_clear = tray_surface(&cleared, &theme);
    assert_eq!(baseline.pixels(), after_clear.pixels());
}

#[test]
fn tray_signal_badge_on_degenerate_bounds_does_not_panic() {
    let theme = Theme::dark();
    let sig = TraySignal::new(IconKind::Network, "Network").with_badge(TrayBadge::new(
        TrayBadgeContent::Count(9),
        TrayBadgeTone::Accent,
    ));
    let mut s = Surface::new(1, 1).expect("surface");
    // A capsule too small to hold anything simply draws nothing, never panics.
    sig.render(&mut s, Rect::new(0, 0, 1, 1), Scale::ONE, &theme);
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
    let (w, h) = sig.readout_size(Scale::ONE, &theme);
    let mut s = Surface::new(w, h).expect("surface");
    sig.render_readout(&mut s, Rect::new(0, 0, w, h), Scale::ONE, &theme);
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
    let (w, h) = sig.readout_size(Scale::ONE, &theme);
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

// --- Render-equivalence equality (the host's repaint gate) ----------------

#[test]
fn hit_test_bookkeeping_is_invisible_to_a_taskbar_item() {
    let theme = Theme::dark();
    let bounds = Rect::new(0, 0, TW, TH);

    // Two samples clear of the item, so only the recorded coordinate differs.
    let mut a = TaskbarItem::new("Editor", IconKind::Generic);
    let mut b = a.clone();
    a.on_pointer(&moved(iv(TW) + 40, iv(TH) + 40), bounds);
    b.on_pointer(&moved(iv(TW) + 90, iv(TH) + 12), bounds);
    assert_eq!(
        a, b,
        "a coordinate clear of the item is not a drawn property"
    );
    assert_eq!(
        task_surface(&a, &theme, Scale::ONE).pixels(),
        task_surface(&b, &theme, Scale::ONE).pixels(),
        "…and the two must therefore paint identically"
    );

    // One holds a real press latch, the other is merely *shown* pressed.
    let mut latched = TaskbarItem::new("Editor", IconKind::Generic);
    latched.on_pointer(&moved(iv(TW) / 2, iv(TH) / 2), bounds);
    latched.on_pointer(&PRESS, bounds);
    let mut shown = TaskbarItem::new("Editor", IconKind::Generic);
    let mut pressed = ControlState::idle();
    pressed.pointer = PointerState::Pressed;
    shown.set_state(pressed);
    assert_eq!(latched, shown, "the press latch is not a drawn property");
    assert_eq!(
        task_surface(&latched, &theme, Scale::ONE).pixels(),
        task_surface(&shown, &theme, Scale::ONE).pixels(),
        "…and the two must therefore paint identically"
    );
    assert_eq!(
        latched.on_pointer(&RELEASE, bounds),
        Some(TaskbarItemAction::Activated),
        "the latch still governs activation, it is only invisible"
    );
}

#[test]
fn pointer_position_alone_never_changes_a_tray_signal_render() {
    let theme = Theme::dark();
    let capsule = Rect::new(0, 0, SS, SS);
    let readout = Rect::new(0, iv(SS), 120, 60);
    // Two samples clear of both the capsule and the readout, so only the
    // recorded coordinate differs; the expansion a hover *causes* is a
    // separate, still-compared property.
    let mut a = TraySignal::new(IconKind::Battery, "Battery").with_value("82%");
    let mut b = a.clone();
    let _ = a.on_pointer(&moved(500, 500), capsule, readout, Scale::ONE, &theme);
    let _ = b.on_pointer(&moved(640, 480), capsule, readout, Scale::ONE, &theme);

    assert_eq!(
        a, b,
        "a coordinate clear of the signal is not a drawn property"
    );
    assert_eq!(
        tray_surface(&a, &theme).pixels(),
        tray_surface(&b, &theme).pixels(),
        "…and the two must therefore paint identically"
    );
}
