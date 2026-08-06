//! Unit tests for the Switchboard window's shared per-section skeleton
//! (spec §17, §20).
//!
//! These prove the composition is assembled from the shared controls and
//! behaves correctly: the window manager decorates server-side so the app's
//! own content fills the client from the top edge, the location band's trail
//! and section list both switch sections (by pointer and keyboard) and mark
//! the one on show, a host can
//! open the panel on any section and lands in exactly the state the keyboard
//! would have reached, a refreshed model re-derives the controls while leaving
//! the user's section, scroll offset, and focus alone and never lets a stale
//! gesture reach a replaced row, the mouse wheel and keyboard scroll the
//! active section, denied actions render distinctly from disabled ones, and
//! the layout scales.

use tairix_geometry::{to_i32, Point, Rect, Scale};
use tairix_input::{InputEvent, Key, NamedKey};
use tairix_raster::Surface;
use tairix_theme::Theme;

use tairix_controls::testkit::high_contrast;
use tairix_controls::{
    ActivityState, ControlDisposition, Crumb, PressureState, RecoveryState, SelectionState,
};

use crate::panel::{MIN_WIN_HEIGHT, MIN_WIN_WIDTH};

use super::test_support::{
    bounds, centre, click, focus_task_row, font, has_ink, model, moved, select_task_row, task_id,
    task_rail_rects, task_row_point, PRESS, RELEASE,
};
use super::{
    resolve_section_frame, ActionVerdict, Reading, RecoveryControl, Section, Switchboard,
    SwitchboardAction, SwitchboardModel, SystemFact, SystemReport, TaskAuthority, TaskControl,
    TaskSummary,
};

#[test]
fn new_starts_on_tasks_at_offset_zero() {
    let sb = Switchboard::new(&model());
    assert_eq!(sb.section(), Section::Tasks);
    assert_eq!(sb.scroll_offset(), 0);
}

#[test]
fn render_paints_content() {
    let theme = Theme::dark();
    let mut sb = Switchboard::new(&model());
    let mut surface = Surface::new(600, 400).expect("surface");
    sb.render(&mut surface, bounds(), Scale::ONE, &theme, font());
    assert!(surface.pixels().iter().any(|p| p.a > 0));
}

#[test]
fn scroll_track_sits_beside_the_content_inside_bounds() {
    let theme = Theme::dark();
    let sb = Switchboard::new(&model());
    let b = bounds();
    let layout = sb.compute_layout(b, Scale::ONE, &theme);
    // The content area stops where the scrollbar gutter begins, and the two
    // together stay inside the bounds the compositor carved out.
    assert_eq!(layout.content.right(), layout.scroll.left());
    assert!(layout.scroll.right() <= b.right());
    assert!(layout.content.bottom() <= b.bottom());
    assert!(layout.scroll.bottom() <= b.bottom());
}

#[test]
fn the_client_content_begins_at_the_top_of_bounds() {
    let theme = Theme::dark();
    let sb = Switchboard::new(&model());
    let b = bounds();
    let layout = sb.compute_layout(b, Scale::ONE, &theme);
    // The window manager decorates server-side, so the app draws no title bar
    // of its own: its first region, the location band, sits at the very top
    // edge of the bounds it was handed. A re-introduced private title bar
    // would inset the client and push the band down, failing this.
    assert_eq!(layout.location.top(), b.top());
    // And the band really is placed there: its trail, the band's one keyboard
    // stop, resolves to the first rows of the client.
    let trail = sb.band(layout.location, &theme, Scale::ONE).trail;
    assert_eq!(trail.top(), b.top());
}

/// Open the section list the way a reader does — a click on the location
/// band's trailing command — and hand back whatever actions that produced.
fn open_section_list(sb: &mut Switchboard, theme: &Theme) -> alloc::vec::Vec<SwitchboardAction> {
    let b = bounds();
    let layout = sb.compute_layout(b, Scale::ONE, theme);
    let command = sb.band(layout.location, theme, Scale::ONE).command;
    let (x, y) = centre(command);
    click(sb, b, Scale::ONE, theme, x, y)
}

/// Open the section list from the other route: a click on the trail's leading
/// crumb, the ancestor a breadcrumb activates.
fn open_section_list_from_trail(
    sb: &mut Switchboard,
    theme: &Theme,
) -> alloc::vec::Vec<SwitchboardAction> {
    let b = bounds();
    let layout = sb.compute_layout(b, Scale::ONE, theme);
    let trail = sb.band(layout.location, theme, Scale::ONE).trail;
    let x = trail.left() + 1;
    let y = centre(trail).1;
    // Aim through the trail's own hit test, so the click is proven to land on
    // the leading crumb rather than on a guessed coordinate.
    assert_eq!(
        sb.trail
            .crumb_at(trail, Scale::ONE, theme, font(), Point::new(x, y)),
        Some(0),
        "the leading crumb draws at the trail's leading edge"
    );
    click(sb, b, Scale::ONE, theme, x, y)
}

/// The centre of the open section list's row for `section`, read from the
/// menu's own row geometry rather than a hand-copied position.
fn section_row_centre(sb: &Switchboard, theme: &Theme, section: Section) -> (i32, i32) {
    let b = bounds();
    let layout = sb.compute_layout(b, Scale::ONE, theme);
    let menu = sb
        .section_menu
        .as_ref()
        .expect("the section list must be open");
    let rect = Switchboard::popup_rect(menu, layout.location, b, Scale::ONE, theme, font());
    let row = menu
        .row_rect(section.index(), rect, Scale::ONE, theme)
        .expect("the row must be drawn");
    centre(row)
}

#[test]
fn section_list_click_switches_section() {
    let theme = Theme::dark();
    let mut sb = Switchboard::new(&model());
    let b = bounds();
    // Pressing the band's section-list command opens the list without
    // switching anything by itself.
    assert_eq!(open_section_list(&mut sb, &theme), alloc::vec::Vec::new());
    assert_eq!(sb.section(), Section::Tasks);
    let (x, y) = section_row_centre(&sb, &theme, Section::Jobs);
    let actions = click(&mut sb, b, Scale::ONE, &theme, x, y);
    assert!(actions.contains(&SwitchboardAction::SectionChanged {
        section: Section::Jobs
    }));
    assert_eq!(sb.section(), Section::Jobs);
    assert!(
        sb.section_menu.is_none(),
        "a choice closes the list it was made in"
    );
}

#[test]
fn the_trails_leading_crumb_opens_the_same_section_list() {
    let theme = Theme::dark();
    let mut sb = Switchboard::new(&model());
    let b = bounds();
    assert_eq!(
        open_section_list_from_trail(&mut sb, &theme),
        alloc::vec::Vec::new()
    );
    assert!(
        sb.section_menu.is_some(),
        "the leading crumb opens the list"
    );
    let (x, y) = section_row_centre(&sb, &theme, Section::Recovery);
    let actions = click(&mut sb, b, Scale::ONE, &theme, x, y);
    assert!(actions.contains(&SwitchboardAction::SectionChanged {
        section: Section::Recovery
    }));
    assert_eq!(sb.section(), Section::Recovery);
}

