//! Host tests for the composed viewer: the request/answer desk, the command
//! set, the input routing, and the damage every change owes.
//!
//! The pointer tests are written the way a user drives the app — a move to
//! where the layout actually puts a thing, then a press, then a release — and
//! no test hard-codes a coordinate, so a change to the geometry moves the
//! tests with it instead of quietly clicking empty space.

use alloc::string::String;
use alloc::vec;

use tairix_controls::damage;
use tairix_font::BitmapFont;
use tairix_geometry::{Point, Rect, Region, Scale};
use tairix_input::{InputEvent, Key, Modifiers, NamedKey, PointerButton};
use tairix_raster::Reorient;
use tairix_sandbox::imagerender::{ViewDocument, ViewFailure, ViewFormat, ViewPage, ViewRefusal};
use tairix_sandbox::SandboxError;
use tairix_theme::{TextRole, Theme, ThemeRegistry};

use super::{Command, Outcome, Refusal, View};
use crate::{Answer, Fit, Layout, Request, ZOOM_ACTUAL_PER_MILLE};

/// The window every test drives: large enough that both panels have room and
/// every band is non-empty.
const WINDOW: (u32, u32) = (1_280, 800);

/// A still picture of `width`x`height` pixels.
fn still(width: u32, height: u32) -> ViewDocument {
    ViewDocument {
        format: ViewFormat::Png,
        animated: false,
        loop_count: None,
        count: 1,
        width,
        height,
    }
}

/// An animation of `count` frames on a `width`x`height` canvas.
fn animation(count: u32, width: u32, height: u32) -> ViewDocument {
    ViewDocument {
        format: ViewFormat::Gif,
        animated: true,
        loop_count: None,
        count,
        width,
        height,
    }
}

/// A page container of `count` independent pages.
fn pages(count: u32, width: u32, height: u32) -> ViewDocument {
    ViewDocument {
        format: ViewFormat::Tiff,
        animated: false,
        loop_count: None,
        count,
        width,
        height,
    }
}

/// The face and theme every test resolves its layout through.
fn dressing() -> (ThemeRegistry, Scale) {
    (ThemeRegistry::with_builtins(), Scale::ONE)
}

/// The face for `theme` at `scale`.
fn font(theme: &Theme, scale: Scale) -> BitmapFont {
    BitmapFont::for_role(theme.fonts(), TextRole::Body, scale)
}

/// A viewer with `document` open and its first entry decoded, laid out in
/// [`WINDOW`], plus the layout and the dressing that produced it.
fn opened(document: ViewDocument) -> (View, Layout, ThemeRegistry) {
    let (registry, scale) = dressing();
    let mut view = View::new(true);
    // The open request is the only one there can be before a document lands.
    assert_eq!(view.next_request(), Some(Request::Open));
    let theme = registry.active();
    let layout = view.layout(WINDOW.0, WINDOW.1, theme, scale, font(theme, scale));
    let mut region = damage::sink();
    let outcome = view.deliver(
        Answer::Opened {
            opened: Ok((document, String::from("picture.png"), 4_096)),
        },
        &layout,
        &mut region,
    );
    assert!(outcome.changed);
    let layout = view.layout(WINDOW.0, WINDOW.1, theme, scale, font(theme, scale));
    (view, layout, registry)
}

/// Answer the render `view` is asking for, as a worker holding a `page_size`
/// page would: the pixels of exactly the window asked for, and the *page's
/// own* geometry — never the render extent, which is the scaled picture.
fn serve(view: &mut View, layout: &Layout, page_size: (u32, u32)) -> bool {
    let Some(Request::Show {
        page,
        extent,
        window,
        mut pixels,
    }) = view.next_request()
    else {
        return false;
    };
    pixels.clear();
    pixels.resize((window.width * window.height * 4) as usize, 0x40);
    let mut region = damage::sink();
    view.deliver(
        Answer::Shown {
            page,
            extent,
            window,
            decoded: Some(ViewPage {
                index: page,
                width: page_size.0,
                height: page_size.1,
                delay_ns: 20_000_000,
            }),
            pixels,
            outcome: Ok(()),
        },
        layout,
        &mut region,
    )
    .changed
}

