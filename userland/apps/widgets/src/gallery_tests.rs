//! Host tests for the widget-gallery model: tab identity, panel population,
//! render-without-panic on every tab, keyboard tab switching, a selector
//! reaction, and radio-group single selection.

use tairix_controls::SelectionState;
use tairix_font::BitmapFont;
use tairix_geometry::{Point, Rect, Scale};
use tairix_input::{InputEvent, Key, Modifiers, NamedKey, PointerButton};
use tairix_raster::Surface;
use tairix_theme::ThemeRegistry;

use crate::gallery::{Gallery, GalleryTab};
use crate::widget::DemoWidget;

/// A 14px font, as the `Run` binary resolves at the theme's UI size.
fn font() -> BitmapFont {
    BitmapFont::monospace(14)
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
    for event in &seq {
        gallery.on_pointer(event, viewport, Scale::ONE, theme);
    }
}

/// Drive the tab strip from the keyboard until `target` is selected.
///
/// `Home` puts the focus cursor at the first tab regardless of where it was,
/// then `Right` walks it to the target and `Enter` selects it — deterministic
/// no matter the gallery's prior state (the tab strip holds keyboard focus
/// after any selection).
fn select_tab(mut gallery: Gallery, target: GalleryTab) -> Gallery {
    gallery.on_key(Key::Named(NamedKey::Home), Modifiers::default());
    for _ in 0..target.index() {
        gallery.on_key(Key::Named(NamedKey::Right), Modifiers::default());
    }
    gallery.on_key(Key::Named(NamedKey::Enter), Modifiers::default());
    gallery
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
    let viewport = Rect::new(0, 0, 820, 620);
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
    gallery.on_key(Key::Named(NamedKey::Tab), Modifiers::default());
    let changed = gallery.on_key(Key::Char(' '), Modifiers::default());

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
    for _ in 0..6 {
        gallery.on_key(Key::Named(NamedKey::Tab), Modifiers::default());
    }
    let changed = gallery.on_key(Key::Char(' '), Modifiers::default());

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
    let viewport = Rect::new(0, 0, 820, 620);
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
