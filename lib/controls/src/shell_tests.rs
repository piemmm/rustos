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
use tairix_icon::{IconKind, IconPicture};
use tairix_input::{InputEvent, Key, NamedKey, PointerButton};
use tairix_raster::{Color, Pixel, Surface};
use tairix_theme::{Rgba, Theme};

use crate::button::{Button, ButtonContent};
use crate::damage::sink;
use crate::shell::{
    Notification, NotificationAction, TaskbarItem, TaskbarItemAction, TrayBadge, TrayBadgeContent,
    TrayBadgeTone, TraySignal, TraySignalAction, WindowPreview, WindowPreviewAction,
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
        note.on_pointer(&moved(110, 112), bounds, Scale::ONE, &theme, &mut sink()),
        None
    );
    assert_eq!(
        note.on_pointer(&PRESS, bounds, Scale::ONE, &theme, &mut sink()),
        None
    );
    assert_eq!(
        note.on_pointer(&RELEASE, bounds, Scale::ONE, &theme, &mut sink()),
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
fn taskbar_item_rests_bare_on_the_bar() {
    for theme in [Theme::dark(), Theme::light()] {
        let item = TaskbarItem::new(IconKind::Generic);
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
        // And it marks nothing on its lower edge: every slot on the icon bar
        // is a running application, so a "running" seam would mark them all
        // alike and say nothing.
        assert!(
            !region_has(
                &s,
                (0, TW),
                (TH - 3, TH),
                premul(theme.palette().on_surface_muted)
            ),
            "{}: a slot draws no presence mark",
            theme.name()
        );
        assert!(
            !region_has(&s, (0, TW), (TH - 3, TH), premul(theme.palette().accent)),
            "{}: nor a focus seam",
            theme.name()
        );
    }
}

#[test]
fn taskbar_item_washes_its_plate_under_the_pointer_without_an_edge() {
    for theme in [Theme::dark(), Theme::light()] {
        let palette = theme.palette();
        let hovered = task_surface(
            &TaskbarItem::new(IconKind::Generic)
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
fn taskbar_item_attention_shows_bead() {
    let theme = Theme::dark();
    let item = TaskbarItem::new(IconKind::Bell).with_attention(true);
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
    let item = TaskbarItem::new(IconKind::Generic)
        .with_attention(true)
        .with_state(ControlState::idle().with_recovery(RecoveryState::Hung));
    let s = task_surface(&item, &theme, Scale::ONE);
    assert!(has_pixel(&s, premul(theme.palette().recovery)));
}

#[test]
fn taskbar_item_denied_shows_lock_and_does_not_activate() {
    let theme = Theme::dark();
    let mut item = TaskbarItem::new(IconKind::Generic)
        .with_state(ControlState::idle().with_authority(AuthorityState::Denied));
    let s = task_surface(&item, &theme, Scale::ONE);
    assert!(has_pixel(&s, premul(theme.palette().denied)));
    let bounds = Rect::new(0, 0, TW, TH);
    assert_eq!(item.on_pointer(&moved(80, 16), bounds, &mut sink()), None);
    assert_eq!(item.on_pointer(&PRESS, bounds, &mut sink()), None);
    // Fail closed: a denied item never activates.
    assert_eq!(item.on_pointer(&RELEASE, bounds, &mut sink()), None);
}

#[test]
fn taskbar_item_activates_by_pointer_and_keyboard() {
    let mut item = TaskbarItem::new(IconKind::Generic);
    let bounds = Rect::new(0, 0, TW, TH);
    assert_eq!(item.on_pointer(&moved(80, 16), bounds, &mut sink()), None);
    assert_eq!(item.on_pointer(&PRESS, bounds, &mut sink()), None);
    assert_eq!(
        item.on_pointer(&RELEASE, bounds, &mut sink()),
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
    let item = TaskbarItem::new(IconKind::Generic);
    let s = task_surface(&item, &hc, scale2());
    assert!(has_pixel(&s, premul(hc.palette().on_surface)));
}

/// A square application slot — the shape every slot on the bar wears.
const PS: u32 = 40;

#[test]
fn taskbar_item_centres_a_plate_sized_glyph() {
    let theme = Theme::dark();
    let bounds = Rect::new(0, 0, PS, PS);
    let wide = Rect::new(0, 0, PS * 2, PS * 2);
    let item = TaskbarItem::new(IconKind::AppBundle);
    // The glyph is sized off the plate, so it grows with the slot rather than
    // staying bound to a text line.
    assert!(item.icon_side(wide, Scale::ONE, &theme) > item.icon_side(bounds, Scale::ONE, &theme));
    let mut s = Surface::new(PS, PS).expect("surface");
    item.render(&mut s, bounds, Scale::ONE, &theme, None);
    // Glyph ink sits in the centre of the plate.
    assert!(region_has(
        &s,
        (PS / 3, PS - PS / 3),
        (PS / 3, PS - PS / 3),
        premul(theme.palette().on_surface)
    ));
}

#[test]
fn taskbar_item_draws_its_icon_and_nothing_beside_it() {
    let theme = Theme::dark();
    // A slot far wider than its icon: the identity is the icon alone, so the
    // space beside it carries no ink at all — a title drawn there would read
    // as a second, unequal kind of button on a strip of equal icons.
    let bounds = Rect::new(0, 0, TW, TH);
    let item = TaskbarItem::new(IconKind::AppBundle);
    let side = item.icon_side(bounds, Scale::ONE, &theme);
    assert!(side > 0);
    let mut s = Surface::new(TW, TH).expect("surface");
    item.render(&mut s, bounds, Scale::ONE, &theme, None);

    let icon_end = u32::midpoint(TW, side);
    assert!(
        region_has(
            &s,
            ((TW - side) / 2, icon_end),
            (0, TH),
            premul(theme.palette().on_surface)
        ),
        "the icon is drawn"
    );
    assert!(
        !region_has(
            &s,
            (icon_end + 1, TW),
            (0, TH),
            premul(theme.palette().on_surface)
        ),
        "and nothing is drawn beside it"
    );
}

#[test]
fn taskbar_item_icon_side_is_zero_for_degenerate_bounds() {
    let theme = Theme::dark();
    assert_eq!(
        TaskbarItem::new(IconKind::AppBundle).icon_side(
            Rect::new(-4, -4, 0, 0),
            Scale::ONE,
            &theme
        ),
        0
    );
}

#[test]
fn taskbar_item_artwork_replaces_the_builtin_glyph_at_any_slot_shape() {
    let theme = Theme::dark();
    let magenta = Color::rgb(255, 0, 255).premultiply();
    // A square slot and a wider one both paint the owner-supplied artwork in
    // place of the built-in class glyph.
    let cases = [
        (Rect::new(0, 0, PS, PS), PS, PS),
        (Rect::new(0, 0, TW, TH), TW, TH),
    ];
    for (bounds, w, h) in cases {
        let item = TaskbarItem::new(IconKind::AppBundle);
        let side = item.icon_side(bounds, Scale::ONE, &theme);
        assert!(side > 0);
        let art = Surface::filled(side, side, magenta).expect("artwork");
        let mut s = Surface::new(w, h).expect("surface");
        item.render(
            &mut s,
            bounds,
            Scale::ONE,
            &theme,
            Some(IconPicture::Artwork(&art)),
        );
        assert!(has_pixel(&s, magenta));
    }
}

#[test]
fn taskbar_item_keeps_status_furniture_in_a_square_slot() {
    let theme = Theme::dark();
    let bounds = Rect::new(0, 0, PS, PS);
    // A denied compact slot still shows the lock bead and refuses to act.
    let mut denied = TaskbarItem::new(IconKind::AppBundle)
        .with_state(ControlState::idle().with_authority(AuthorityState::Denied));
    let mut d = Surface::new(PS, PS).expect("surface");
    denied.render(&mut d, bounds, Scale::ONE, &theme, None);
    assert!(has_pixel(&d, premul(theme.palette().denied)));
    assert_eq!(denied.on_pointer(&moved(20, 20), bounds, &mut sink()), None);
    assert_eq!(denied.on_pointer(&PRESS, bounds, &mut sink()), None);
    assert_eq!(denied.on_pointer(&RELEASE, bounds, &mut sink()), None);
}

// --- WindowPreview (spec §11.26) ---------------------------------------

/// A picker cell: wide enough for a landscape thumbnail and a caption line.
const PW: u32 = 160;
const PH: u32 = 120;

#[test]
fn window_preview_draws_its_thumbnail_and_caption() {
    for theme in [Theme::dark(), Theme::light()] {
        let bounds = Rect::new(0, 0, PW, PH);
        let preview = WindowPreview::new("notes.txt", IconKind::AppBundle);
        let thumb = preview.thumbnail_bounds(bounds, Scale::ONE, &theme);
        assert!(!thumb.is_empty());
        // The caption line sits below the thumbnail, inside the plate.
        assert!(thumb.bottom() < i32::try_from(PH).expect("a modest cell"));

        let magenta = Color::rgb(255, 0, 255).premultiply();
        let image = Surface::filled(thumb.width, thumb.height, magenta).expect("thumbnail");
        let mut s = Surface::new(PW, PH).expect("surface");
        preview.render(&mut s, bounds, Scale::ONE, &theme, Some(&image), None);
        // The owner-scaled frame is blitted exactly where the query said.
        assert!(has_pixel(&s, magenta));
        // And the caption's ink is under it, not over it.
        assert!(
            region_has(
                &s,
                (0, PW),
                (u32::try_from(thumb.top()).expect("fits") + thumb.height, PH),
                premul(theme.palette().on_surface)
            ),
            "{}: the caption is drawn below the thumbnail",
            theme.name()
        );
    }
}

#[test]
fn window_preview_without_a_thumbnail_falls_back_to_its_glyph() {
    // A window that has not presented yet, or whose pixels were released
    // under memory pressure, still states something: the cell can never come
    // up blank.
    let theme = Theme::dark();
    let bounds = Rect::new(0, 0, PW, PH);
    let preview = WindowPreview::new("Terminal", IconKind::AppBundle);
    let mut s = Surface::new(PW, PH).expect("surface");
    preview.render(&mut s, bounds, Scale::ONE, &theme, None, None);
    assert!(has_pixel(&s, premul(theme.palette().on_surface)));
}

#[test]
fn window_preview_thumbnail_bounds_are_empty_for_a_degenerate_cell() {
    let theme = Theme::dark();
    let preview = WindowPreview::new("x", IconKind::AppBundle);
    assert!(preview
        .thumbnail_bounds(Rect::new(0, 0, 0, 0), Scale::ONE, &theme)
        .is_empty());
    // A cell with no room for a caption line above the plate's padding has
    // nowhere to put a thumbnail either, and says so rather than drawing
    // outside itself.
    assert!(preview
        .thumbnail_bounds(Rect::new(0, 0, PW, 4), Scale::ONE, &theme)
        .is_empty());
}

#[test]
fn window_preview_activates_by_pointer_and_keyboard() {
    let bounds = Rect::new(0, 0, PW, PH);
    let mut preview = WindowPreview::new("Terminal", IconKind::AppBundle);
    assert_eq!(
        preview.on_pointer(&moved(10, 10), bounds, &mut sink()),
        None
    );
    assert_eq!(preview.on_pointer(&PRESS, bounds, &mut sink()), None);
    assert_eq!(
        preview.on_pointer(&RELEASE, bounds, &mut sink()),
        Some(WindowPreviewAction::Activated)
    );
    // A release away from the cell is not an activation.
    assert_eq!(preview.on_pointer(&PRESS, bounds, &mut sink()), None);
    assert_eq!(
        preview.on_pointer(&moved(-5, -5), bounds, &mut sink()),
        None
    );
    assert_eq!(preview.on_pointer(&RELEASE, bounds, &mut sink()), None);

    assert_eq!(preview.on_key(Key::Char(' ')), None);
    preview.set_focused(true);
    assert_eq!(
        preview.on_key(Key::Char(' ')),
        Some(WindowPreviewAction::Activated)
    );
}

#[test]
fn window_preview_equality_is_a_repaint_gate() {
    // Equal previews draw the same pixels, so a picker may compare them to
    // decide whether to repaint; the pointer position is not part of that.
    let bounds = Rect::new(0, 0, PW, PH);
    let mut a = WindowPreview::new("Terminal", IconKind::AppBundle);
    let b = a.clone();
    assert_eq!(a, b);
    assert_eq!(a.on_pointer(&moved(400, 400), bounds, &mut sink()), None);
    assert_eq!(a, b, "a pointer sample outside the cell changes no pixel");
    assert_ne!(
        WindowPreview::new("Terminal", IconKind::AppBundle),
        WindowPreview::new("Editor", IconKind::AppBundle)
    );
}

// --- TraySignal (spec §11.27) ------------------------------------------

const SS: u32 = 32;

fn tray_surface(sig: &TraySignal, theme: &Theme) -> Surface {
    let mut s = Surface::new(SS, SS).expect("surface");
    sig.render(&mut s, Rect::new(0, 0, SS, SS), Scale::ONE, theme, None);
    s
}

/// The capsule painted with a flat-coloured square standing in for the
/// shipped artwork, rasterised at exactly the side the capsule reports.
fn tray_surface_with_artwork(sig: &TraySignal, theme: &Theme, colour: Color) -> Surface {
    let bounds = Rect::new(0, 0, SS, SS);
    let side = sig.icon_side(bounds, Scale::ONE, theme);
    let art = Surface::filled(side, side, colour.premultiply()).expect("artwork");
    let mut s = Surface::new(SS, SS).expect("surface");
    sig.render(
        &mut s,
        bounds,
        Scale::ONE,
        theme,
        Some(IconPicture::Artwork(&art)),
    );
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

/// The capsule is the desktop's rightmost icon and resolves through the same
/// tiers as every other one: the shipped artwork when the system has it, the
/// built-in glyph otherwise. Rasterising the glyph unconditionally left the
/// capsule unable to ever show a shipped icon.
#[test]
fn tray_signal_draws_shipped_artwork_in_place_of_its_glyph() {
    let theme = Theme::dark();
    let sig = TraySignal::new(IconKind::User, "Switchboard");
    let art = Color::rgb(255, 0, 255);
    let s = tray_surface_with_artwork(&sig, &theme, art);

    assert!(has_pixel(&s, art.premultiply()), "the artwork is drawn");
    assert!(
        !has_pixel(&s, premul(theme.palette().on_surface)),
        "and the built-in glyph is not drawn as well"
    );
}

/// The capsule seats an icon exactly as the application slots beside it do.
/// It was sized off the body font's glyph height instead, which on the icon
/// bar drew the Switchboard a third of its neighbours' size and adrift in
/// its own space.
#[test]
fn tray_signal_seats_its_icon_at_the_side_every_other_bar_icon_uses() {
    let theme = Theme::dark();
    let bounds = Rect::new(0, 0, SS, SS);
    let capsule =
        TraySignal::new(IconKind::User, "Switchboard").icon_side(bounds, Scale::ONE, &theme);
    let slot = TaskbarItem::new(IconKind::Generic).icon_side(bounds, Scale::ONE, &theme);
    assert_eq!(capsule, slot);
    assert!(
        capsule * 4 >= SS * 3,
        "an icon fills most of its slot: {capsule} of {SS}"
    );
}

/// An owner rasterises the artwork itself, so the side the capsule reports
/// must be the side it draws: an exactly-sized square lands whole, centred,
/// and inside the plate border.
#[test]
fn tray_signal_draws_artwork_at_exactly_the_side_it_reports() {
    let theme = Theme::dark();
    let sig = TraySignal::new(IconKind::User, "Switchboard");
    let side = sig.icon_side(Rect::new(0, 0, SS, SS), Scale::ONE, &theme);
    assert!(
        side > 0 && side < SS,
        "the icon is inset within the capsule"
    );

    let art = Color::rgb(255, 0, 255).premultiply();
    let s = tray_surface_with_artwork(&sig, &theme, Color::rgb(255, 0, 255));
    let drawn = s.pixels().iter().filter(|&&p| p == art).count();
    assert_eq!(
        drawn,
        usize::try_from(side * side).expect("fits in usize"),
        "the whole square is placed, neither clipped nor scaled"
    );

    let offset = (SS - side) / 2;
    assert_eq!(s.get(offset, offset), Some(art), "placed centred");
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
    sig.render(&mut s, Rect::new(0, 0, W, SS), Scale::ONE, &theme, None);
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

/// The drift guard behind `draws_same_capsule`: the readout's own fields
/// must not reach the capsule's pixels. A live value line moves on every
/// reading the owner publishes, and a taskbar that repainted its whole bar
/// for one is what this split exists to stop.
#[test]
fn tray_signal_capsule_ignores_the_readout_only_fields() {
    for theme in [Theme::dark(), Theme::light(), high_contrast()] {
        let base = TraySignal::new(IconKind::User, "System normal")
            .with_value("CPU 7%")
            .with_action(Button::labelled("Open Switchboard"));
        for other in [
            // A different value line — the every-sample case.
            TraySignal::new(IconKind::User, "System normal")
                .with_value("sysmon — 31% CPU")
                .with_action(Button::labelled("Open Switchboard")),
            // A different state name.
            TraySignal::new(IconKind::User, "Background work")
                .with_value("CPU 7%")
                .with_action(Button::labelled("Open Switchboard")),
            // No value and no action at all.
            TraySignal::new(IconKind::User, "System normal"),
        ] {
            assert!(base.draws_same_capsule(&other));
            assert_eq!(
                tray_surface(&base, &theme).pixels(),
                tray_surface(&other, &theme).pixels()
            );
        }
    }
}

/// The other direction, and the one whose failure would leave stale pixels
/// on screen: every field the capsule *does* draw moves both its rendering
/// and `draws_same_capsule`.
#[test]
fn tray_signal_capsule_tracks_every_field_it_draws() {
    let theme = Theme::dark();
    let base = TraySignal::new(IconKind::User, "System normal");
    for other in [
        TraySignal::new(IconKind::Battery, "System normal"),
        TraySignal::new(IconKind::User, "System normal")
            .with_state(ControlState::idle().with_activity(ActivityState::Working)),
        TraySignal::new(IconKind::User, "System normal").with_state(
            ControlState::idle().with_pressure(PressureState::Under(PressureKind::Memory)),
        ),
        TraySignal::new(IconKind::User, "System normal")
            .with_state(ControlState::idle().with_recovery(RecoveryState::Hung)),
        TraySignal::new(IconKind::User, "System normal").with_badge(TrayBadge::new(
            TrayBadgeContent::Alert,
            TrayBadgeTone::Danger,
        )),
    ] {
        assert!(!base.draws_same_capsule(&other));
        assert_ne!(
            tray_surface(&base, &theme).pixels(),
            tray_surface(&other, &theme).pixels()
        );
    }
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
    sig.render(&mut s, Rect::new(0, 0, 1, 1), Scale::ONE, &theme, None);
}

#[test]
fn tray_signal_expands_on_hover_and_focus() {
    let theme = Theme::dark();
    let mut sig = TraySignal::new(IconKind::Battery, "Battery").with_value("82%");
    assert!(!sig.is_expanded());
    let capsule = Rect::new(0, 0, SS, SS);
    let readout = Rect::new(0, iv(SS), 120, 60);
    // Hovering the capsule expands the readout.
    let _ = sig.on_pointer(
        &moved(10, 10),
        capsule,
        readout,
        Scale::ONE,
        &theme,
        &mut sink(),
    );
    assert!(sig.is_expanded());
    // Focus alone also expands.
    let _ = sig.on_pointer(
        &moved(500, 500),
        capsule,
        readout,
        Scale::ONE,
        &theme,
        &mut sink(),
    );
    assert!(!sig.is_expanded());
    sig.set_focused(true);
    assert!(sig.is_expanded());
}

/// A hover has to be able to end without the pointer moving: a window rising
/// over the bar takes the pointer away while leaving it at the capsule's own
/// coordinates, and re-testing those would answer "still hovered" — stranding
/// an expanded instrument readout over that window.
#[test]
fn tray_signal_pointer_left_collapses_a_readout_the_pointer_never_moved_off() {
    let theme = Theme::dark();
    let mut sig = TraySignal::new(IconKind::Battery, "Battery").with_value("82%");
    let capsule = Rect::new(0, 0, SS, SS);
    let readout = Rect::new(0, iv(SS), 120, 60);
    let _ = sig.on_pointer(
        &moved(10, 10),
        capsule,
        readout,
        Scale::ONE,
        &theme,
        &mut sink(),
    );
    assert!(sig.is_expanded());

    // The pointer is still at (10, 10) — inside the capsule — and the readout
    // collapses anyway, because it was told rather than asked.
    let mut damage = sink();
    assert!(sig.pointer_left(capsule, readout, &mut damage));
    assert!(!sig.is_expanded());
    // Both the capsule and the readout it was showing are repainted.
    assert!(damage.bounds().contains(Point::new(2, 2)));
    assert!(damage.bounds().contains(Point::new(2, iv(SS) + 2)));

    // Saying it twice changes nothing and repaints nothing.
    let mut again = sink();
    assert!(!sig.pointer_left(capsule, readout, &mut again));
    assert!(again.is_empty());
}

/// The keyboard can hold the readout open, and a pointer leaving is not the
/// keyboard letting go: the capsule stays expanded for whoever is driving it
/// with keys.
#[test]
fn tray_signal_pointer_left_leaves_a_keyboard_held_readout_open() {
    let theme = Theme::dark();
    let mut sig = TraySignal::new(IconKind::Battery, "Battery").with_value("82%");
    let capsule = Rect::new(0, 0, SS, SS);
    let readout = Rect::new(0, iv(SS), 120, 60);
    let _ = sig.on_pointer(
        &moved(10, 10),
        capsule,
        readout,
        Scale::ONE,
        &theme,
        &mut sink(),
    );
    sig.set_focused(true);

    assert!(sig.pointer_left(capsule, readout, &mut sink()));
    assert!(sig.is_expanded(), "focus still holds the readout open");
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

/// The capsule's readout is one of the desktop's floating surfaces, and the
/// trap is the same one the menu's test names: its rim is drawn across the
/// whole plate first, so a ground merely *composited* over it comes back
/// opaque and the popup frosts nothing on screen.
#[test]
fn a_floating_readout_lays_a_see_through_ground_over_its_rim() {
    for theme in [Theme::dark(), Theme::light()] {
        let sig = TraySignal::new(IconKind::Battery, "Battery").with_value("82%");
        let chrome_theme = theme.clone().floating();
        let (w, h) = sig.readout_size(Scale::ONE, &theme);
        let mut floating = Surface::new(w, h).expect("surface");
        sig.render_readout(
            &mut floating,
            Rect::new(0, 0, w, h),
            Scale::ONE,
            &chrome_theme,
        );
        // The padding row just inside the top border: the readout's own
        // ground, clear of the name and value lines that fill its middle.
        let interior = floating.get(w / 2, 1).expect("in bounds");
        assert_eq!(
            interior,
            premul(
                theme
                    .palette()
                    .surface_raised
                    .with_alpha(theme.palette().chrome_alpha)
            ),
            "{}: opaque ground",
            theme.name()
        );
        assert!(interior.a < 255, "{}: the chrome covers", theme.name());
        // The rim survives as the readout's edge, at the surface's own weight:
        // part of the glass, not a hard line drawn on it.
        let rim = premul(theme.palette().rim.with_alpha(theme.palette().chrome_alpha));
        assert_eq!(
            floating.get(0, h / 2),
            Some(rim),
            "{}: the laid ground ate the rim",
            theme.name()
        );
        assert_ne!(rim, interior, "{}: the edge must read", theme.name());

        let mut opaque = Surface::new(w, h).expect("surface");
        sig.render_readout(&mut opaque, Rect::new(0, 0, w, h), Scale::ONE, &theme);
        assert_eq!(
            opaque.get(w / 2, 1),
            Some(premul(theme.palette().surface_raised)),
            "{}: an ordinary readout changed",
            theme.name()
        );
    }
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
        sig.on_pointer(
            &moved(20, iv(by)),
            capsule,
            readout,
            Scale::ONE,
            &theme,
            &mut sink()
        ),
        None
    );
    assert_eq!(
        sig.on_pointer(&PRESS, capsule, readout, Scale::ONE, &theme, &mut sink()),
        None
    );
    assert_eq!(
        sig.on_pointer(&RELEASE, capsule, readout, Scale::ONE, &theme, &mut sink()),
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
    let mut a = TaskbarItem::new(IconKind::Generic);
    let mut b = a.clone();
    a.on_pointer(&moved(iv(TW) + 40, iv(TH) + 40), bounds, &mut sink());
    b.on_pointer(&moved(iv(TW) + 90, iv(TH) + 12), bounds, &mut sink());
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
    let mut latched = TaskbarItem::new(IconKind::Generic);
    latched.on_pointer(&moved(iv(TW) / 2, iv(TH) / 2), bounds, &mut sink());
    latched.on_pointer(&PRESS, bounds, &mut sink());
    let mut shown = TaskbarItem::new(IconKind::Generic);
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
        latched.on_pointer(&RELEASE, bounds, &mut sink()),
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
    let _ = a.on_pointer(
        &moved(500, 500),
        capsule,
        readout,
        Scale::ONE,
        &theme,
        &mut sink(),
    );
    let _ = b.on_pointer(
        &moved(640, 480),
        capsule,
        readout,
        Scale::ONE,
        &theme,
        &mut sink(),
    );

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