#[test]
fn the_location_band_paints_the_trail_and_its_command() {
    let theme = Theme::dark();
    let mut sb = Switchboard::new(&model());
    let b = bounds();
    let mut surface = Surface::new(b.width, b.height).expect("surface");
    sb.render(&mut surface, b, Scale::ONE, &theme, font());
    let layout = sb.compute_layout(b, Scale::ONE, &theme);
    let band = sb.band(layout.location, &theme, Scale::ONE);
    let (trail, command) = (band.trail, band.command);
    assert!(has_ink(&surface, trail), "the trail names the location");
    assert!(
        has_ink(&surface, command),
        "the section-list command is drawn"
    );
    assert!(
        trail.right() < command.left(),
        "the trail and the command never share a pixel"
    );
    assert_eq!(command.right(), layout.location.right());
}

#[test]
fn the_trail_names_the_section_on_show() {
    let mut sb = Switchboard::new(&model());
    for section in Section::ALL {
        sb.select_section(section);
        let labels: alloc::vec::Vec<&str> = sb.trail.crumbs().iter().map(Crumb::label).collect();
        assert_eq!(labels, alloc::vec!["Switchboard", section.title()]);
    }
}

#[test]
fn the_section_list_marks_the_section_on_show() {
    let theme = Theme::dark();
    for section in Section::ALL {
        let mut sb = Switchboard::new(&model());
        sb.select_section(section);
        open_section_list(&mut sb, &theme);
        let menu = sb.section_menu.as_ref().expect("open");
        assert_eq!(menu.current(), Some(section.index()));
        for (i, item) in menu.items().iter().enumerate() {
            let expected = if i == section.index() {
                SelectionState::Selected
            } else {
                SelectionState::Unselected
            };
            assert_eq!(item.state().selection, expected, "row {i}");
            assert_eq!(item.label(), Section::ALL[i].title());
        }
    }
}

#[test]
fn a_press_off_the_section_list_closes_it_and_changes_nothing() {
    let theme = Theme::dark();
    let mut sb = Switchboard::new(&model());
    let b = bounds();
    open_section_list(&mut sb, &theme);
    let (x, y) = off_menu_point(&sb, &theme);
    let actions = click(&mut sb, b, Scale::ONE, &theme, x, y);
    assert_eq!(actions, alloc::vec::Vec::new());
    assert!(sb.section_menu.is_none());
    assert_eq!(sb.section(), Section::Tasks);
}

#[test]
fn a_refresh_leaves_an_open_section_list_alone() {
    let theme = Theme::dark();
    let mut sb = Switchboard::new(&model());
    let b = bounds();
    open_section_list(&mut sb, &theme);
    let open = sb.section_menu.clone();

    sb.set_model(&model());

    assert_eq!(
        sb.section_menu, open,
        "the list's rows are the closed section set, so no sample can stale it"
    );
    let (x, y) = section_row_centre(&sb, &theme, Section::Pressure);
    let actions = click(&mut sb, b, Scale::ONE, &theme, x, y);
    assert!(actions.contains(&SwitchboardAction::SectionChanged {
        section: Section::Pressure
    }));
}

#[test]
fn choosing_the_section_already_shown_closes_the_list_without_a_change() {
    let theme = Theme::dark();
    let mut sb = Switchboard::new(&model());
    let b = bounds();
    open_section_list(&mut sb, &theme);
    let (x, y) = section_row_centre(&sb, &theme, Section::Tasks);
    let actions = click(&mut sb, b, Scale::ONE, &theme, x, y);
    assert_eq!(actions, alloc::vec::Vec::new());
    assert!(sb.section_menu.is_none());
    assert_eq!(sb.section(), Section::Tasks);
}

#[test]
fn wheel_scrolls_the_active_section() {
    let theme = Theme::dark();
    let mut sb = Switchboard::new(&model());
    let b = bounds();
    let action = sb.on_pointer(
        &InputEvent::PointerScrolled { dx: 0, dy: 3 },
        b,
        Scale::ONE,
        &theme,
        font(),
    );
    match action {
        Some(SwitchboardAction::Scrolled { offset }) => assert_eq!(offset, 3),
        other => panic!("expected a scroll, got {other:?}"),
    }
    assert_eq!(sb.scroll_offset(), 3);
}

#[test]
fn keyboard_scrolls_the_focused_scrollbar() {
    let theme = Theme::dark();
    let mut sb = Switchboard::new(&model());
    let mut surface = Surface::new(600, 400).expect("surface");
    // Render once so the scroll model matches the layout.
    sb.render(&mut surface, bounds(), Scale::ONE, &theme, font());
    // Cycle focus Content -> Scrollbar (one Tab).
    assert_eq!(sb.on_key(Key::Named(NamedKey::Tab)), None);
    let action = sb.on_key(Key::Named(NamedKey::Down));
    match action {
        Some(SwitchboardAction::Scrolled { offset }) => assert!(offset >= 1),
        other => panic!("expected a keyboard scroll, got {other:?}"),
    }
}

#[test]
fn keyboard_cycles_focus_and_selects_a_section() {
    let mut sb = Switchboard::new(&model());
    // Content -> Scrollbar -> Location.
    for _ in 0..2 {
        assert_eq!(sb.on_key(Key::Named(NamedKey::Tab)), None);
    }
    // The band's leading crumb opens the section list, which then walks to
    // Jobs and shows it.
    assert_eq!(sb.on_key(Key::Named(NamedKey::Enter)), None);
    assert!(sb.section_menu.is_some(), "the list is open");
    assert_eq!(sb.on_key(Key::Named(NamedKey::Down)), None);
    let action = sb.on_key(Key::Named(NamedKey::Enter));
    assert_eq!(
        action,
        Some(SwitchboardAction::SectionChanged {
            section: Section::Jobs
        })
    );
    assert!(sb.section_menu.is_none(), "a choice closes the list");
}

#[test]
fn escape_closes_the_section_list_and_leaves_the_section_alone() {
    let mut sb = Switchboard::new(&model());
    for _ in 0..2 {
        assert_eq!(sb.on_key(Key::Named(NamedKey::Tab)), None);
    }
    assert_eq!(sb.on_key(Key::Named(NamedKey::Enter)), None);
    assert_eq!(sb.on_key(Key::Named(NamedKey::Down)), None);
    assert_eq!(sb.on_key(Key::Named(NamedKey::Escape)), None);
    assert!(sb.section_menu.is_none());
    assert_eq!(sb.section(), Section::Tasks);
}

