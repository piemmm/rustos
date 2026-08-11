//! Host tests for the widget-gallery model: tab identity, panel population,
//! render-without-panic on every tab, keyboard tab switching, a selector
//! reaction, radio-group single selection, and the damage reports the `Run`
//! binary presents by.

use tairix_controls::{damage, SelectionState};
use tairix_font::BitmapFont;
use tairix_geometry::{Point, Rect, Region, Scale};
use tairix_input::{InputEvent, Key, Modifiers, NamedKey, PointerButton};
use tairix_raster::Surface;
use tairix_theme::{Theme, ThemeRegistry};

use crate::gallery::{Gallery, GalleryTab};
use crate::widget::DemoWidget;

/// A 14px font, as the `Run` binary resolves at the theme's UI size.
fn font() -> BitmapFont {
    BitmapFont::monospace(14)
}

/// The gallery window the `Run` binary creates, as the viewport every layout
/// derives from.
fn window() -> Rect {
    Rect::new(0, 0, 820, 620)
}

/// A press-then-release click sequence at `point`, each preceded by the move
/// that positions the pointer (as a real device reports it).
fn click(gallery: &mut Gallery, point: Point, viewport: Rect, themes: &ThemeRegistry) {
    let theme = themes.active();
    let seq = [
        InputEvent::PointerMoved { to: point },
        InputEvent::PointerPressed {
            button: PointerButton::Primary,
        },
        InputEvent::PointerReleased {
            button: PointerButton::Primary,
        },
    ];
    let mut damage = damage::sink();
    for event in &seq {
        gallery.on_pointer(event, viewport, Scale::ONE, theme, &mut damage);
    }
}

/// Drive the tab strip from the keyboard until `target` is selected.
///
/// `Home` puts the focus cursor at the first tab regardless of where it was,
/// then `Right` walks it to the target and `Enter` selects it — deterministic
/// no matter the gallery's prior state (the tab strip holds keyboard focus
/// after any selection).
fn select_tab(mut gallery: Gallery, target: GalleryTab) -> Gallery {
    let themes = ThemeRegistry::with_builtins();
    press(&mut gallery, Key::Named(NamedKey::Home), &themes);
    for _ in 0..target.index() {
        press(&mut gallery, Key::Named(NamedKey::Right), &themes);
    }
    press(&mut gallery, Key::Named(NamedKey::Enter), &themes);
    gallery
}

/// One unmodified key press, laid out at the gallery's window geometry,
/// reporting whether the view changed.
fn press(gallery: &mut Gallery, key: Key, themes: &ThemeRegistry) -> bool {
    gallery.on_key(
        key,
        Modifiers::default(),
        window(),
        Scale::ONE,
        themes.active(),
        &mut damage::sink(),
    )
}

#[test]
fn tab_identity_round_trips() {
    assert_eq!(GalleryTab::ALL.len(), 9);
    for (i, tab) in GalleryTab::ALL.iter().enumerate() {
        assert_eq!(tab.index(), i);
        assert_eq!(GalleryTab::from_index(i), Some(*tab));
        assert!(!tab.title().is_empty());
    }
    assert_eq!(GalleryTab::from_index(9), None);
}

#[test]
fn every_panel_is_populated() {
    let gallery = Gallery::new();
    assert_eq!(gallery.current_tab(), GalleryTab::Buttons);
    for tab in GalleryTab::ALL {
        assert!(
            !crate::panels::build(tab).is_empty(),
            "panel {tab:?} has no demo widgets"
        );
    }
}

#[test]
fn renders_every_tab_without_panic() {
    let themes = ThemeRegistry::with_builtins();
    let theme = themes.active();
    let viewport = window();
    let mut gallery = Gallery::new();
    for tab in GalleryTab::ALL {
        gallery = select_tab(gallery, tab);
        let mut surface = Surface::new(viewport.width, viewport.height).expect("surface");
        gallery.render(&mut surface, viewport, Scale::ONE, theme, font());
        assert_eq!(gallery.current_tab(), tab);
    }
}

#[test]
fn keyboard_switches_tabs() {
    let gallery = select_tab(Gallery::new(), GalleryTab::Selectors);
    assert_eq!(gallery.current_tab(), GalleryTab::Selectors);
}

#[test]
fn focused_toggle_flips_on_space() {
    let mut gallery = select_tab(Gallery::new(), GalleryTab::Selectors);

    // The first Selectors item is the "Wi-Fi" toggle, which starts on.
    assert!(toggle_on(&gallery, 0));

    // Tab once to focus that first item, then Space actuates it.
    let themes = ThemeRegistry::with_builtins();
    press(&mut gallery, Key::Named(NamedKey::Tab), &themes);
    let changed = press(&mut gallery, Key::Char(' '), &themes);

    assert!(changed);
    assert!(
        !toggle_on(&gallery, 0),
        "Space should have flipped the toggle off"
    );
}