/// A viewer with `document` open, its first entry decoded, and its first
/// window drawn.
fn drawn(document: ViewDocument) -> (View, Layout, ThemeRegistry) {
    let page_size = (document.width, document.height);
    let (mut view, layout, registry) = opened(document);
    assert!(
        serve(&mut view, &layout, page_size),
        "the first window was drawn"
    );
    (view, layout, registry)
}

/// Run `command` and report the outcome plus the damage it owed.
fn run(view: &mut View, layout: &Layout, command: Command) -> (Outcome, Region) {
    let mut region = damage::sink();
    let outcome = view.run(command, layout, &mut region);
    (outcome, region)
}

// ---- the desk ----------------------------------------------------------

#[test]
fn nothing_is_asked_for_before_the_document_is_open() {
    // The property the pending state makes unrepresentable: a render before
    // an open would be a render of nothing, and would displace the open on a
    // latest-wins desk. The open stays outstanding rather than being handed
    // over once, because only the embedder knows when it holds a source.
    let mut view = View::new(true);
    assert_eq!(view.next_request(), Some(Request::Open));
    assert_eq!(view.next_request(), Some(Request::Open));
    assert!(view.document().is_none());
}

#[test]
fn a_document_replacing_another_costs_no_render_of_the_one_it_replaces() {
    let (mut view, layout, _registry) = drawn(still(400, 300));
    // The user chose another file, so the embedder says one is on its way.
    view.expect_document();
    assert!(view.refusal().is_none(), "nothing has gone wrong");
    assert_eq!(
        view.next_request(),
        Some(Request::Open),
        "the open is what is wanted, not a draw of the old picture"
    );
    let mut region = damage::sink();
    view.deliver(
        Answer::Opened {
            opened: Ok((still(64, 64), String::from("other.png"), 128)),
        },
        &layout,
        &mut region,
    );
    assert_eq!(view.document().expect("open").natural(), (64, 64));
    assert!(matches!(view.next_request(), Some(Request::Show { .. })));
}

#[test]
fn a_viewer_launched_with_no_document_asks_for_nothing_at_all() {
    let mut view = View::new(false);
    assert_eq!(view.next_request(), None);
    assert!(view.document().is_none());
    assert!(view.refusal().is_none(), "nothing has gone wrong yet");
}

#[test]
fn one_render_is_outstanding_at_a_time() {
    let (mut view, _layout, _registry) = opened(still(400, 300));
    assert!(matches!(view.next_request(), Some(Request::Show { .. })));
    assert_eq!(
        view.next_request(),
        None,
        "a second render is not asked for while one is in flight"
    );
}

#[test]
fn a_drawn_window_is_not_asked_for_again() {
    let (mut view, layout, _registry) = drawn(still(400, 300));
    assert_eq!(
        view.next_request(),
        None,
        "what is held is already what the state calls for"
    );
    let _ = layout;
}

#[test]
fn the_pixel_buffer_comes_back_and_is_lent_out_again() {
    // The property that keeps a pan free of allocation: the buffer the worker
    // drew into is handed back on the next request rather than a fresh one
    // being made.
    let (mut view, layout, _registry) = drawn(still(4_000, 3_000));
    let (outcome, _) = run(&mut view, &layout, Command::ActualSize);
    assert!(outcome.changed);
    let Some(Request::Show { pixels, .. }) = view.next_request() else {
        panic!("a render was asked for");
    };
    assert!(
        pixels.capacity() > 0,
        "the buffer the last render used was lent out again"
    );
}

#[test]
fn a_superseded_answer_is_dropped_rather_than_drawn() {
    let (mut view, layout, _registry) = drawn(still(4_000, 3_000));
    // Ask for one render, then change the state again so the answer describes
    // a rectangle the user has already moved away from.
    let (outcome, _) = run(&mut view, &layout, Command::ActualSize);
    assert!(outcome.changed, "a render is now called for");
    let Some(Request::Show {
        page,
        extent,
        window,
        mut pixels,
    }) = view.next_request()
    else {
        panic!("a render was asked for");
    };
    let (outcome, _) = run(&mut view, &layout, Command::FitWindow);
    assert!(outcome.changed, "the state moved on");
    pixels.resize((window.width * window.height * 4) as usize, 0xFF);
    let mut region = damage::sink();
    let delivered = view.deliver(
        Answer::Shown {
            page,
            extent,
            window,
            decoded: None,
            pixels,
            outcome: Ok(()),
        },
        &layout,
        &mut region,
    );
    assert!(!delivered.changed, "a stale answer changes nothing");
    assert!(region.is_empty(), "and owes no repaint");
}