#[test]
fn denied_action_renders_distinct_from_disabled() {
    let theme = Theme::dark();
    let mut m = SwitchboardModel::new("Switchboard");
    m.tasks.push(TaskSummary {
        proc_id: task_id(0),
        name: alloc::string::String::from("locked task"),
        pressure: PressureState::None,
        activity: ActivityState::Idle,
        recovery: RecoveryState::None,
        // Refused for want of authority, and one command the task's own
        // state rules out — so the two treatments can be told apart.
        authority: TaskAuthority {
            resume: ActionVerdict::DisabledByState,
            ..TaskAuthority::default()
        },
        group: None,
        ..TaskSummary::default()
    });
    let mut sb = Switchboard::new(&m);
    select_task_row(&mut sb, bounds(), Scale::ONE, &theme, 0);

    assert_eq!(
        sb.tasks.rail.items()[7].state().disposition(),
        ControlDisposition::DeniedByAuthority,
        "a command the caller may not use wears the Authority Mark"
    );
    assert_eq!(
        sb.tasks.rail.items()[3].state().disposition(),
        ControlDisposition::DisabledByState,
        "one the task's state rules out is plainly disabled instead"
    );
}

#[test]
fn offsets_are_independent_per_section() {
    let theme = Theme::dark();
    let mut sb = Switchboard::new(&model());
    let b = bounds();
    // Scroll Tasks down.
    sb.on_pointer(
        &InputEvent::PointerScrolled { dx: 0, dy: 4 },
        b,
        Scale::ONE,
        &theme,
        font(),
    );
    assert_eq!(sb.scroll_offset(), 4);
    // Switching to Jobs shows its own (zero) offset.
    sb.select_section(Section::Jobs);
    assert_eq!(sb.scroll_offset(), 0);
    // Switching back restores the Tasks offset.
    sb.select_section(Section::Tasks);
    assert_eq!(sb.scroll_offset(), 4);
}

#[test]
fn section_switch_reclamps_offset_to_new_content() {
    let theme = Theme::dark();
    let mut m = model();
    // A System screen with no readings has almost nothing to scroll.
    m.system = SystemReport::default();
    let mut sb = Switchboard::new(&m);
    let b = bounds();
    sb.on_pointer(
        &InputEvent::PointerScrolled { dx: 0, dy: 6 },
        b,
        Scale::ONE,
        &theme,
        font(),
    );
    sb.select_section(Section::System);
    // Sync against the (empty) System page clamps the offset to zero.
    let mut surface = Surface::new(600, 400).expect("surface");
    sb.render(&mut surface, b, Scale::ONE, &theme, font());
    assert_eq!(sb.scroll_offset(), 0);
}

#[test]
fn layout_scales_with_the_ui_scale() {
    let theme = Theme::dark();
    let sb = Switchboard::new(&model());
    let one = sb.compute_layout(bounds(), Scale::ONE, &theme);
    let two = sb.compute_layout(bounds(), Scale::from_percent(200).expect("scale"), &theme);
    assert!(two.location.height > one.location.height);
}

#[test]
fn light_theme_renders() {
    let theme = Theme::light();
    let mut sb = Switchboard::new(&model());
    let mut surface = Surface::new(600, 400).expect("surface");
    sb.render(&mut surface, bounds(), Scale::ONE, &theme, font());
    assert!(surface.pixels().iter().any(|p| p.a > 0));
}

#[test]
fn high_contrast_theme_renders() {
    let theme = high_contrast();
    let mut sb = Switchboard::new(&model());
    let mut surface = Surface::new(600, 400).expect("surface");
    sb.render(&mut surface, bounds(), Scale::ONE, &theme, font());
    assert!(surface.pixels().iter().any(|p| p.a > 0));
}

#[test]
fn press_on_the_location_bands_first_row_reaches_its_command() {
    let theme = Theme::dark();
    let mut sb = Switchboard::new(&model());
    let b = bounds();
    let layout = sb.compute_layout(b, Scale::ONE, &theme);
    let command = sb.band(layout.location, &theme, Scale::ONE).command;
    // The band's very first row of pixels, at the very top of the client.
    let x = centre(command).0;
    let y = layout.location.top();
    let actions = click(&mut sb, b, Scale::ONE, &theme, x, y);
    assert_eq!(actions, alloc::vec::Vec::new());
    assert!(
        sb.section_menu.is_some(),
        "the press reached the band's section-list command"
    );
}

#[test]
fn window_too_short_for_the_anatomy_still_renders_in_bounds() {
    let theme = Theme::dark();
    let mut sb = Switchboard::new(&model());
    // Shorter than the location band would ordinarily need.
    let b = Rect::new(0, 0, 600, 24);
    let mut surface = Surface::new(b.width, b.height).expect("surface");
    // Must not panic: every region clips to the bounds instead.
    sb.render(&mut surface, b, Scale::ONE, &theme, font());
    let layout = sb.compute_layout(b, Scale::ONE, &theme);
    assert!(layout.location.bottom() <= b.bottom());
    assert!(layout.content.bottom() <= b.bottom());
    assert!(layout.scroll.bottom() <= b.bottom());
}

#[test]
fn the_minimum_window_size_seats_every_declared_anatomy() {
    let theme = Theme::dark();
    let mut sb = Switchboard::new(&model());
    // The panel clamps a resize up so a section is never drawn into a box that
    // starves what it must keep. What it must keep is its primary column's
    // declared floor: the optional columns beside it are shed in the frame's
    // drop order, which is the designed outcome on a narrow window rather than
    // a lost region — but the sidebar and the action rail are last in that
    // order and must still be there. A section seated in this content area is
    // seated in the real client, and a section that later declares a wider
    // row-command strip, sidebar or rail than this floor can hold fails here
    // instead of pushing its own commands off the row on a small window.
    let b = Rect::new(0, 0, MIN_WIN_WIDTH, MIN_WIN_HEIGHT);
    let layout = sb.compute_layout(b, Scale::ONE, &theme);
    for section in Section::ALL {
        sb.select_section(section);
        let anatomy = sb.active().anatomy();
        let frame = resolve_section_frame(layout.content, anatomy, Scale::ONE, &theme);
        assert!(
            frame.primary.width >= anatomy.primary_floor(Scale::ONE, &theme),
            "{}'s primary column falls below its declared floor",
            section.title()
        );
        assert!(
            anatomy.sidebar_width == 0 || frame.sidebar.is_some(),
            "{} loses its sidebar at the minimum window",
            section.title()
        );
        assert!(
            anatomy.rail_width == 0 || frame.rail.is_some(),
            "{} loses its action rail at the minimum window",
            section.title()
        );
        assert!(
            layout.content.height >= anatomy.minimum_height(Scale::ONE),
            "{} asks for more height than the minimum window seats",
            section.title()
        );
    }
}

