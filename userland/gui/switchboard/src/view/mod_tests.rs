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
use tairix_icon::NoArtwork;
use tairix_input::{InputEvent, Key, NamedKey};
use tairix_raster::{Color, Surface};
use tairix_theme::Theme;

use tairix_controls::testkit::high_contrast;
use tairix_controls::{ActivityState, ControlDisposition, PressureState, RecoveryState};

use crate::panel::{MIN_WIN_HEIGHT, MIN_WIN_WIDTH};

use super::test_support::{
    bounds, centre, click, focus_task_row, font, key, model, moved, pointer, refresh, report,
    resource_report, select_task_row, shot, task_id, task_rail_rects, task_row_point,
    unreported_change, PRESS, RELEASE,
};
use super::{
    resolve_section_frame, ActionVerdict, Reading, RecoveryControl, Section, Switchboard,
    SwitchboardAction, SwitchboardModel, TaskAuthority, TaskControl, TaskSummary,
};

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
    pointer(sb, bounds(), Scale::ONE, theme, event)
}

/// Which Tasks row the content cursor is on, for a test that indexes the
/// section's rows directly.
fn focused_task_row(sb: &Switchboard) -> usize {
    sb.active()
        .focus_row(sb.active().content_focus())
        .expect("the cursor is on a task row")
}

/// A point the composition has no control at: outside the window entirely, so
/// a sample there crosses nothing whatever the active section holds.
fn inert_point() -> (i32, i32) {
    (bounds().right() + 10, bounds().bottom() + 10)
}

/// The active section's list metrics at the test bounds.
/// A screen showing `model` on the Tasks section.
///
/// The surface opens on Resources, whose pane is a flow of instrument items
/// rather than a list of rows. A test about rows — hover, focus rings, the
/// pixels a press moves — says which list it means rather than relying on
/// whichever subject leads the rail.
fn on_tasks(model: &SwitchboardModel) -> Switchboard {
    let mut sb = Switchboard::new(model);
    sb.select_section(Section::Tasks);
    sb
}

fn list_info(sb: &Switchboard, theme: &Theme) -> super::ListInfo {
    let layout = Switchboard::compute_layout(bounds(), Scale::ONE, theme);
    sb.list_info(&layout, Scale::ONE, theme)
}

/// What this machine is doing is the question a monitor is opened to answer,
/// so the surface leads with it rather than with the task list.
#[test]
fn new_starts_on_resources_at_offset_zero() {
    let sb = Switchboard::new(&model());
    assert_eq!(sb.section(), Section::Resources);
    assert_eq!(sb.scroll_offset(), 0);
    assert_eq!(
        sb.resources.selected,
        model().resources.devices.first().map(|device| device.id),
        "the rail opens on the processor, the first subject of the devices"
    );
}

#[test]
fn render_paints_content() {
    let theme = Theme::dark();
    let mut sb = on_tasks(&model());
    let mut surface = Surface::new(600, 400).expect("surface");
    sb.render(
        &mut surface,
        bounds(),
        Scale::ONE,
        &theme,
        font(),
        &mut NoArtwork,
    );
    assert!(surface.pixels().iter().any(|p| p.a > 0));
}

#[test]
fn scroll_track_sits_beside_the_content_inside_bounds() {
    let theme = Theme::dark();
    let b = bounds();
    let layout = Switchboard::compute_layout(b, Scale::ONE, &theme);
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
    let b = bounds();
    let layout = Switchboard::compute_layout(b, Scale::ONE, &theme);
    // The window manager decorates server-side, so the app draws no title bar
    // of its own: its first region, the navigation rail, sits at the very top
    // left of the bounds it was handed. A re-introduced private title bar
    // would inset the client and push it down, failing this.
    assert_eq!(layout.rail.top(), b.top());
    assert_eq!(layout.rail.left(), b.left());
    assert_eq!(layout.content.top(), b.top());
    // The rail claims the leading edge and the content sits beside it, so no
    // region overlaps another.
    assert_eq!(layout.content.left(), layout.rail.right());
    assert!(layout.scroll.left() >= layout.content.right());
}

