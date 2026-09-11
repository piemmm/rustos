//! Host tests for the viewer's painting.
//!
//! What these assert is that a paint draws *only* from state and that the
//! pictures and text it produces say what the viewer holds — not that a
//! particular pixel has a particular colour, which a theme change would
//! rightly move.

use alloc::string::String;
use alloc::vec;

use tairix_controls::damage;
use tairix_font::BitmapFont;
use tairix_geometry::{Rect, Scale};
use tairix_icon::NoArtwork;
use tairix_raster::{Color, Reorient, Surface};
use tairix_sandbox::imagerender::{ViewDocument, ViewFailure, ViewFormat, ViewPage, ViewRefusal};
use tairix_theme::{TextRole, Theme, ThemeRegistry};

use super::{bytes, facts, percent, render_into, summary};
use crate::view::View;
use crate::{Answer, Layout, Request};

/// The window every test paints.
const WINDOW: (u32, u32) = (800, 600);

/// The face the tests resolve their layout in.
fn font(theme: &Theme) -> BitmapFont {
    BitmapFont::for_role(theme.fonts(), TextRole::Body, Scale::ONE)
}

/// A JPEG still of `width`x`height` pixels.
fn still(width: u32, height: u32) -> ViewDocument {
    ViewDocument {
        format: ViewFormat::Jpeg,
        animated: false,
        loop_count: None,
        count: 1,
        width,
        height,
    }
}

/// A viewer with `document` open and its first window drawn, plus the layout
/// and the theme registry that produced it.
fn drawn(document: ViewDocument, name: &str, bytes: u64) -> (View, Layout, ThemeRegistry) {
    let registry = ThemeRegistry::with_builtins();
    let theme = registry.active();
    let page = (document.width, document.height);
    let mut view = View::new(true);
    assert_eq!(view.next_request(), Some(Request::Open));
    let layout = view.layout(WINDOW.0, WINDOW.1, theme, Scale::ONE, font(theme));
    let mut region = damage::sink();
    view.deliver(
        Answer::Opened {
            opened: Ok((document, String::from(name), bytes)),
        },
        &layout,
        &mut region,
    );
    let layout = view.layout(WINDOW.0, WINDOW.1, theme, Scale::ONE, font(theme));
    if let Some(Request::Show {
        page: index,
        extent,
        window,
        mut pixels,
    }) = view.next_request()
    {
        pixels.resize((window.width * window.height * 4) as usize, 0x80);
        view.deliver(
            Answer::Shown {
                page: index,
                extent,
                window,
                decoded: Some(ViewPage {
                    index,
                    width: page.0,
                    height: page.1,
                    delay_ns: 0,
                }),
                pixels,
                outcome: Ok(()),
            },
            &layout,
            &mut region,
        );
    }
    (view, layout, registry)
}

/// Paint `view` into a fresh window-sized surface.
fn painted(view: &View, layout: &Layout, theme: &Theme) -> Surface {
    let mut surface = Surface::new(WINDOW.0, WINDOW.1).expect("allocates");
    render_into(
        &mut surface,
        view,
        layout,
        theme,
        Scale::ONE,
        font(theme),
        &mut NoArtwork,
    );
    surface
}

#[test]
fn painting_is_a_function_of_state_alone() {
    // The property every interactive surface owes: two paints of the same
    // state produce the same pixels, so nothing was read, timed, or counted
    // on the way through.
    let (view, layout, registry) = drawn(still(400, 300), "photo.jpg", 2_400_000);
    let theme = registry.active();
    let first = painted(&view, &layout, theme);
    let second = painted(&view, &layout, theme);
    assert_eq!(first.pixels(), second.pixels());
}

#[test]
fn a_viewer_with_nothing_open_draws_its_reason_rather_than_a_blank_canvas() {
    let registry = ThemeRegistry::with_builtins();
    let theme = registry.active();
    let mut view = View::new(false);
    let layout = view.layout(WINDOW.0, WINDOW.1, theme, Scale::ONE, font(theme));
    let waiting = painted(&view, &layout, theme);
    assert!(view.cancelled());
    let refused = painted(&view, &layout, theme);
    assert_ne!(
        waiting.pixels(),
        refused.pixels(),
        "the stated reason is drawn, so the canvas is not blank"
    );
}