#[test]
fn select_section_shows_that_section_and_names_it_in_the_trail() {
    let theme = Theme::dark();
    let b = bounds();
    let mut painted = alloc::vec::Vec::new();
    for section in Section::ALL {
        let mut sb = Switchboard::new(&model());
        let changed = sb.select_section(section);
        if section == Section::Tasks {
            assert_eq!(changed, None, "Tasks is what a fresh Switchboard shows");
        } else {
            assert_eq!(changed, Some(SwitchboardAction::SectionChanged { section }));
        }
        assert_eq!(sb.section(), section);
        assert_eq!(
            sb.trail.crumbs().last().map(Crumb::label),
            Some(section.title())
        );
        let mut surface = Surface::new(b.width, b.height).expect("surface");
        sb.render(&mut surface, b, Scale::ONE, &theme, font());
        painted.push((section, surface.pixels().to_vec()));
    }
    for (i, (section, pixels)) in painted.iter().enumerate() {
        for (other_section, other_pixels) in painted.iter().skip(i + 1) {
            assert_ne!(
                pixels, other_pixels,
                "{section:?} and {other_section:?} drew the same surface"
            );
        }
    }
}

#[test]
fn select_section_reranges_the_scroll_for_the_new_section() {
    let theme = Theme::dark();
    let mut sb = Switchboard::new(&model());
    let b = bounds();
    let mut surface = Surface::new(b.width, b.height).expect("surface");
    // Scroll deep into the long (50-item) Tasks list.
    sb.on_pointer(
        &InputEvent::PointerScrolled { dx: 0, dy: 20 },
        b,
        Scale::ONE,
        &theme,
        font(),
    );
    let deep = sb.scroll_offset();
    assert!(deep > 0, "the long list must actually scroll");

    // The short (6-item) Recovery list opens at its own offset, never the
    // long list's, and the scrollbar is re-ranged to its content.
    assert_eq!(
        sb.select_section(Section::Recovery),
        Some(SwitchboardAction::SectionChanged {
            section: Section::Recovery
        })
    );
    sb.render(&mut surface, b, Scale::ONE, &theme, font());
    let range = sb.scroll.model().range();
    assert_eq!(range.content_extent(), 6);
    assert_eq!(sb.scroll_offset(), 0);
    assert!(range.offset() <= range.max_offset());

    // Going back restores the long list's own, still-valid offset.
    assert_eq!(
        sb.select_section(Section::Tasks),
        Some(SwitchboardAction::SectionChanged {
            section: Section::Tasks
        })
    );
    sb.render(&mut surface, b, Scale::ONE, &theme, font());
    let range = sb.scroll.model().range();
    assert_eq!(range.content_extent(), 50);
    assert_eq!(sb.scroll_offset(), deep);
    assert!(range.offset() <= range.max_offset());
}

#[test]
fn selecting_the_shown_section_changes_nothing() {
    let theme = Theme::dark();
    let mut sb = Switchboard::new(&model());
    let b = bounds();
    sb.on_pointer(
        &InputEvent::PointerScrolled { dx: 0, dy: 4 },
        b,
        Scale::ONE,
        &theme,
        font(),
    );
    // Move the keyboard off the first item too, so a stray reset would show.
    assert_eq!(sb.on_key(Key::Named(NamedKey::Down)), None);
    let before = sb.clone();
    let offset = sb.scroll_offset();

    assert_eq!(sb.select_section(Section::Tasks), None);

    assert_eq!(
        sb, before,
        "re-selecting the shown section must change nothing"
    );
    assert_eq!(sb.scroll_offset(), offset);
}

#[test]
fn pointer_after_selection_reaches_the_new_sections_content() {
    let theme = Theme::dark();
    let mut sb = Switchboard::new(&model());
    let b = bounds();
    sb.select_section(Section::Recovery);
    let layout = sb.compute_layout(b, Scale::ONE, &theme);
    // Recovery's commands moved from the row into an anchored rail, so the
    // aim moved with them: the rail sits in a column the Tasks section does
    // not seat at all, so only the new section can answer.
    let frame = resolve_section_frame(layout.content, sb.active().anatomy(), Scale::ONE, &theme);
    let rail = sb
        .recovery
        .rail_content(&frame, Scale::ONE, &theme)
        .expect("the default window seats the recovery rail");
    let (x, y) = centre(
        sb.recovery
            .rail
            .item_rect(rail, 0, Scale::ONE, &theme, font())
            .expect("the rail seats its restart command"),
    );
    let actions = click(&mut sb, b, Scale::ONE, &theme, x, y);
    assert!(actions.contains(&SwitchboardAction::Recovery {
        index: 0,
        control: RecoveryControl::Restart
    }));
    assert!(
        !actions
            .iter()
            .any(|a| matches!(a, SwitchboardAction::Task { .. })),
        "the superseded section must not still be receiving input"
    );
}

#[test]
fn direct_selection_and_the_keyboard_path_agree() {
    let mut by_key = Switchboard::new(&model());
    let mut direct = Switchboard::new(&model());
    // Put both on the location band (Content -> Scrollbar -> Location) so the
    // only difference is how the section is chosen.
    for _ in 0..2 {
        assert_eq!(by_key.on_key(Key::Named(NamedKey::Tab)), None);
        assert_eq!(direct.on_key(Key::Named(NamedKey::Tab)), None);
    }
    // One opens the section list, walks it to Recovery and commits it...
    assert_eq!(by_key.on_key(Key::Named(NamedKey::Enter)), None);
    for _ in 0..Section::Recovery.index() {
        assert_eq!(by_key.on_key(Key::Named(NamedKey::Down)), None);
    }
    let by_key_action = by_key.on_key(Key::Named(NamedKey::Enter));
    // ...the other asks for it directly.
    let direct_action = direct.select_section(Section::Recovery);

    assert_eq!(
        by_key_action,
        Some(SwitchboardAction::SectionChanged {
            section: Section::Recovery
        })
    );
    assert_eq!(by_key_action, direct_action);
    assert_eq!(by_key, direct, "one transition must leave one state");
}

/// A model of `tasks` tasks and `facts` machine facts and nothing else, for a
/// refresh that shortens, empties, or re-populates a section.
///
/// The first task's action is refused while the rest are permitted. The base
/// [`model`] permits every one of its tasks, so a refused first row is how a
/// test tells a refreshed row apart from the one it replaced: only the new row
/// can answer a click with silence.
fn refreshed_model(tasks: usize, facts: usize) -> SwitchboardModel {
    let mut m = SwitchboardModel::new("Switchboard");
    for i in 0..tasks {
        m.tasks.push(TaskSummary {
            proc_id: task_id(100 + i),
            name: alloc::format!("fresh {i}"),
            memory_bytes: Some(u64::try_from(i).unwrap_or(0) * 1024 * 1024),
            pressure: PressureState::None,
            activity: ActivityState::Idle,
            recovery: RecoveryState::None,
            // The first task refuses every command; the rest permit them, so
            // a test can tell a refused row from a permitted one.
            authority: if i > 0 {
                TaskAuthority {
                    switch: ActionVerdict::Ready,
                    pause: ActionVerdict::Ready,
                    resume: ActionVerdict::Ready,
                    lower_priority: ActionVerdict::Ready,
                    force_quit: ActionVerdict::Ready,
                }
            } else {
                TaskAuthority::default()
            },
            group: None,
            ..TaskSummary::default()
        });
    }
    for i in 0..facts {
        m.system.machine.push(SystemFact::new(
            alloc::format!("R{i}"),
            Reading::measured("10%"),
        ));
    }
    m
}