#[test]
fn a_refused_open_states_the_reason_and_holds_no_document() {
    let (registry, scale) = dressing();
    let theme = registry.active();
    let mut view = View::new(true);
    let layout = view.layout(WINDOW.0, WINDOW.1, theme, scale, font(theme, scale));
    let mut region = damage::sink();
    let outcome = view.deliver(
        Answer::Opened {
            opened: Err(Refusal::Failed(ViewFailure::Refused(ViewRefusal::TooLarge))),
        },
        &layout,
        &mut region,
    );
    assert!(outcome.changed);
    assert!(view.document().is_none());
    assert!(matches!(view.refusal(), Some(Refusal::Failed(_))));
    assert_eq!(
        view.next_request(),
        None,
        "nothing is asked for about a document that would not open"
    );
}

#[test]
fn a_refused_render_states_the_reason_and_keeps_showing_what_it_had() {
    let (mut view, layout, _registry) = drawn(still(4_000, 3_000));
    let (outcome, _) = run(&mut view, &layout, Command::ActualSize);
    assert!(outcome.changed);
    let Some(Request::Show {
        page,
        extent,
        window,
        pixels,
    }) = view.next_request()
    else {
        panic!("a render was asked for");
    };
    let mut region = damage::sink();
    let delivered = view.deliver(
        Answer::Shown {
            page,
            extent,
            window,
            decoded: None,
            pixels,
            outcome: Err(Refusal::Failed(ViewFailure::Sandbox(
                SandboxError::WorkerFailed,
            ))),
        },
        &layout,
        &mut region,
    );
    assert!(delivered.changed);
    assert!(matches!(view.refusal(), Some(Refusal::Failed(_))));
    assert!(
        view.document().is_some(),
        "the document is still open; only this draw failed"
    );
}

#[test]
fn a_cancelled_pick_leaves_the_window_open_and_says_so() {
    // A refused optional action is an answer, not a death.
    let mut view = View::new(false);
    assert!(view.cancelled());
    assert!(matches!(view.refusal(), Some(Refusal::Cancelled)));

    // With a document already open, a cancelled pick says nothing new: the
    // picture on screen is still what the user is looking at.
    let (mut open, _layout, _registry) = drawn(still(400, 300));
    assert!(!open.cancelled());
    assert!(open.refusal().is_none());
}

// ---- commands ----------------------------------------------------------

#[test]
fn the_zoom_tools_step_the_ladder_and_the_slider_follows() {
    let (mut view, layout, _registry) = drawn(still(4_000, 3_000));
    let before = view.viewport().zoom();
    let (outcome, region) = run(&mut view, &layout, Command::ZoomIn);
    assert!(outcome.changed);
    assert!(view.viewport().zoom() > before);
    assert_eq!(
        view.viewport().fit,
        Fit::Free,
        "a tool sets the user's factor"
    );
    assert!(
        region
            .rects()
            .iter()
            .any(|rect| *rect == layout.zoom_slider())
            || region
                .rects()
                .iter()
                .any(|rect| { !rect.intersection(&layout.zoom_slider()).is_empty() }),
        "the slider reports the zoom, so it is repainted"
    );
    assert_eq!(
        crate::slider_at_zoom(view.viewport().zoom()),
        view.zoom_control().value(),
        "the control is a view of the zoom, never a second copy of it"
    );
}

#[test]
fn actual_size_and_fit_are_different_answers() {
    let (mut view, layout, _registry) = drawn(still(4_000, 3_000));
    let (_, _) = run(&mut view, &layout, Command::ActualSize);
    assert_eq!(view.viewport().zoom(), ZOOM_ACTUAL_PER_MILLE);
    assert_eq!(view.viewport().fit, Fit::Actual);
    let (_, _) = run(&mut view, &layout, Command::FitWindow);
    assert!(
        view.viewport().zoom() < ZOOM_ACTUAL_PER_MILLE,
        "a big picture shrinks to fit"
    );
    assert_eq!(view.viewport().fit, Fit::Window);
}