#[test]
fn every_paint_leaves_the_toolbar_and_status_line_drawn() {
    // Whatever else is going on, the chrome is there: a viewer with no
    // picture is still a window a user can act in.
    let registry = ThemeRegistry::with_builtins();
    let theme = registry.active();
    let mut view = View::new(false);
    let layout = view.layout(WINDOW.0, WINDOW.1, theme, Scale::ONE, font(theme));
    let surface = painted(&view, &layout, theme);
    let background = Surface::filled(
        WINDOW.0,
        WINDOW.1,
        tairix_raster::Color::from(theme.palette().surface).premultiply(),
    )
    .expect("allocates");
    let differs = |band: Rect| {
        let (Ok(x), Ok(y)) = (u32::try_from(band.left()), u32::try_from(band.top())) else {
            return false;
        };
        (y..y + band.height).any(|row| {
            (x..x + band.width)
                .any(|column| surface.get(column, row) != background.get(column, row))
        })
    };
    assert!(differs(layout.tools()), "the tools are drawn");
    assert!(differs(layout.zoom_slider()), "the zoom slider is drawn");
    assert!(differs(layout.status()), "the status line is drawn");
}

#[test]
fn the_chrome_strips_stand_off_the_body() {
    // The canvas must read as content rather than as more window, which is
    // what the two strips being a different colour from the body is for.
    let (view, layout, registry) = drawn(still(400, 300), "photo.jpg", 2_400_000);
    let theme = registry.active();
    let surface = painted(&view, &layout, theme);
    let body = Color::from(theme.palette().surface).premultiply();
    let at = |rect: Rect| {
        let (Ok(x), Ok(y)) = (u32::try_from(rect.left()), u32::try_from(rect.top())) else {
            return None;
        };
        surface.get(x, y)
    };
    assert_ne!(
        at(layout.toolbar()),
        Some(body),
        "the toolbar strip is chrome"
    );
    assert_ne!(
        at(layout.status()),
        Some(body),
        "the status strip is chrome"
    );
}