#[test]
fn set_model_keeps_the_section_the_offset_and_the_focus() {
    let theme = Theme::dark();
    let b = bounds();
    let mut sb = Switchboard::new(&model());
    let mut surface = Surface::new(b.width, b.height).expect("surface");
    sb.select_section(Section::Jobs);
    sb.render(&mut surface, b, Scale::ONE, &theme, font());

    // Walk the keyboard down the job list, scroll it away from the top, then
    // step the focus region on, so all three are off their defaults.
    for _ in 0..3 {
        assert_eq!(sb.on_key(Key::Named(NamedKey::Down)), None);
    }
    sb.on_pointer(
        &InputEvent::PointerScrolled { dx: 0, dy: 3 },
        b,
        Scale::ONE,
        &theme,
        font(),
    );
    let offset = sb.scroll_offset();
    assert!(offset > 0, "the job list must actually scroll");
    assert_eq!(sb.on_key(Key::Named(NamedKey::Tab)), None);
    let focus = sb.focus;
    let content_focus = sb.active().content_focus();

    sb.set_model(&model());

    assert_eq!(sb.section(), Section::Jobs);
    assert_eq!(
        sb.trail.crumbs().last().map(Crumb::label),
        Some(Section::Jobs.title())
    );
    assert_eq!(
        sb.scroll_offset(),
        offset,
        "a sample must not scroll the user back to the top"
    );
    assert_eq!(sb.focus, focus);
    assert_eq!(sb.active().content_focus(), content_focus);
}

#[test]
fn set_model_clamps_an_offset_past_the_end_of_a_shorter_list() {
    let theme = Theme::dark();
    let b = bounds();
    let mut sb = Switchboard::new(&model());
    let mut surface = Surface::new(b.width, b.height).expect("surface");
    sb.render(&mut surface, b, Scale::ONE, &theme, font());
    sb.on_pointer(
        &InputEvent::PointerScrolled { dx: 0, dy: 40 },
        b,
        Scale::ONE,
        &theme,
        font(),
    );
    assert!(
        sb.scroll_offset() > 5,
        "the 50-item list must scroll well past a 5-item one"
    );

    // Five tasks have nowhere near that far to scroll: the refresh re-ranges
    // there and then, rather than leaving a dangling offset for the next frame.
    sb.set_model(&refreshed_model(5, 3));

    let range = sb.scroll.model().range();
    assert_eq!(range.content_extent(), 5);
    assert!(range.offset() <= range.max_offset());
    assert!(
        sb.scroll_offset() < 5,
        "the offset must land inside the new list"
    );

    sb.render(&mut surface, b, Scale::ONE, &theme, font());
    let range = sb.scroll.model().range();
    assert_eq!(range.content_extent(), 5);
    assert!(range.offset() <= range.max_offset());
}

#[test]
fn set_model_to_an_empty_model_stays_valid_and_renderable() {
    let theme = Theme::dark();
    let b = bounds();
    let mut sb = Switchboard::new(&model());
    let mut surface = Surface::new(b.width, b.height).expect("surface");
    sb.render(&mut surface, b, Scale::ONE, &theme, font());
    for _ in 0..4 {
        assert_eq!(sb.on_key(Key::Named(NamedKey::Down)), None);
    }
    sb.on_pointer(
        &InputEvent::PointerScrolled { dx: 0, dy: 20 },
        b,
        Scale::ONE,
        &theme,
        font(),
    );
    assert!(sb.scroll_offset() > 0);

    sb.set_model(&SwitchboardModel::new("Switchboard"));

    assert_eq!(sb.scroll_offset(), 0, "an empty list has nowhere to scroll");
    assert_eq!(sb.active().content_focus(), 0);
    assert_eq!(
        sb.on_key(Key::Named(NamedKey::Enter)),
        None,
        "an emptied section has nothing to activate"
    );
    sb.render(&mut surface, b, Scale::ONE, &theme, font());
    let layout = sb.compute_layout(b, Scale::ONE, &theme);
    assert!(layout.content.bottom() <= b.bottom());
}

#[test]
fn pointer_after_set_model_addresses_the_new_rows() {
    let theme = Theme::dark();
    let b = bounds();
    let mut sb = Switchboard::new(&model());
    let mut surface = Surface::new(b.width, b.height).expect("surface");
    sb.render(&mut surface, b, Scale::ONE, &theme, font());

    // Three tasks replace fifty, and the first of the three refuses every
    // command while the rest permit them.
    sb.set_model(&refreshed_model(3, 3));
    sb.render(&mut surface, b, Scale::ONE, &theme, font());

    // Choosing row 2 must select the task the refresh put there.
    select_task_row(&mut sb, b, Scale::ONE, &theme, 2);
    let switch = centre(task_rail_rects(&sb, b, Scale::ONE, &theme)[0]);
    assert!(
        click(&mut sb, b, Scale::ONE, &theme, switch.0, switch.1).contains(
            &SwitchboardAction::Task {
                index: 2,
                control: TaskControl::Switch,
            }
        ),
        "the command names the task now at that row"
    );

    // Row 0's replacement refuses everything, so its commands fail closed.
    select_task_row(&mut sb, b, Scale::ONE, &theme, 0);
    let switch = centre(task_rail_rects(&sb, b, Scale::ONE, &theme)[0]);
    assert!(
        click(&mut sb, b, Scale::ONE, &theme, switch.0, switch.1).is_empty(),
        "the refused new row must answer, not the permitted row it replaced"
    );

    // Row three is gone; a press one row-height below the last row it does
    // have must select nothing at all.
    let layout = sb.compute_layout(b, Scale::ONE, &theme);
    let info = sb.list_info(&layout, Scale::ONE, &theme);
    let last = info.item_rect(2);
    let (x, y) = (
        centre(last).0,
        centre(last).1 + to_i32(last.height).saturating_add(4),
    );
    let before = sb.tasks.selected;
    assert!(click(&mut sb, b, Scale::ONE, &theme, x, y).is_empty());
    assert_eq!(
        sb.tasks.selected, before,
        "a row the refresh removed must never be selectable"
    );
}

#[test]
fn set_model_cannot_complete_a_press_begun_on_the_row_it_replaced() {
    let theme = Theme::dark();
    let b = bounds();
    let mut sb = Switchboard::new(&model());
    let mut surface = Surface::new(b.width, b.height).expect("surface");
    sb.render(&mut surface, b, Scale::ONE, &theme, font());
    // Move the selection off row 0 first, so a press completing there would
    // be visible as a change rather than hidden by the resting selection.
    select_task_row(&mut sb, b, Scale::ONE, &theme, 3);
    let held = sb.tasks.selected.expect("row 3 selected");
    let (x, y) = task_row_point(&sb, b, Scale::ONE, &theme, 0);

    // Arm row 0, refresh under the held pointer, let go. The replacement row
    // sits at the same place, so only the dropped arm can keep the release
    // from selecting it.
    assert_eq!(
        sb.on_pointer(&moved(x, y), b, Scale::ONE, &theme, font()),
        None
    );
    assert_eq!(sb.on_pointer(&PRESS, b, Scale::ONE, &theme, font()), None);
    sb.set_model(&model());

    assert_eq!(sb.on_pointer(&RELEASE, b, Scale::ONE, &theme, font()), None);
    assert_eq!(
        sb.tasks.selected,
        Some(held),
        "a press must not complete against the row that replaced its target"
    );

    select_task_row(&mut sb, b, Scale::ONE, &theme, 0);
    assert_eq!(
        sb.tasks.selected,
        Some(task_id(0)),
        "a fresh gesture on the new row must still work"
    );
}