#[test]
fn turning_composes_and_swaps_the_displayed_axes() {
    let (mut view, layout, _registry) = drawn(still(400, 300));
    let (outcome, _) = run(&mut view, &layout, Command::RotateRight);
    assert!(outcome.changed);
    assert_eq!(view.viewport().reorient, Reorient::QuarterTurnRight);
    assert_eq!(view.viewport().displayed((400, 300)), (300, 400));
    let (_, _) = run(&mut view, &layout, Command::RotateLeft);
    assert_eq!(
        view.viewport().reorient,
        Reorient::None,
        "back where it started"
    );
    let (_, _) = run(&mut view, &layout, Command::Mirror);
    assert_eq!(view.viewport().reorient, Reorient::FlipHorizontal);
}

#[test]
fn a_turn_is_asked_for_in_page_space_and_the_answer_is_shown_turned() {
    let (mut view, layout, _registry) = drawn(still(400, 300));
    let (_, _) = run(&mut view, &layout, Command::RotateRight);
    let Some(Request::Show { extent, window, .. }) = view.next_request() else {
        panic!("a render was asked for");
    };
    // The worker knows nothing of the turn, so both the extent and the window
    // it is sent are in the page's own axes.
    assert!(
        extent.0 >= extent.1,
        "a landscape page stays landscape in the request: {extent:?}"
    );
    assert!(window.width <= extent.0 && window.height <= extent.1);
}

#[test]
fn stepping_pages_stops_at_either_end() {
    const PAGE: (u32, u32) = (400, 300);
    let (mut view, layout, _registry) = drawn(pages(3, 400, 300));
    assert_eq!(view.document().expect("open").index(), 0);
    let (outcome, _) = run(&mut view, &layout, Command::PreviousPage);
    assert!(!outcome.changed, "already at the first page");

    for expected in 1..3 {
        let (outcome, _) = run(&mut view, &layout, Command::NextPage);
        assert!(outcome.changed, "moved to page {expected}");
        assert!(serve(&mut view, &layout, PAGE));
        assert_eq!(view.document().expect("open").index(), expected);
    }
    let (outcome, _) = run(&mut view, &layout, Command::NextPage);
    assert!(!outcome.changed, "already at the last page");
}

#[test]
fn a_page_change_keeps_the_turn_and_the_fit_but_not_the_pan() {
    const PAGE: (u32, u32) = (4_000, 3_000);
    let (mut view, layout, _registry) = drawn(pages(3, 4_000, 3_000));
    let (_, _) = run(&mut view, &layout, Command::RotateRight);
    let (_, _) = run(&mut view, &layout, Command::ActualSize);
    assert!(serve(&mut view, &layout, PAGE));
    let (_, _) = run(&mut view, &layout, Command::Pan { dx: 3, dy: 3 });
    assert_ne!(view.viewport().pan(), (0, 0));

    let (outcome, _) = run(&mut view, &layout, Command::NextPage);
    assert!(outcome.changed);
    assert_eq!(
        view.viewport().reorient,
        Reorient::QuarterTurnRight,
        "the turn is the user's and survives"
    );
    assert_eq!(view.viewport().fit, Fit::Actual, "so is the fit");
    assert_eq!(
        view.viewport().pan(),
        (0, 0),
        "a different page is not the same picture at the same offset"
    );
}

#[test]
fn a_still_picture_cannot_be_played() {
    let (mut view, layout, _registry) = drawn(still(400, 300));
    let (outcome, _) = run(&mut view, &layout, Command::TogglePlayback);
    assert!(!outcome.changed);
    assert!(!view.playing());
    assert_eq!(view.deadline_ns(), None, "and arms no timer");
}

#[test]
fn playback_arms_exactly_one_deadline_and_a_pause_disarms_it() {
    let (mut view, layout, _registry) = drawn(animation(4, 400, 300));
    assert_eq!(view.deadline_ns(), None, "a paused viewer arms nothing");
    let (outcome, _) = run(&mut view, &layout, Command::TogglePlayback);
    assert!(outcome.changed);
    assert!(view.playing());

    view.arm_deadline(1_000);
    let first = view.deadline_ns().expect("a deadline was armed");
    assert!(first > 1_000);
    view.arm_deadline(2_000);
    assert_eq!(
        view.deadline_ns(),
        Some(first),
        "one deadline, not one per call"
    );

    let (_, _) = run(&mut view, &layout, Command::TogglePlayback);
    assert!(!view.playing());
    assert_eq!(view.deadline_ns(), None, "a pause arms no timer at all");
}