#[test]
fn radio_group_keeps_single_selection() {
    let mut gallery = select_tab(Gallery::new(), GalleryTab::Selectors);

    // The Selectors panel ends with two radios: index 5 (off) and 6 (on).
    assert!(!radio_selected(&gallery, 5));
    assert!(radio_selected(&gallery, 6));

    // Focus the first radio (item 5): Tab moves Tabs -> item0 .. -> item5.
    let themes = ThemeRegistry::with_builtins();
    for _ in 0..6 {
        press(&mut gallery, Key::Named(NamedKey::Tab), &themes);
    }
    let changed = press(&mut gallery, Key::Char(' '), &themes);

    assert!(changed);
    assert!(
        radio_selected(&gallery, 5),
        "the actuated radio should be selected"
    );
    assert!(
        !radio_selected(&gallery, 6),
        "the sibling radio should have been cleared"
    );
}

#[test]
fn pointer_click_selects_a_checkbox() {
    let themes = ThemeRegistry::with_builtins();
    let viewport = window();
    let mut gallery = select_tab(Gallery::new(), GalleryTab::Selectors);

    // The fourth Selectors item is the "Accept terms" checkbox (checked).
    assert_eq!(checkbox_selection(&gallery, 3), SelectionState::Selected);

    // Click the centre of that checkbox's actual on-screen rectangle.
    let rect = gallery
        .widget_rect_for_test(3, viewport, Scale::ONE, themes.active())
        .expect("checkbox rect");
    let centre = Point::new(
        rect.left() + i32::try_from(rect.width / 2).unwrap_or(0),
        rect.top() + i32::try_from(rect.height / 2).unwrap_or(0),
    );
    click(&mut gallery, centre, viewport, &themes);

    assert_eq!(
        checkbox_selection(&gallery, 3),
        SelectionState::Unselected,
        "clicking the checked checkbox should clear it"
    );
}

fn toggle_on(gallery: &Gallery, index: usize) -> bool {
    match &gallery.current_panel()[index].widget {
        DemoWidget::Toggle(t) => t.is_on(),
        other => panic!("item {index} is not a toggle: {other:?}"),
    }
}

fn radio_selected(gallery: &Gallery, index: usize) -> bool {
    match &gallery.current_panel()[index].widget {
        DemoWidget::Radio(r) => r.is_selected(),
        other => panic!("item {index} is not a radio: {other:?}"),
    }
}

fn checkbox_selection(gallery: &Gallery, index: usize) -> SelectionState {
    match &gallery.current_panel()[index].widget {
        DemoWidget::Checkbox(c) => c.selection(),
        other => panic!("item {index} is not a checkbox: {other:?}"),
    }
}

/// Drives the gallery while proving that every pixel a round changes lies
/// inside what that round reported.
///
/// This is the invariant the `Run` binary's narrowed present rests on: it
/// converts and declares only the reported rectangle, so a pixel that changed
/// outside it is one the session never copies — a stale pixel left on screen.
/// Reporting *more* than changed is safe and passes here; reporting less does
/// not.
struct Prover {
    gallery: Gallery,
    theme: Theme,
    /// The frame as it stands after every round proven so far.
    shown: Surface,
}

impl Prover {
    fn new() -> Self {
        let gallery = Gallery::new();
        let theme = ThemeRegistry::with_builtins().active().clone();
        let shown = painted(&gallery, &theme);
        Self {
            gallery,
            theme,
            shown,
        }
    }

    /// Run one round through `act`, then assert its report covered it.
    fn prove(&mut self, what: &str, act: impl FnOnce(&mut Gallery, &Theme, &mut Region)) {
        let Self {
            gallery,
            theme,
            shown,
        } = self;
        let mut reported = damage::sink();
        act(gallery, theme, &mut reported);
        let after = painted(gallery, theme);
        let width = i32::try_from(after.width()).expect("window width fits an i32");
        for (i, (was, now)) in shown.pixels().iter().zip(after.pixels()).enumerate() {
            if was == now {
                continue;
            }
            let at = i32::try_from(i).expect("pixel index fits an i32");
            let point = Point::new(at % width, at / width);
            assert!(
                reported.contains(point),
                "{what}: ({}, {}) changed but was not reported; reported {:?}",
                point.x,
                point.y,
                reported.rects()
            );
        }
        *shown = after;
    }

    /// Move the pointer to `point`, then press and release there, proving each.
    fn prove_click(&mut self, what: &str, point: Point) {
        for (step, event) in [
            ("move", InputEvent::PointerMoved { to: point }),
            (
                "press",
                InputEvent::PointerPressed {
                    button: PointerButton::Primary,
                },
            ),
            (
                "release",
                InputEvent::PointerReleased {
                    button: PointerButton::Primary,
                },
            ),
        ] {
            self.prove(
                &alloc::format!("{what} {step}"),
                |gallery, theme, damage| {
                    gallery.on_pointer(&event, window(), Scale::ONE, theme, damage);
                },
            );
        }
    }