#[test]
fn a_section_change_drops_an_in_flight_inline_edit() {
    let theme = Theme::dark();
    let mut sb = Switchboard::new(&model());
    sb.select_section(Section::Activities);
    let _ = painted(&mut sb, &theme);
    // Rename is the third action on the focused header row.
    for _ in 0..2 {
        sb.on_key(Key::Named(NamedKey::Right));
    }
    sb.on_key(Key::Named(NamedKey::Enter));
    assert!(
        sb.activities.rename.is_some(),
        "the edit must genuinely be open, or this proves nothing"
    );

    sb.select_section(Section::Tasks);

    assert!(
        sb.activities.rename.is_none(),
        "an edit naming a row the reader has navigated away from must not \
         outlive the section that opened it"
    );
    assert_eq!(
        sb.on_key(Key::Named(NamedKey::Down)),
        None,
        "and the keyboard must reach the new section rather than the edit"
    );
    assert_eq!(
        sb.active().content_focus(),
        1,
        "the arrow moved the shown section's cursor, so no invisible edit \
         swallowed it"
    );
}

#[test]
fn new_then_set_model_draws_what_building_with_that_model_draws() {
    let theme = Theme::dark();
    let b = bounds();
    // Neither has been interacted with, so there is no preserved state to
    // account for: any difference would be a second derivation.
    let mut refreshed = Switchboard::new(&model());
    refreshed.set_model(&refreshed_model(4, 2));
    let mut built = Switchboard::new(&refreshed_model(4, 2));

    let mut refreshed_surface = Surface::new(b.width, b.height).expect("surface");
    let mut built_surface = Surface::new(b.width, b.height).expect("surface");
    refreshed.render(&mut refreshed_surface, b, Scale::ONE, &theme, font());
    built.render(&mut built_surface, b, Scale::ONE, &theme, font());

    assert_eq!(
        refreshed_surface.pixels(),
        built_surface.pixels(),
        "one derivation must draw one surface"
    );
    assert_eq!(refreshed, built, "one derivation must leave one state");
}

#[test]
fn six_sections_in_order() {
    assert_eq!(
        Section::ALL.map(Section::title),
        [
            "Tasks",
            "Jobs",
            "Pressure",
            "Activities",
            "Recovery",
            "System"
        ]
    );
    for (i, section) in Section::ALL.iter().enumerate() {
        assert_eq!(section.index(), i);
        assert_eq!(Section::from_index(i), Some(*section));
    }
    assert_eq!(Section::from_index(Section::ALL.len()), None);
}

#[test]
fn offsets_persist_for_the_new_sections() {
    let theme = Theme::dark();
    let mut sb = Switchboard::new(&model());
    let b = bounds();
    sb.select_section(Section::Pressure);
    sb.on_pointer(
        &InputEvent::PointerScrolled { dx: 0, dy: 2 },
        b,
        Scale::ONE,
        &theme,
        font(),
    );
    assert_eq!(sb.scroll_offset(), 2);
    sb.select_section(Section::Activities);
    assert_eq!(sb.scroll_offset(), 0);
    sb.on_pointer(
        &InputEvent::PointerScrolled { dx: 0, dy: 3 },
        b,
        Scale::ONE,
        &theme,
        font(),
    );
    assert_eq!(sb.scroll_offset(), 3);
    sb.select_section(Section::Pressure);
    assert_eq!(sb.scroll_offset(), 2);
    sb.select_section(Section::Activities);
    assert_eq!(sb.scroll_offset(), 3);
}

#[test]
fn action_focus_clamps_and_resets_with_the_row_focus() {
    let mut sb = Switchboard::new(&model());
    // The Tasks table's rows carry no controls of their own, so the sideways
    // cursor has nowhere to go within a row; the filter strip, whose tabs it
    // does traverse, is where the clamp is worth proving.
    focus_task_row(&mut sb, 0);
    for key in [NamedKey::Left, NamedKey::Right] {
        assert_eq!(sb.on_key(Key::Named(key)), None);
        assert_eq!(
            sb.active().row_action(),
            0,
            "a row has one action slot, so sideways moves stay put"
        );
    }

    // A fresh screen rests on the filter strip, whose tabs the sideways
    // cursor does traverse.
    let mut sb = Switchboard::new(&model());
    let stops = sb.tasks.filters.len();
    assert_eq!(sb.on_key(Key::Named(NamedKey::Left)), None);
    assert_eq!(
        sb.active().row_action(),
        0,
        "Left at the first tab stays put"
    );
    for _ in 0..stops + 2 {
        assert_eq!(sb.on_key(Key::Named(NamedKey::Right)), None);
    }
    assert_eq!(
        sb.active().row_action(),
        stops - 1,
        "Right clamps at the last tab"
    );
    assert_eq!(sb.on_key(Key::Named(NamedKey::Down)), None);
    assert_eq!(
        sb.active().row_action(),
        0,
        "moving the cursor resets the action focus"
    );
}

#[test]
fn light_and_high_contrast_render_the_new_sections() {
    let b = bounds();
    for theme in [Theme::light(), high_contrast()] {
        for section in [Section::Pressure, Section::Activities] {
            let mut sb = Switchboard::new(&model());
            sb.select_section(section);
            let mut surface = Surface::new(b.width, b.height).expect("surface");
            sb.render(&mut surface, b, Scale::ONE, &theme, font());
            let layout = sb.compute_layout(b, Scale::ONE, &theme);
            assert!(
                has_ink(&surface, layout.content),
                "{section:?} painted nothing"
            );
        }
    }
}

// --- Render-equivalence equality (the host's repaint gate) ----------------

/// Paint `sb` at the standard bounds and hand back the surface a host would
/// present.
fn painted(sb: &mut Switchboard, theme: &Theme) -> Surface {
    let b = bounds();
    let mut surface = Surface::new(b.width, b.height).expect("surface");
    sb.render(&mut surface, b, Scale::ONE, theme, font());
    surface
}

/// A Switchboard whose layout has been settled by one render, so a following
/// pointer event resolves against the geometry the next render will use.
fn settled(theme: &Theme) -> Switchboard {
    let mut sb = Switchboard::new(&model());
    let _ = painted(&mut sb, theme);
    sb
}