#[test]
fn wheel_scrolls_the_active_section() {
    let theme = Theme::dark();
    let mut sb = on_tasks(&model());
    let b = bounds();
    let action = pointer(
        &mut sb,
        b,
        Scale::ONE,
        &theme,
        &InputEvent::PointerScrolled { dx: 0, dy: 3 },
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
    let mut sb = on_tasks(&model());
    let mut surface = Surface::new(600, 400).expect("surface");
    // Render once so the scroll model matches the layout.
    sb.render(
        &mut surface,
        bounds(),
        Scale::ONE,
        &theme,
        font(),
        &mut NoArtwork,
    );
    // Cycle focus Content -> Scrollbar (one Tab).
    assert_eq!(key(&mut sb, Key::Named(NamedKey::Tab)), None);
    let action = key(&mut sb, Key::Named(NamedKey::Down));
    match action {
        Some(SwitchboardAction::Scrolled { offset }) => assert!(offset >= 1),
        other => panic!("expected a keyboard scroll, got {other:?}"),
    }
}

#[test]
fn no_part_of_the_client_is_left_transparent() {
    let b = bounds();
    for theme in [Theme::dark(), Theme::light(), high_contrast()] {
        let mut sb = on_tasks(&model());
        let mut surface = Surface::new(b.width, b.height).expect("surface");
        sb.render(&mut surface, b, Scale::ONE, &theme, font(), &mut NoArtwork);

        // The window manager decorates the window; its content pixels are the
        // client's own. Any pixel left clear shows whatever the shared frame
        // region held before, which reads as a transparent window.
        let clear = (0..b.width)
            .flat_map(|x| (0..b.height).map(move |y| (x, y)))
            .find(|&(x, y)| surface.get(x, y).is_some_and(|p| p.a == 0));
        assert_eq!(clear, None, "pixel {clear:?} was left transparent");
    }
}

#[test]
fn the_client_is_laid_over_the_theme_surface_tint() {
    let theme = Theme::dark();
    let b = bounds();
    let mut sb = on_tasks(&model());
    let mut surface = Surface::new(b.width, b.height).expect("surface");
    sb.render(&mut surface, b, Scale::ONE, &theme, font(), &mut NoArtwork);
    assert!(
        surface
            .pixels()
            .contains(&Color::from(theme.palette().surface).premultiply()),
        "the base surface tint must show wherever no control covers it"
    );
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
        ..TaskSummary::default()
    });
    let mut sb = Switchboard::new(&m);
    select_task_row(&mut sb, bounds(), Scale::ONE, &theme, 0);

    assert_eq!(
        sb.tasks.rail.items()[6].state().disposition(),
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
fn layout_scales_with_the_ui_scale() {
    let theme = Theme::dark();
    let one = Switchboard::compute_layout(bounds(), Scale::ONE, &theme);
    let two =
        Switchboard::compute_layout(bounds(), Scale::from_percent(200).expect("scale"), &theme);
    assert!(two.rail.width > one.rail.width);
}

#[test]
fn light_theme_renders() {
    let theme = Theme::light();
    let mut sb = on_tasks(&model());
    let mut surface = Surface::new(600, 400).expect("surface");
    sb.render(
        &mut surface,
        bounds(),
        Scale::ONE,
        &theme,
        font(),
        &mut NoArtwork,
    );
    assert!(surface.pixels().iter().any(|p| p.a > 0));
}

#[test]
fn high_contrast_theme_renders() {
    let theme = high_contrast();
    let mut sb = on_tasks(&model());
    let mut surface = Surface::new(600, 400).expect("surface");
    sb.render(
        &mut surface,
        bounds(),
        Scale::ONE,
        &theme,
        font(),
        &mut NoArtwork,
    );
    assert!(surface.pixels().iter().any(|p| p.a > 0));
}

#[test]
fn window_too_short_for_the_anatomy_still_renders_in_bounds() {
    let theme = Theme::dark();
    let mut sb = on_tasks(&model());
    // Shorter than the location band would ordinarily need.
    let b = Rect::new(0, 0, 600, 24);
    let mut surface = Surface::new(b.width, b.height).expect("surface");
    // Must not panic: every region clips to the bounds instead.
    sb.render(&mut surface, b, Scale::ONE, &theme, font(), &mut NoArtwork);
    let layout = Switchboard::compute_layout(b, Scale::ONE, &theme);
    assert!(layout.rail.bottom() <= b.bottom());
    assert!(layout.content.bottom() <= b.bottom());
    assert!(layout.scroll.bottom() <= b.bottom());
}

#[test]
fn the_minimum_window_size_seats_every_declared_anatomy() {
    let theme = Theme::dark();
    let mut sb = on_tasks(&model());
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
    let layout = Switchboard::compute_layout(b, Scale::ONE, &theme);
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
        let mut sb = on_tasks(&model());
        let changed = sb.select_section(section);
        if section == Section::Tasks {
            assert_eq!(changed, None, "Tasks is what a fresh Switchboard shows");
        } else {
            assert_eq!(changed, Some(SwitchboardAction::SectionChanged { section }));
        }
        assert_eq!(sb.section(), section);
        let mut surface = Surface::new(b.width, b.height).expect("surface");
        sb.render(&mut surface, b, Scale::ONE, &theme, font(), &mut NoArtwork);
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
    let mut sb = on_tasks(&model());
    let b = bounds();
    let mut surface = Surface::new(b.width, b.height).expect("surface");
    // Scroll deep into the long (50-item) Tasks list.
    pointer(
        &mut sb,
        b,
        Scale::ONE,
        &theme,
        &InputEvent::PointerScrolled { dx: 0, dy: 20 },
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
    sb.render(&mut surface, b, Scale::ONE, &theme, font(), &mut NoArtwork);
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
    sb.render(&mut surface, b, Scale::ONE, &theme, font(), &mut NoArtwork);
    let range = sb.scroll.model().range();
    assert_eq!(range.content_extent(), 50);
    assert_eq!(sb.scroll_offset(), deep);
    assert!(range.offset() <= range.max_offset());
}

#[test]
fn selecting_the_shown_section_changes_nothing() {
    let theme = Theme::dark();
    let mut sb = on_tasks(&model());
    let b = bounds();
    pointer(
        &mut sb,
        b,
        Scale::ONE,
        &theme,
        &InputEvent::PointerScrolled { dx: 0, dy: 4 },
    );
    // Move the keyboard off the first item too, so a stray reset would show.
    assert_eq!(key(&mut sb, Key::Named(NamedKey::Down)), None);
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
    let mut sb = on_tasks(&model());
    let b = bounds();
    sb.select_section(Section::Recovery);
    let layout = Switchboard::compute_layout(b, Scale::ONE, &theme);
    // Recovery's commands moved from the row into an anchored rail, so the
    // aim moved with them: the rail sits in a column the Tasks section does
    // not seat at all, so only the new section can answer.
    let frame = resolve_section_frame(layout.content, sb.active().anatomy(), Scale::ONE, &theme);
    let rail = crate::view::recovery::RecoverySection::rail_content(&frame, Scale::ONE, &theme)
        .expect("the default window seats the recovery rail");
    let (x, y) = centre(
        sb.recovery
            .rail
            .item_rect(rail, 0, Scale::ONE, &theme)
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

/// Walking the rail onto a subject and asking for its section directly are
/// the same transition, so they must leave the same state: the rail is the
/// only route a reader has, and a host that asks for a section must not land
/// somewhere the rail could not.
#[test]
fn direct_selection_and_the_keyboard_path_agree() {
    let mut by_key = on_tasks(&model());
    let mut direct = on_tasks(&model());
    // Put both on the rail (Content -> Scrollbar -> Rail) so the only
    // difference is how the subject is chosen.
    for _ in 0..2 {
        assert_eq!(key(&mut by_key, Key::Named(NamedKey::Tab)), None);
        assert_eq!(key(&mut direct, Key::Named(NamedKey::Tab)), None);
    }
    // Recovery is the last subject the rail lists, so End walks straight to
    // it — and on a rail the cursor *is* the choice.
    let by_key_action = key(&mut by_key, Key::Named(NamedKey::End));
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

/// A model of `tasks` tasks and `devices` resource devices and nothing else,
/// for a refresh that shortens, empties, or re-populates a section.
///
/// The first task's action is refused while the rest are permitted. The base
/// [`model`] permits every one of its tasks, so a refused first row is how a
/// test tells a refreshed row apart from the one it replaced: only the new row
/// can answer a click with silence.
fn refreshed_model(tasks: usize, devices: usize) -> SwitchboardModel {
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
            ..TaskSummary::default()
        });
    }
    let report = resource_report();
    m.resources.devices = report.devices.into_iter().take(devices).collect();
    m
}

#[test]
fn set_model_clamps_an_offset_past_the_end_of_a_shorter_list() {
    let theme = Theme::dark();
    let b = bounds();
    let mut sb = on_tasks(&model());
    let mut surface = Surface::new(b.width, b.height).expect("surface");
    sb.render(&mut surface, b, Scale::ONE, &theme, font(), &mut NoArtwork);
    pointer(
        &mut sb,
        b,
        Scale::ONE,
        &theme,
        &InputEvent::PointerScrolled { dx: 0, dy: 40 },
    );
    assert!(
        sb.scroll_offset() > 5,
        "the 50-item list must scroll well past a 5-item one"
    );

    // Five tasks have nowhere near that far to scroll: the refresh re-ranges
    // there and then, rather than leaving a dangling offset for the next frame.
    let _ = refresh(&mut sb, &refreshed_model(5, 3));

    let range = sb.scroll.model().range();
    assert_eq!(range.content_extent(), 5);
    assert!(range.offset() <= range.max_offset());
    assert!(
        sb.scroll_offset() < 5,
        "the offset must land inside the new list"
    );

    sb.render(&mut surface, b, Scale::ONE, &theme, font(), &mut NoArtwork);
    let range = sb.scroll.model().range();
    assert_eq!(range.content_extent(), 5);
    assert!(range.offset() <= range.max_offset());
}

#[test]
fn set_model_to_an_empty_model_stays_valid_and_renderable() {
    let theme = Theme::dark();
    let b = bounds();
    let mut sb = on_tasks(&model());
    let mut surface = Surface::new(b.width, b.height).expect("surface");
    sb.render(&mut surface, b, Scale::ONE, &theme, font(), &mut NoArtwork);
    for _ in 0..4 {
        assert_eq!(key(&mut sb, Key::Named(NamedKey::Down)), None);
    }
    pointer(
        &mut sb,
        b,
        Scale::ONE,
        &theme,
        &InputEvent::PointerScrolled { dx: 0, dy: 20 },
    );
    assert!(sb.scroll_offset() > 0);

    let _ = refresh(&mut sb, &SwitchboardModel::new("Switchboard"));

    assert_eq!(sb.scroll_offset(), 0, "an empty list has nowhere to scroll");
    assert_eq!(sb.active().content_focus(), 0);
    assert_eq!(
        key(&mut sb, Key::Named(NamedKey::Enter)),
        None,
        "an emptied section has nothing to activate"
    );
    sb.render(&mut surface, b, Scale::ONE, &theme, font(), &mut NoArtwork);
    let layout = Switchboard::compute_layout(b, Scale::ONE, &theme);
    assert!(layout.content.bottom() <= b.bottom());
}

#[test]
fn pointer_after_set_model_addresses_the_new_rows() {
    let theme = Theme::dark();
    let b = bounds();
    let mut sb = on_tasks(&model());
    let mut surface = Surface::new(b.width, b.height).expect("surface");
    sb.render(&mut surface, b, Scale::ONE, &theme, font(), &mut NoArtwork);

    // Three tasks replace fifty, and the first of the three refuses every
    // command while the rest permit them.
    let _ = refresh(&mut sb, &refreshed_model(3, 3));
    sb.render(&mut surface, b, Scale::ONE, &theme, font(), &mut NoArtwork);

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
    let layout = Switchboard::compute_layout(b, Scale::ONE, &theme);
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
    let mut sb = on_tasks(&model());
    let mut surface = Surface::new(b.width, b.height).expect("surface");
    sb.render(&mut surface, b, Scale::ONE, &theme, font(), &mut NoArtwork);
    // Move the selection off row 0 first, so a press completing there would
    // be visible as a change rather than hidden by the resting selection.
    select_task_row(&mut sb, b, Scale::ONE, &theme, 3);
    let held = sb.tasks.selected.expect("row 3 selected");
    let (x, y) = task_row_point(&sb, b, Scale::ONE, &theme, 0);

    // Arm row 0, refresh under the held pointer, let go. The replacement row
    // sits at the same place, so only the dropped arm can keep the release
    // from selecting it.
    assert_eq!(pointer(&mut sb, b, Scale::ONE, &theme, &moved(x, y)), None);
    assert_eq!(pointer(&mut sb, b, Scale::ONE, &theme, &PRESS), None);
    let _ = refresh(&mut sb, &model());

    assert_eq!(pointer(&mut sb, b, Scale::ONE, &theme, &RELEASE), None);
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
fn new_then_set_model_draws_what_building_with_that_model_draws() {
    let theme = Theme::dark();
    let b = bounds();
    // Neither has been interacted with, so there is no preserved state to
    // account for: any difference would be a second derivation. Choosing a
    // section would be an interaction, so both stay where the surface opens.
    let mut refreshed = Switchboard::new(&model());
    let _ = refresh(&mut refreshed, &refreshed_model(4, 2));
    let mut built = Switchboard::new(&refreshed_model(4, 2));

    let mut refreshed_surface = Surface::new(b.width, b.height).expect("surface");
    let mut built_surface = Surface::new(b.width, b.height).expect("surface");
    refreshed.render(
        &mut refreshed_surface,
        b,
        Scale::ONE,
        &theme,
        font(),
        &mut NoArtwork,
    );
    built.render(
        &mut built_surface,
        b,
        Scale::ONE,
        &theme,
        font(),
        &mut NoArtwork,
    );

    assert_eq!(
        refreshed_surface.pixels(),
        built_surface.pixels(),
        "one derivation must draw one surface"
    );
    assert_eq!(refreshed, built, "one derivation must leave one state");
}

#[test]
fn action_focus_clamps_and_resets_with_the_row_focus() {
    let mut sb = on_tasks(&model());
    // The Tasks table's rows carry no controls of their own, so the sideways
    // cursor has nowhere to go within a row; the column headings, whose
    // sortable columns it does traverse, are where the clamp is worth
    // proving.
    focus_task_row(&mut sb, 0);
    for sideways in [NamedKey::Left, NamedKey::Right] {
        assert_eq!(key(&mut sb, Key::Named(sideways)), None);
        assert_eq!(
            sb.active().row_action(),
            0,
            "a row has one action slot, so sideways moves stay put"
        );
    }

    // A fresh screen rests on the column headings, which the sideways cursor
    // does traverse.
    let mut sb = on_tasks(&model());
    let stops = sb.tasks.header.columns().len();
    assert_eq!(key(&mut sb, Key::Named(NamedKey::Left)), None);
    assert_eq!(
        sb.active().row_action(),
        0,
        "Left at the first heading stays put"
    );
    for _ in 0..stops + 2 {
        assert_eq!(key(&mut sb, Key::Named(NamedKey::Right)), None);
    }
    assert_eq!(
        sb.active().row_action(),
        stops - 1,
        "Right clamps at the last heading"
    );
    assert_eq!(key(&mut sb, Key::Named(NamedKey::Down)), None);
    assert_eq!(
        sb.active().row_action(),
        0,
        "moving the cursor resets the action focus"
    );
}

// --- Render-equivalence equality (the host's repaint gate) ----------------

/// Paint `sb` at the standard bounds and hand back the surface a host would
/// present.
fn painted(sb: &mut Switchboard, theme: &Theme) -> Surface {
    let b = bounds();
    let mut surface = Surface::new(b.width, b.height).expect("surface");
    sb.render(&mut surface, b, Scale::ONE, theme, font(), &mut NoArtwork);
    surface
}

/// A Switchboard whose layout has been settled by one render, so a following
/// pointer event resolves against the geometry the next render will use.
fn settled(theme: &Theme) -> Switchboard {
    let mut sb = on_tasks(&model());
    let _ = painted(&mut sb, theme);
    sb
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
fn focus_change_changes_the_composition() {
    let theme = Theme::dark();
    let mut sb = settled(&theme);
    let before = sb.clone();

    key(&mut sb, Key::Named(NamedKey::Tab));

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
    key(&mut sb, Key::Named(NamedKey::Tab));

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
fn the_focus_field_is_visible_in_the_pixels() {
    let theme = Theme::dark();
    let mut in_field = settled(&theme);
    key(&mut in_field, Key::Named(NamedKey::Down));
    let mut elsewhere = in_field.clone();
    // Move focus off the list entirely, leaving the same rows on screen.
    key(&mut elsewhere, Key::Named(NamedKey::Tab));

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
    let _ = refresh(&mut sb, &refreshed);

    assert_ne!(sb, before, "a re-derived row shows the new reading");
}

// --- What a round reports, and what it must cover -------------------------

#[test]
fn a_hover_reports_only_the_row_it_entered() {
    let theme = Theme::dark();
    let mut sb = settled(&theme);
    let row = list_info(&sb, &theme).item_rect(0);
    let (x, y) = centre(row);

    let damage = report(&mut sb, &moved(x, y));

    assert_eq!(
        damage.rects(),
        &[row],
        "entering a row from nowhere marks that row and nothing else"
    );
}

#[test]
fn a_second_sample_inside_the_same_row_reports_nothing() {
    let theme = Theme::dark();
    let mut sb = settled(&theme);
    let row = list_info(&sb, &theme).item_rect(0);
    let (x, y) = centre(row);
    let _ = report(&mut sb, &moved(x, y));

    let damage = report(&mut sb, &moved(x + 1, y));

    assert!(
        damage.is_empty(),
        "a motion that crosses no boundary changes no pixel and must report none"
    );
}

#[test]
fn a_hover_that_leaves_one_row_for_the_next_reports_both() {
    let theme = Theme::dark();
    let mut sb = settled(&theme);
    let info = list_info(&sb, &theme);
    let (first, second) = (info.item_rect(0), info.item_rect(1));
    let (x, y) = centre(first);
    let _ = report(&mut sb, &moved(x, y));

    let (x, y) = centre(second);
    let damage = report(&mut sb, &moved(x, y));

    let mut want = tairix_controls::damage::sink();
    want.add(first);
    want.add(second);
    assert_eq!(
        damage.rects(),
        want.rects(),
        "the row left and the row entered are the two that changed"
    );
}

#[test]
fn every_pixel_a_walk_moves_lies_inside_what_it_reported() {
    let theme = Theme::dark();
    for section in Section::ALL {
        let mut sb = settled(&theme);
        sb.select_section(section);
        let _ = painted(&mut sb, &theme);

        let info = list_info(&sb, &theme);
        let (row0, row1) = (centre(info.item_rect(0)), centre(info.item_rect(1)));
        let layout = Switchboard::compute_layout(bounds(), Scale::ONE, &theme);
        let rail_entry = centre(Rect::new(
            layout.rail.left(),
            layout.rail.top(),
            layout.rail.width,
            super::Switchboard::row_item_height(Scale::ONE, &theme),
        ));
        let steps = [
            moved(row0.0, row0.1),
            PRESS,
            RELEASE,
            moved(row1.0, row1.1),
            InputEvent::PointerScrolled { dx: 0, dy: 2 },
            moved(rail_entry.0, rail_entry.1),
            PRESS,
            RELEASE,
        ];

        let mut moved_any = false;
        for (index, step) in steps.iter().enumerate() {
            let before = shot(&mut sb);
            let damage = report(&mut sb, step);
            let after = shot(&mut sb);
            assert_eq!(
                unreported_change(&before, &after, bounds(), &damage),
                None,
                "{section:?} step {index} moved a pixel it did not report"
            );
            moved_any |= before.pixels() != after.pixels();
        }
        assert!(
            moved_any,
            "{section:?} drew nothing new for the whole walk, so it proved nothing"
        );
    }
}

#[test]
fn a_scroll_reports_the_whole_list_the_bar_alone_does_not_describe() {
    let theme = Theme::dark();
    let mut sb = settled(&theme);
    let before = shot(&mut sb);

    let damage = report(&mut sb, &InputEvent::PointerScrolled { dx: 0, dy: 2 });
    let after = shot(&mut sb);

    assert_ne!(sb.scroll_offset(), 0, "the fixture list is scrollable");
    assert_eq!(
        unreported_change(&before, &after, bounds(), &damage),
        None,
        "every row is drawn somewhere new, not just the scrollbar's thumb"
    );
}

// --- What a fresh reading reports ---------------------------------------

/// A model whose readings have all moved: every task's CPU cell, every fault
/// card's state, and the CPU pane's hero value.
///
/// One fixture rather than three, so the three tests below all publish the
/// same kind of change and none can pass on a section the edit missed.
fn moved_reading() -> SwitchboardModel {
    let mut m = model();
    for (index, task) in m.tasks.iter_mut().enumerate() {
        task.cpu_permille = Some(u16::try_from(index).unwrap_or(0) * 7 + 3);
    }
    for item in &mut m.recovery {
        item.recovery = RecoveryState::RestartRecommended;
    }
    if let Some(cpu) = m.resources.devices.first_mut() {
        cpu.reading = Reading::measured("77%");
        cpu.hero.value = Reading::measured("77");
    }
    m
}

#[test]
fn a_refresh_reports_every_pixel_it_moved_in_every_section() {
    for section in Section::ALL {
        let mut sb = on_tasks(&model());
        let _ = sb.select_section(section);
        let before = shot(&mut sb);

        let damage = refresh(&mut sb, &moved_reading());
        let after = shot(&mut sb);

        assert!(
            !damage.is_empty(),
            "{section:?}: a reading that moved reports the instrument it moved in"
        );
        assert_eq!(
            unreported_change(&before, &after, bounds(), &damage),
            None,
            "{section:?}: a pixel the refresh moved was left out of its report"
        );
    }
}

#[test]
fn a_refresh_reports_less_than_the_client_in_every_section() {
    for section in Section::ALL {
        let mut sb = on_tasks(&model());
        let _ = sb.select_section(section);
        let _ = shot(&mut sb);

        let damage = refresh(&mut sb, &moved_reading());

        assert_ne!(
            damage.bounds(),
            bounds(),
            "{section:?}: the whole client is what this exists to avoid"
        );
    }
}

#[test]
fn a_refresh_that_changed_the_count_reports_every_pixel_it_moved() {
    // A sample that shortens a list moves the scrollbar's thumb as well as
    // the rows, and the bar is no section's region to report.
    for section in Section::ALL {
        let mut sb = on_tasks(&model());
        let _ = sb.select_section(section);
        let before = shot(&mut sb);

        let damage = refresh(&mut sb, &refreshed_model(4, 2));
        let after = shot(&mut sb);

        assert_eq!(
            unreported_change(&before, &after, bounds(), &damage),
            None,
            "{section:?}: a pixel the shortened sample moved was left out of its report"
        );
    }
}

#[test]
fn a_refresh_that_moved_nothing_reports_nothing_in_every_section() {
    for section in Section::ALL {
        let mut sb = on_tasks(&model());
        let _ = sb.select_section(section);
        let _ = shot(&mut sb);

        let damage = refresh(&mut sb, &model());

        assert!(
            damage.is_empty(),
            "{section:?}: an unmoved reading owes the screen nothing, reported {:?}",
            damage.rects()
        );
    }
}