#[test]
fn a_tick_before_the_deadline_does_nothing_and_one_after_it_steps_the_frame() {
    const PAGE: (u32, u32) = (400, 300);
    let (mut view, layout, _registry) = drawn(animation(4, 400, 300));
    let (_, _) = run(&mut view, &layout, Command::TogglePlayback);
    view.arm_deadline(0);
    let deadline = view.deadline_ns().expect("armed");
    assert!(!view.tick(deadline - 1), "not due yet");
    assert!(view.tick(deadline), "due");
    assert_eq!(view.deadline_ns(), None, "the deadline is spent");
    assert!(serve(&mut view, &layout, PAGE));
    assert_eq!(view.document().expect("open").index(), 1);
}

#[test]
fn playback_wraps_at_the_last_frame() {
    const PAGE: (u32, u32) = (400, 300);
    let (mut view, layout, _registry) = drawn(animation(2, 400, 300));
    let (_, _) = run(&mut view, &layout, Command::TogglePlayback);
    for expected in [1, 0, 1] {
        view.arm_deadline(0);
        let deadline = view.deadline_ns().expect("armed");
        assert!(view.tick(deadline));
        assert!(serve(&mut view, &layout, PAGE));
        assert_eq!(view.document().expect("open").index(), expected);
    }
}

#[test]
fn a_paused_viewer_never_steps_however_late_the_clock_is() {
    let (mut view, _layout, _registry) = drawn(animation(4, 400, 300));
    assert!(!view.tick(u64::MAX));
    assert_eq!(view.document().expect("open").index(), 0);
}

// ---- damage ------------------------------------------------------------

#[test]
fn a_pan_repaints_the_canvas_and_its_chrome_and_nothing_more() {
    const PAGE: (u32, u32) = (4_000, 3_000);
    let (mut view, layout, _registry) = drawn(still(4_000, 3_000));
    let (_, _) = run(&mut view, &layout, Command::ActualSize);
    assert!(serve(&mut view, &layout, PAGE));
    let (outcome, region) = run(&mut view, &layout, Command::Pan { dx: 1, dy: 1 });
    assert!(outcome.changed);
    assert!(
        !region.is_empty(),
        "a change that reports nothing would never appear"
    );
    let covers = |rect: Rect| {
        region
            .rects()
            .iter()
            .any(|part| part.intersection(&rect) == rect)
    };
    assert!(covers(layout.canvas()), "the picture moved");
    assert!(
        covers(layout.status()),
        "the status line reports where it is"
    );
    assert!(
        !covers(layout.window()),
        "a pan does not reshape the window, so it does not repaint it"
    );
}

#[test]
fn opening_a_panel_repaints_the_window_because_every_band_moved() {
    let (mut view, layout, _registry) = drawn(still(400, 300));
    let (outcome, region) = run(&mut view, &layout, Command::ToggleInfo);
    assert!(outcome.changed);
    assert!(view.info_open());
    assert!(
        region
            .rects()
            .iter()
            .any(|part| part.intersection(&layout.window()) == layout.window()),
        "the bands moved, so nothing on screen can be kept"
    );
}

#[test]
fn a_command_that_changes_nothing_reports_no_damage() {
    // The property that stops a viewer repainting on every keystroke: a
    // command whose state is already what it asks for owes nothing.
    let (mut view, layout, _registry) = drawn(still(400, 300));
    let (_, _) = run(&mut view, &layout, Command::FitWindow);
    let (outcome, region) = run(&mut view, &layout, Command::FitWindow);
    assert!(!outcome.changed);
    assert!(region.is_empty());
}

#[test]
fn a_command_with_nothing_open_touches_only_what_it_can() {
    let mut view = View::new(false);
    let (registry, scale) = dressing();
    let theme = registry.active();
    let layout = view.layout(WINDOW.0, WINDOW.1, theme, scale, font(theme, scale));
    for command in [
        Command::ZoomIn,
        Command::ZoomOut,
        Command::FitWindow,
        Command::RotateRight,
        Command::NextPage,
        Command::TogglePlayback,
        Command::Pan { dx: 1, dy: 1 },
    ] {
        let (outcome, region) = run(&mut view, &layout, command);
        assert!(!outcome.changed, "{command:?} acted on no picture");
        assert!(region.is_empty(), "{command:?} owed no repaint");
    }
    // The panel toggle acts with nothing open, because it is about the window
    // rather than about a picture.
    let (outcome, _) = run(&mut view, &layout, Command::ToggleInfo);
    assert!(outcome.changed);
    assert!(view.info_open());
}