/// A settled Switchboard showing a section whose rows carry an *anchored*
/// action column, which is what the Edge Wake belongs to.
///
/// The Tasks table draws its commands in a trailing column that scrolls
/// with its rows, so it has no anchored column and no wake to prove
/// anything about; Recovery still does, and it is the wake's behaviour —
/// not any one section's — these tests are about.
fn settled_railed(theme: &Theme) -> Switchboard {
    let mut sb = settled(theme);
    sb.select_section(Section::Activities);
    let _ = painted(&mut sb, theme);
    assert!(
        sb.active().row_buttons() > 0,
        "the wake needs a section with an anchored action column"
    );
    assert!(
        sb.active().item_count() > 0,
        "the wake needs rows that can move past the column"
    );
    sb
}

/// The active section's list metrics at the test bounds.
fn list_info(sb: &Switchboard, theme: &Theme) -> super::ListInfo {
    let layout = sb.compute_layout(bounds(), Scale::ONE, theme);
    sb.list_info(&layout, Scale::ONE, theme)
}

/// How many pixels wide the Edge Wake seam is at the action column's leading
/// edge, measured on the row band's vertical centre so no row plate, label,
/// or button rim is sampled instead.
fn wake_seam_width(sb: &Switchboard, surface: &Surface, theme: &Theme) -> u32 {
    let info = list_info(sb, theme);
    let Some(column) =
        Switchboard::action_column(info, sb.active().row_buttons(), Scale::ONE, theme)
    else {
        return 0;
    };
    let want = tairix_raster::Color::from(theme.palette().rim_active).premultiply();
    let y = u32::try_from(column.top()).unwrap_or(0) + column.height / 2;
    let x0 = u32::try_from(column.left()).unwrap_or(0);
    (x0..x0 + column.width)
        .take_while(|&x| surface.get(x, y) == Some(want))
        .count()
        .try_into()
        .unwrap_or(u32::MAX)
}

/// Whether the action column's leading edge carries an Edge Wake.
fn column_edge_is_lit(sb: &Switchboard, surface: &Surface, theme: &Theme) -> bool {
    wake_seam_width(sb, surface, theme) > 0
}

/// A point the composition has no control at: outside the window entirely, so
/// a sample there crosses nothing whatever the active section holds.
fn inert_point() -> (i32, i32) {
    (bounds().right() + 10, bounds().bottom() + 10)
}

/// Which Tasks row the content cursor is on, for a test that indexes the
/// section's rows directly.
fn focused_task_row(sb: &Switchboard) -> usize {
    sb.active()
        .focus_row(sb.active().content_focus())
        .expect("the cursor is on a task row")
}

/// A point inside the client but clear of the open section list.
///
/// The list is a popup anchored under the location band and overlaying the
/// content, so "press somewhere else" has to be measured against the popup
/// the composition actually drew, not against a corner of the content that
/// the popup may well cover.
fn off_menu_point(sb: &Switchboard, theme: &Theme) -> (i32, i32) {
    let b = bounds();
    let layout = sb.compute_layout(b, Scale::ONE, theme);
    let menu = sb.section_menu.as_ref().expect("the list is open");
    let rect = Switchboard::popup_rect(menu, layout.location, b, Scale::ONE, theme, font());
    let y = rect.bottom() + 4;
    assert!(
        y < layout.content.bottom(),
        "the probe must stay inside the content"
    );
    (layout.content.left() + 4, y)
}

/// A point over the first row of the active section's scrollable list.
///
/// Taken from the section's own list metrics rather than from the corner of
/// the content rect: a section with a header band of its own (the Tasks
/// table's census tiles, filters and column headings) seats its first row
/// well below that corner, and a probe there would sample the header
/// instead of a row.
fn content_point(sb: &Switchboard, theme: &Theme) -> (i32, i32) {
    let item = list_info(sb, theme).item_rect(0);
    (item.left() + 4, item.top() + to_i32(item.height / 2))
}

fn feed(sb: &mut Switchboard, theme: &Theme, event: &InputEvent) -> Option<SwitchboardAction> {
    sb.on_pointer(event, bounds(), Scale::ONE, theme, font())
}

#[test]
fn pointer_move_that_crosses_no_control_leaves_the_composition_equal() {
    let theme = Theme::dark();
    let mut sb = settled(&theme);
    let (x, y) = inert_point();
    feed(&mut sb, &theme, &moved(x, y));

    let before = sb.clone();
    feed(&mut sb, &theme, &moved(x + 5, y + 1));

    assert_ne!(
        *sb.pointer, *before.pointer,
        "the sample must genuinely land on a new coordinate, or this proves \
         nothing"
    );
    assert_eq!(
        sb, before,
        "a sample at a new coordinate that crosses no control draws the same \
         pixels, so it must not defeat a host's repaint gate"
    );
}

#[test]
fn pointer_position_alone_never_changes_the_pixels() {
    let theme = Theme::dark();
    let mut moved_pointer = settled(&theme);
    let mut resting = moved_pointer.clone();
    *moved_pointer.pointer = Point::new(517, 313);

    assert_ne!(
        *moved_pointer.pointer, *resting.pointer,
        "the two must genuinely differ in the excluded field"
    );
    assert_eq!(
        moved_pointer, resting,
        "the raw pointer coordinate is excluded from equality"
    );
    let a = painted(&mut moved_pointer, &theme);
    let b = painted(&mut resting, &theme);
    assert_eq!(
        a.pixels(),
        b.pixels(),
        "that exclusion is only sound because no render path reads it"
    );
}

#[test]
fn pointer_move_onto_a_row_changes_the_composition() {
    let theme = Theme::dark();
    let mut sb = settled(&theme);
    let (bx, by) = inert_point();
    feed(&mut sb, &theme, &moved(bx, by));

    let before = sb.clone();
    let (x, y) = content_point(&sb, &theme);
    feed(&mut sb, &theme, &moved(x, y));

    assert_ne!(
        sb, before,
        "a hover highlight is visible, so it must force a repaint"
    );
}

#[test]
fn press_and_release_each_change_the_composition() {
    let theme = Theme::dark();
    let mut sb = settled(&theme);
    let (x, y) = content_point(&sb, &theme);
    feed(&mut sb, &theme, &moved(x, y));

    let hovered = sb.clone();
    feed(&mut sb, &theme, &PRESS);
    assert_ne!(sb, hovered, "a press is visible on the pressed row");

    let pressed = sb.clone();
    feed(&mut sb, &theme, &RELEASE);
    assert_ne!(sb, pressed, "the release drops the pressed treatment");
}

#[test]
fn selection_change_changes_the_composition() {
    let theme = Theme::dark();
    let mut sb = settled(&theme);
    let before = sb.clone();

    sb.select_section(Section::Jobs);

    assert_ne!(
        sb, before,
        "the marked tab and the shown section are both visible"
    );
}

#[test]
fn focus_change_changes_the_composition() {
    let theme = Theme::dark();
    let mut sb = settled(&theme);
    let before = sb.clone();

    sb.on_key(Key::Named(NamedKey::Tab));

    assert_ne!(sb, before, "the focus ring moves to another region");
}