#[test]
fn a_turned_picture_is_drawn_turned() {
    let (mut view, layout, registry) = drawn(still(400, 300), "photo.jpg", 2_400_000);
    let theme = registry.active();
    let upright = painted(&view, &layout, theme);
    let mut region = damage::sink();
    assert!(
        view.run(crate::Command::RotateRight, &layout, &mut region)
            .changed
    );
    // Serve the render the turn calls for, so the canvas holds turned pixels.
    if let Some(Request::Show {
        page,
        extent,
        window,
        mut pixels,
    }) = view.next_request()
    {
        pixels.resize((window.width * window.height * 4) as usize, 0x80);
        view.deliver(
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
    }
    assert_eq!(view.viewport().reorient, Reorient::QuarterTurnRight);
    let turned = painted(&view, &layout, theme);
    assert_ne!(upright.pixels(), turned.pixels());
}

#[test]
fn the_information_panel_states_what_the_container_declared() {
    let (view, _layout, _registry) = drawn(
        ViewDocument {
            format: ViewFormat::Gif,
            animated: true,
            loop_count: Some(3),
            count: 12,
            width: 320,
            height: 240,
        },
        "spin.gif",
        65_536,
    );
    let stated: alloc::vec::Vec<(String, String)> = facts(&view)
        .iter()
        .map(|fact| (String::from(fact.label()), String::from(fact.value())))
        .collect();
    let says =
        |label: &str, value: &str| stated.iter().any(|(l, v)| l == label && v.contains(value));
    assert!(says("Name", "spin.gif"));
    assert!(says("Format", "GIF"));
    assert!(says("Pixels", "320 x 240"));
    assert!(says("Frame", "1 of 12"), "{stated:?}");
    assert!(says("Repeats", "3"));
    assert!(says("Zoom", "%"));
}

#[test]
fn an_animation_that_repeats_for_ever_says_so_rather_than_stating_a_count() {
    let (view, _layout, _registry) = drawn(
        ViewDocument {
            format: ViewFormat::Webp,
            animated: true,
            loop_count: None,
            count: 4,
            width: 64,
            height: 64,
        },
        "loop.webp",
        1_024,
    );
    assert!(
        facts(&view)
            .iter()
            .any(|fact| fact.label() == "Repeats" && fact.value() == "for ever"),
        "a file that declares no count is not reported as declaring one"
    );
}

#[test]
fn a_still_picture_states_no_frame_count_and_no_repeat() {
    let (view, _layout, _registry) = drawn(still(400, 300), "photo.jpg", 2_400_000);
    let labels: alloc::vec::Vec<String> = facts(&view)
        .iter()
        .map(|fact| String::from(fact.label()))
        .collect();
    assert!(!labels.iter().any(|label| label == "Frame"));
    assert!(!labels.iter().any(|label| label == "Page"));
    assert!(!labels.iter().any(|label| label == "Repeats"));
}

#[test]
fn a_viewer_with_nothing_open_states_that_rather_than_inventing_facts() {
    let view = View::new(false);
    let stated = facts(&view);
    assert_eq!(stated.len(), 1);
    assert_eq!(stated[0].label(), "Document");
}

#[test]
fn the_summary_names_the_document_and_the_refusal_names_the_reason() {
    let (view, _layout, _registry) = drawn(still(4_032, 3_024), "IMG_0001.JPG", 2_400_000);
    let line = summary(&view);
    assert!(line.contains("IMG_0001.JPG"));
    assert!(line.contains("JPEG"));
    assert!(line.contains("4032 x 3024"));

    let mut empty = View::new(false);
    assert!(empty.cancelled());
    let stated = summary(&empty);
    assert!(!stated.is_empty(), "a refusal always states something");
}

#[test]
fn a_refused_open_is_summarised_as_the_reason_it_gave() {
    let registry = ThemeRegistry::with_builtins();
    let theme = registry.active();
    let mut view = View::new(true);
    let layout = view.layout(WINDOW.0, WINDOW.1, theme, Scale::ONE, font(theme));
    let mut region = damage::sink();
    view.deliver(
        Answer::Opened {
            opened: Err(crate::Refusal::Failed(ViewFailure::Refused(
                ViewRefusal::TooLarge,
            ))),
        },
        &layout,
        &mut region,
    );
    let stated = summary(&view);
    assert!(
        stated.contains("larger"),
        "the user is told the picture is large, not that it is broken: {stated}"
    );
}

#[test]
fn every_view_format_has_a_name() {
    // `ViewFormat` is deliberately exhaustive, so this is a compile-time
    // obligation; the test pins that each name is distinct and non-empty.
    let formats = [
        ViewFormat::Png,
        ViewFormat::Jpeg,
        ViewFormat::Gif,
        ViewFormat::Bmp,
        ViewFormat::Ico,
        ViewFormat::Sprite,
        ViewFormat::Tiff,
        ViewFormat::Webp,
        ViewFormat::Svg,
    ];
    let mut seen = vec![];
    for format in formats {
        let (view, _layout, _registry) = drawn(
            ViewDocument {
                format,
                animated: false,
                loop_count: None,
                count: 1,
                width: 8,
                height: 8,
            },
            "f",
            1,
        );
        let shown = facts(&view)
            .iter()
            .find(|fact| fact.label() == "Format")
            .map(|fact| String::from(fact.value()))
            .expect("a format is always stated");
        assert!(!shown.is_empty(), "{format:?}");
        assert!(!seen.contains(&shown), "{format:?} shares a name");
        seen.push(shown);
    }
}

#[test]
fn a_byte_count_reads_in_the_units_a_user_expects() {
    assert_eq!(bytes(0), "0 B");
    assert_eq!(bytes(1_023), "1023 B");
    assert_eq!(bytes(1_024), "1.0 KiB");
    assert_eq!(bytes(1_536), "1.5 KiB");
    assert_eq!(bytes(1_048_576), "1.0 MiB");
    assert_eq!(bytes(2_400_000), "2.2 MiB");
    assert_eq!(bytes(1_073_741_824), "1.0 GiB");
    // The largest count a file could have still reads, rather than wrapping.
    assert!(bytes(u64::MAX).ends_with("TiB"), "{}", bytes(u64::MAX));
}

#[test]
fn a_zoom_reads_as_a_percentage_and_keeps_a_tenth_when_it_has_one() {
    assert_eq!(percent(1_000), "100%");
    assert_eq!(percent(10), "1%");
    assert_eq!(percent(64_000), "6400%");
    assert_eq!(percent(1_005), "100.5%");
}

#[test]
fn a_window_with_no_canvas_still_paints_without_complaint() {
    let registry = ThemeRegistry::with_builtins();
    let theme = registry.active();
    let mut view = View::new(false);
    for (width, height) in [(0, 0), (1, 1), (40, 12)] {
        let layout = view.layout(width, height, theme, Scale::ONE, font(theme));
        let mut surface = Surface::new(width, height).expect("allocates");
        render_into(
            &mut surface,
            &view,
            &layout,
            theme,
            Scale::ONE,
            font(theme),
            &mut NoArtwork,
        );
    }
}