// ---- input routing -----------------------------------------------------

#[test]
fn clicking_a_tool_runs_its_command() {
    let (mut view, layout, registry) = drawn(still(4_000, 3_000));
    let theme = registry.active();
    let tools = layout.tools();
    // The zoom-out tool is first, so its own slot is the leading one.
    let target = Point {
        x: tools.left() + tairix_geometry::to_i32(tools.height / 2),
        y: tools.top() + tairix_geometry::to_i32(tools.height / 2),
    };
    let before = view.viewport().zoom();
    let mut region = damage::sink();
    for event in [
        InputEvent::PointerMoved { to: target },
        InputEvent::PointerPressed {
            button: PointerButton::Primary,
        },
        InputEvent::PointerReleased {
            button: PointerButton::Primary,
        },
    ] {
        view.on_pointer(&event, &layout, Scale::ONE, theme, &mut region);
    }
    assert!(
        view.viewport().zoom() < before,
        "the leading tool reduced the picture"
    );
}

#[test]
fn dragging_the_canvas_pans_the_picture_the_other_way() {
    const PAGE: (u32, u32) = (4_000, 3_000);
    let (mut view, layout, registry) = drawn(still(4_000, 3_000));
    let theme = registry.active();
    let (_, _) = run(&mut view, &layout, Command::ActualSize);
    assert!(serve(&mut view, &layout, PAGE));
    let centre = layout.canvas().center();
    let mut region = damage::sink();
    let mut feed = |view: &mut View, event: InputEvent| {
        view.on_pointer(&event, &layout, Scale::ONE, theme, &mut region)
    };
    feed(&mut view, InputEvent::PointerMoved { to: centre });
    feed(
        &mut view,
        InputEvent::PointerPressed {
            button: PointerButton::Primary,
        },
    );
    let outcome = feed(
        &mut view,
        InputEvent::PointerMoved {
            to: Point {
                x: centre.x - 40,
                y: centre.y - 30,
            },
        },
    );
    assert!(outcome.changed, "the drag panned");
    // Dragging the picture leftward moves the view rightward, which is what
    // grabbing a picture and pulling it means.
    assert_eq!(view.viewport().pan(), (40, 30));
    feed(
        &mut view,
        InputEvent::PointerReleased {
            button: PointerButton::Primary,
        },
    );
    // A move after the release is not a pan: the drag has ended.
    let outcome = feed(&mut view, InputEvent::PointerMoved { to: centre });
    assert!(!outcome.changed);
    assert_eq!(view.viewport().pan(), (40, 30));
}

#[test]
fn a_secondary_press_on_the_canvas_asks_for_the_context_menu() {
    let (mut view, layout, registry) = drawn(still(400, 300));
    let theme = registry.active();
    let centre = layout.canvas().center();
    let mut region = damage::sink();
    view.on_pointer(
        &InputEvent::PointerMoved { to: centre },
        &layout,
        Scale::ONE,
        theme,
        &mut region,
    );
    let outcome = view.on_pointer(
        &InputEvent::PointerPressed {
            button: PointerButton::Secondary,
        },
        &layout,
        Scale::ONE,
        theme,
        &mut region,
    );
    assert_eq!(outcome.menu, Some(centre));
    assert!(
        !outcome.changed,
        "the plate is the session's, not the app's"
    );
}

#[test]
fn the_wheel_over_the_canvas_pans_and_elsewhere_does_not() {
    const PAGE: (u32, u32) = (4_000, 3_000);
    let (mut view, layout, registry) = drawn(still(4_000, 3_000));
    let theme = registry.active();
    let (_, _) = run(&mut view, &layout, Command::ActualSize);
    assert!(serve(&mut view, &layout, PAGE));
    let mut region = damage::sink();
    view.on_pointer(
        &InputEvent::PointerMoved {
            to: layout.canvas().center(),
        },
        &layout,
        Scale::ONE,
        theme,
        &mut region,
    );
    let outcome = view.on_pointer(
        &InputEvent::PointerScrolled { dx: 0, dy: 1 },
        &layout,
        Scale::ONE,
        theme,
        &mut region,
    );
    assert!(outcome.changed);
    let panned = view.viewport().pan();
    assert_ne!(panned, (0, 0));

    // The wheel over the status line is not the canvas's.
    view.on_pointer(
        &InputEvent::PointerMoved {
            to: layout.status().center(),
        },
        &layout,
        Scale::ONE,
        theme,
        &mut region,
    );
    let outcome = view.on_pointer(
        &InputEvent::PointerScrolled { dx: 0, dy: 1 },
        &layout,
        Scale::ONE,
        theme,
        &mut region,
    );
    assert!(!outcome.changed);
    assert_eq!(view.viewport().pan(), panned);
}