#[test]
fn the_focused_row_holds_the_ring_and_only_that_row() {
    let theme = Theme::dark();
    let mut sb = settled(&theme);
    focus_task_row(&mut sb, 0);

    let focused = focused_task_row(&sb);
    let entry = &sb.tasks.entries[focused];
    assert!(
        entry.row.state().focus.in_focus_field,
        "the focused row is a member of its own field"
    );
    assert!(
        entry.row.state().focus.focused,
        "a row carries no controls of its own, so it takes the ring itself"
    );

    let other = &sb.tasks.entries[focused + 1];
    assert!(
        !other.row.state().focus.in_focus_field && !other.row.state().focus.focused,
        "an unfocused row is no part of the field"
    );
}

#[test]
fn leaving_the_content_region_clears_the_focus_field() {
    let theme = Theme::dark();
    let mut sb = settled(&theme);
    focus_task_row(&mut sb, 1);
    assert!(
        sb.tasks.entries[focused_task_row(&sb)]
            .row
            .state()
            .focus
            .in_focus_field
    );

    // Content -> Scrollbar: the content list no longer holds the keyboard.
    sb.on_key(Key::Named(NamedKey::Tab));

    assert!(
        sb.tasks
            .entries
            .iter()
            .all(|t| !t.row.state().focus.in_focus_field && !t.row.state().focus.focused),
        "no row glows once focus has left the list"
    );
    assert!(
        sb.tasks
            .rail
            .items()
            .iter()
            .all(|item| !item.state().focus.in_focus_field),
        "nor does any of the selected task's commands"
    );
}

#[test]
fn an_unscrolled_list_shows_no_edge_wake() {
    let theme = Theme::dark();
    let mut sb = settled_railed(&theme);
    assert_eq!(sb.scroll_offset(), 0, "the fixture starts at the top");

    let surface = painted(&mut sb, &theme);
    assert!(
        !column_edge_is_lit(&sb, &surface, &theme),
        "nothing has moved, so the anchored column has nothing to confirm"
    );
}

#[test]
fn scrolling_the_list_wakes_the_action_columns_edge() {
    let theme = Theme::dark();
    let mut sb = settled_railed(&theme);
    feed(
        &mut sb,
        &theme,
        &InputEvent::PointerScrolled { dx: 0, dy: 3 },
    );
    assert!(sb.scroll_offset() > 0, "the list must actually scroll");

    let surface = painted(&mut sb, &theme);
    assert!(
        column_edge_is_lit(&sb, &surface, &theme),
        "the anchored column wakes on the edge the rows moved past"
    );
}

#[test]
fn scrolling_back_to_the_top_lets_the_edge_wake_settle() {
    let theme = Theme::dark();
    let mut sb = settled_railed(&theme);
    feed(
        &mut sb,
        &theme,
        &InputEvent::PointerScrolled { dx: 0, dy: 3 },
    );
    feed(
        &mut sb,
        &theme,
        &InputEvent::PointerScrolled { dx: 0, dy: -3 },
    );
    assert_eq!(sb.scroll_offset(), 0);

    let surface = painted(&mut sb, &theme);
    assert!(
        !column_edge_is_lit(&sb, &surface, &theme),
        "the wake is a state, so it clears with the displacement that caused it"
    );
}

#[test]
fn a_card_section_has_no_action_column_to_wake() {
    let theme = Theme::dark();
    let mut sb = settled(&theme);
    sb.select_section(Section::Jobs);
    feed(
        &mut sb,
        &theme,
        &InputEvent::PointerScrolled { dx: 0, dy: 3 },
    );
    assert!(sb.scroll_offset() > 0, "the job list must actually scroll");

    // A card draws its own footer actions inside itself, so there is no
    // anchored column beside the list and nothing to wake.
    let info = list_info(&sb, &theme);
    assert_eq!(
        Switchboard::action_column(info, sb.active().row_buttons(), Scale::ONE, &theme),
        None
    );
}

#[test]
fn the_edge_wake_strengthens_under_heavy_contrast() {
    let normal = Theme::dark();
    let heavy = high_contrast();
    let mut a = settled_railed(&normal);
    let mut b = settled_railed(&heavy);
    for (sb, theme) in [(&mut a, &normal), (&mut b, &heavy)] {
        feed(sb, theme, &InputEvent::PointerScrolled { dx: 0, dy: 3 });
        assert!(sb.scroll_offset() > 0, "the list must actually scroll");
    }

    let thin = painted(&mut a, &normal);
    let thick = painted(&mut b, &heavy);
    assert!(
        wake_seam_width(&b, &thick, &heavy) > wake_seam_width(&a, &thin, &normal),
        "high contrast strengthens the wake's edge rather than adding glow"
    );
}

#[test]
fn the_edge_wake_lands_on_the_action_columns_leading_edge() {
    let theme = Theme::dark();
    let sb = settled_railed(&theme);

    let info = list_info(&sb, &theme);
    let buttons_per_row = sb.active().row_buttons();
    let column =
        Switchboard::action_column(info, buttons_per_row, Scale::ONE, &theme).expect("column");
    let (_, buttons) =
        Switchboard::split_row(info.item_rect(0), buttons_per_row, Scale::ONE, &theme);
    assert_eq!(
        column.left(),
        buttons.first().expect("an action button").left(),
        "the column starts exactly where its first button does, so the wake \
         cannot drift away from the controls it belongs to"
    );
    assert_eq!(column.top(), info.list_rect.top());
    assert_eq!(column.height, info.list_rect.height);
}

#[test]
fn the_focus_field_is_visible_in_the_pixels() {
    let theme = Theme::dark();
    let mut in_field = settled(&theme);
    in_field.on_key(Key::Named(NamedKey::Down));
    let mut elsewhere = in_field.clone();
    // Move focus off the list entirely, leaving the same rows on screen.
    elsewhere.on_key(Key::Named(NamedKey::Tab));

    let a = painted(&mut in_field, &theme);
    let b = painted(&mut elsewhere, &theme);
    assert_ne!(
        a.pixels(),
        b.pixels(),
        "a Focus Field the user cannot see is not a Focus Field"
    );
}

#[test]
fn scrolling_the_content_changes_the_composition() {
    let theme = Theme::dark();
    let mut sb = settled(&theme);
    let before = sb.clone();

    feed(
        &mut sb,
        &theme,
        &InputEvent::PointerScrolled { dx: 0, dy: 3 },
    );

    assert_ne!(sb.scroll_offset(), before.scroll_offset());
    assert_ne!(sb, before, "different rows are on screen");
}

#[test]
fn model_refresh_changes_the_composition() {
    let theme = Theme::dark();
    let mut sb = settled(&theme);
    let before = sb.clone();

    let mut refreshed = model();
    refreshed.tasks[0].cpu_permille = Some(990);
    sb.set_model(&refreshed);

    assert_ne!(sb, before, "a re-derived row shows the new reading");
}