    /// Press `key`, proving the round.
    fn prove_key(&mut self, what: &str, key: Key) {
        self.prove(what, |gallery, theme, damage| {
            gallery.on_key(
                key,
                Modifiers::default(),
                window(),
                Scale::ONE,
                theme,
                damage,
            );
        });
    }
}

/// The gallery rendered whole at the window geometry the `Run` binary uses.
fn painted(gallery: &Gallery, theme: &Theme) -> Surface {
    let viewport = window();
    let mut surface = Surface::new(viewport.width, viewport.height).expect("surface");
    gallery.render(&mut surface, viewport, Scale::ONE, theme, font());
    surface
}

/// The centre of demo item `index`'s on-screen rectangle.
fn item_centre(gallery: &Gallery, theme: &Theme, index: usize) -> Option<Point> {
    let rect = gallery.widget_rect_for_test(index, window(), Scale::ONE, theme)?;
    Some(Point::new(
        rect.left() + i32::try_from(rect.width / 2).unwrap_or(0),
        rect.top() + i32::try_from(rect.height / 2).unwrap_or(0),
    ))
}

/// The centre of tab `index`'s cell in the strip.
fn tab_centre(index: usize) -> Point {
    let viewport = window();
    let span = viewport.width / u32::try_from(GalleryTab::ALL.len()).expect("tab count fits");
    let x = i32::try_from(span).unwrap_or(0) * i32::try_from(index).unwrap_or(0)
        + i32::try_from(span / 2).unwrap_or(0);
    Point::new(x, viewport.top() + 8)
}

#[test]
fn every_round_reports_every_pixel_it_changes() {
    let mut prover = Prover::new();
    for tab in GalleryTab::ALL {
        prover.prove_click(&alloc::format!("{tab:?} tab"), tab_centre(tab.index()));
        assert_eq!(prover.gallery.current_tab(), tab);

        // Hover, press and release each widget in turn: enter/leave marks, the
        // press look, and the value the owner writes back on release.
        let items = prover.gallery.current_panel().len();
        for index in 0..items {
            let Some(centre) = item_centre(&prover.gallery, &prover.theme, index) else {
                continue;
            };
            prover.prove_click(&alloc::format!("{tab:?} item {index}"), centre);
        }

        // Then walk the whole focus ring and actuate each stop from the
        // keyboard, which is the path that moves the ring between two widgets.
        for step in 0..=items {
            prover.prove_key(
                &alloc::format!("{tab:?} focus step {step}"),
                Key::Named(NamedKey::Tab),
            );
            prover.prove_key(&alloc::format!("{tab:?} actuate {step}"), Key::Char(' '));
        }
    }
}

#[test]
fn a_hover_reports_the_widget_it_entered_and_nothing_else() {
    let themes = ThemeRegistry::with_builtins();
    let theme = themes.active();
    let mut gallery = select_tab(Gallery::new(), GalleryTab::Buttons);
    let centre = item_centre(&gallery, theme, 0).expect("first item is laid out");
    let rect = gallery
        .widget_rect_for_test(0, window(), Scale::ONE, theme)
        .expect("first item is laid out");

    let mut reported = damage::sink();
    gallery.on_pointer(
        &InputEvent::PointerMoved { to: centre },
        window(),
        Scale::ONE,
        theme,
        &mut reported,
    );
    assert_eq!(reported.rects(), &[rect], "the hovered widget, exactly");

    // A second sample inside the same widget changes nothing and costs nothing.
    let mut again = damage::sink();
    gallery.on_pointer(
        &InputEvent::PointerMoved {
            to: Point::new(centre.x + 1, centre.y),
        },
        window(),
        Scale::ONE,
        theme,
        &mut again,
    );
    assert!(
        again.is_empty(),
        "a sample inside one widget reports nothing"
    );
}

#[test]
fn a_tab_switch_reports_the_content_it_redraws() {
    let themes = ThemeRegistry::with_builtins();
    let theme = themes.active();
    let mut gallery = Gallery::new();

    // The keyboard cursor starts off the strip, so the first Right lands it on
    // the first tab and the second moves it to the next one.
    for _ in 0..2 {
        press(&mut gallery, Key::Named(NamedKey::Right), &themes);
    }
    let mut reported = damage::sink();
    gallery.on_key(
        Key::Named(NamedKey::Enter),
        Modifiers::default(),
        window(),
        Scale::ONE,
        theme,
        &mut reported,
    );
    assert_ne!(
        gallery.current_tab(),
        GalleryTab::Buttons,
        "Enter selected the tab the cursor was on"
    );

    // A different panel is drawn, so the report must cover the content band and
    // not merely the two tab cells the selection moved between.
    let content = gallery
        .widget_rect_for_test(0, window(), Scale::ONE, theme)
        .expect("first item is laid out");
    assert!(
        reported.contains(Point::new(content.left(), content.top())),
        "the content band is reported: {:?}",
        reported.rects()
    );
}