#[test]
fn the_keyboard_reaches_the_commands_the_tools_do() {
    let (mut view, layout, _registry) = drawn(still(4_000, 3_000));
    let mut region = damage::sink();
    let mut press =
        |view: &mut View, key: Key| view.on_key(key, Modifiers::default(), &layout, &mut region);
    press(&mut view, Key::Char('1'));
    assert_eq!(view.viewport().zoom(), ZOOM_ACTUAL_PER_MILLE);
    press(&mut view, Key::Char('0'));
    assert_eq!(view.viewport().fit, Fit::Window);
    press(&mut view, Key::Char(']'));
    assert_eq!(view.viewport().reorient, Reorient::QuarterTurnRight);
    press(&mut view, Key::Char('m'));
    assert_eq!(
        view.viewport().reorient,
        Reorient::QuarterTurnRight.then(Reorient::FlipHorizontal)
    );
    assert!(press(&mut view, Key::Char('i')).changed);
    assert!(view.info_open());
    assert!(press(&mut view, Key::Char('o')).pick, "asks for the picker");
    assert!(
        press(&mut view, Key::Named(NamedKey::Escape)).close,
        "escape closes the window"
    );
}

#[test]
fn the_arrow_keys_pan_and_shifted_they_change_page() {
    const PAGE: (u32, u32) = (4_000, 3_000);
    let (mut view, layout, _registry) = drawn(pages(3, 4_000, 3_000));
    let (_, _) = run(&mut view, &layout, Command::ActualSize);
    assert!(serve(&mut view, &layout, PAGE));
    let mut region = damage::sink();
    let outcome = view.on_key(
        Key::Named(NamedKey::Right),
        Modifiers::default(),
        &layout,
        &mut region,
    );
    assert!(outcome.changed);
    assert_ne!(view.viewport().pan().0, 0, "an arrow key panned");
    assert_eq!(
        view.document().expect("open").index(),
        0,
        "and stayed on the page"
    );

    let shift = Modifiers {
        shift: true,
        ..Modifiers::default()
    };
    let outcome = view.on_key(Key::Named(NamedKey::Right), shift, &layout, &mut region);
    assert!(outcome.changed);
    assert!(serve(&mut view, &layout, PAGE));
    assert_eq!(
        view.document().expect("open").index(),
        1,
        "shifted, it turned the page"
    );
}

#[test]
fn an_unbound_key_changes_nothing() {
    let (mut view, layout, _registry) = drawn(still(400, 300));
    let mut region = damage::sink();
    let outcome = view.on_key(Key::Char('q'), Modifiers::default(), &layout, &mut region);
    assert!(!outcome.changed);
    assert!(!outcome.close);
    assert!(!outcome.pick);
    assert!(region.is_empty());
}

// ---- the layout and the viewport stay in step -------------------------

#[test]
fn a_resize_refits_a_fitted_zoom_and_reclamps_the_pan() {
    const PAGE: (u32, u32) = (4_000, 3_000);
    let (mut view, _layout, registry) = drawn(still(4_000, 3_000));
    let theme = registry.active();
    let fitted = view.viewport().zoom();
    let bigger = view.layout(1_920, 1_200, theme, Scale::ONE, font(theme, Scale::ONE));
    assert!(
        view.viewport().zoom() > fitted,
        "a wider window fits the picture larger"
    );
    assert!(!bigger.canvas().is_empty());

    let (_, _) = run(&mut view, &bigger, Command::ActualSize);
    assert!(serve(&mut view, &bigger, PAGE));
    let (_, _) = run(&mut view, &bigger, Command::Pan { dx: 8, dy: 8 });
    let panned = view.viewport().pan();
    assert_ne!(panned, (0, 0));
    let _ = view.layout(3_840, 2_400, theme, Scale::ONE, font(theme, Scale::ONE));
    assert!(
        view.viewport().pan().0 <= panned.0 && view.viewport().pan().1 <= panned.1,
        "a larger canvas can reach less far, so the pan came back inside it"
    );
}

#[test]
fn a_window_with_no_canvas_asks_for_no_render() {
    let (mut view, _layout, registry) = drawn(still(400, 300));
    let theme = registry.active();
    let _ = view.layout(0, 0, theme, Scale::ONE, font(theme, Scale::ONE));
    assert_eq!(
        view.next_request(),
        None,
        "there is no rectangle to draw into"
    );
}

#[test]
fn the_scrollbars_report_the_pan_they_are_a_view_of() {
    const PAGE: (u32, u32) = (4_000, 3_000);
    let (mut view, layout, _registry) = drawn(still(4_000, 3_000));
    let (_, _) = run(&mut view, &layout, Command::ActualSize);
    assert!(serve(&mut view, &layout, PAGE));
    let (_, _) = run(&mut view, &layout, Command::Pan { dx: 2, dy: 2 });
    let (vertical, horizontal) = view.bars();
    let pan = view.viewport().pan();
    assert_eq!(vertical.model().offset(), u64::from(pan.1));
    assert_eq!(horizontal.model().offset(), u64::from(pan.0));
}

#[test]
fn a_picture_that_fits_gives_its_bars_nothing_to_scroll() {
    let (view, _layout, _registry) = drawn(still(100, 100));
    let (vertical, horizontal) = view.bars();
    assert!(!vertical.model().range().is_scrollable());
    assert!(!horizontal.model().range().is_scrollable());
}

#[test]
fn a_page_whose_own_size_differs_from_the_declared_canvas_is_refitted_to_itself() {
    // A page container's declared geometry is its *largest* page, so the
    // first render of a smaller page is asked for against the wrong figure.
    // The engine adopts the entry, refits to it, and asks again rather than
    // drawing a rectangle that describes the container instead of the page.
    let (mut view, layout, _registry) = opened(pages(2, 4_000, 3_000));
    let Some(Request::Show {
        page,
        extent,
        window,
        mut pixels,
    }) = view.next_request()
    else {
        panic!("a render was asked for");
    };
    pixels.resize((window.width * window.height * 4) as usize, 0x20);
    let mut region = damage::sink();
    let delivered = view.deliver(
        Answer::Shown {
            page,
            extent,
            window,
            decoded: Some(ViewPage {
                index: page,
                width: 400,
                height: 300,
                delay_ns: 0,
            }),
            pixels,
            outcome: Ok(()),
        },
        &layout,
        &mut region,
    );
    assert!(!delivered.changed, "those pixels describe the wrong extent");
    assert_eq!(
        view.document().expect("open").natural(),
        (400, 300),
        "the entry's own size is what the viewer now shows"
    );
    assert!(
        matches!(view.next_request(), Some(Request::Show { .. })),
        "and the render is asked for again, against the page itself"
    );
}

#[test]
fn an_answer_about_an_entry_the_user_has_left_is_recorded_but_not_drawn() {
    let (mut view, layout, _registry) = drawn(pages(3, 400, 300));
    let Some(Request::Show { .. }) = ({
        let (outcome, _) = run(&mut view, &layout, Command::NextPage);
        assert!(outcome.changed);
        view.next_request()
    }) else {
        panic!("a render was asked for");
    };
    // While that render is in flight the user turns the page again, so the
    // answer is about an entry nobody is looking at any more.
    let (outcome, _) = run(&mut view, &layout, Command::NextPage);
    assert!(outcome.changed);
    let mut region = damage::sink();
    let delivered = view.deliver(
        Answer::Shown {
            page: 1,
            extent: (400, 300),
            window: Rect::new(0, 0, 400, 300),
            decoded: Some(ViewPage {
                index: 1,
                width: 400,
                height: 300,
                delay_ns: 0,
            }),
            pixels: vec![0u8; 400 * 300 * 4],
            outcome: Ok(()),
        },
        &layout,
        &mut region,
    );
    assert!(!delivered.changed, "not the entry being shown");
    assert_eq!(view.document().expect("open").index(), 2);
    assert!(
        !view.document().expect("open").decoded(),
        "the worker holds a different entry from the one on screen"
    );
}
