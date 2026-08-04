//! Host unit tests for the wallpaper chooser engine.

use alloc::string::String;
use alloc::vec;

use tairix_abi::input::NamedKeyCode;
use tairix_theme::ThemeRegistry;
use tairix_wallpaper::{CatalogEntry, WallpaperPath};

use super::*;

/// A settings document naming a wallpaper that is one of `catalog`'s
/// candidates, otherwise at the shared crate default.
fn settings_selecting(path: &str) -> PinboardSettings {
    PinboardSettings {
        wallpaper: WallpaperChoice::Image(WallpaperPath::new(path).expect("a valid test path")),
        ..PinboardSettings::default()
    }
}

/// Three catalog entries under the shipped store, in listing order.
fn sample_catalog() -> Vec<Candidate> {
    let entries = vec![
        CatalogEntry {
            name: String::from("alpha.png"),
            bytes: 10,
        },
        CatalogEntry {
            name: String::from("beta.jpg"),
            bytes: 20,
        },
        CatalogEntry {
            name: String::from("gamma.jpeg"),
            bytes: 30,
        },
    ];
    candidates_from_catalog(&entries)
}

/// A chooser over [`sample_catalog`], opened on its first image candidate.
fn sample_chooser() -> Chooser {
    let settings = settings_selecting("/System/Graphics/Wallpapers/alpha.png");
    Chooser::new(sample_catalog(), &settings)
}

/// Press `key` and discard the action reported, for a test whose assertion
/// is about the state the press left behind rather than what it returned.
fn press(chooser: &mut Chooser, key: NamedKeyCode) {
    let _ = chooser.handle_key(key, false);
}

/// Move focus to `region` with Tab presses, bounded by one full cycle so a
/// tab order that never reaches it fails rather than looping.
fn focus_on(chooser: &mut Chooser, region: Focus) {
    for _ in 0..Focus::ORDER.len() {
        if chooser.focus() == region {
            return;
        }
        press(chooser, NamedKeyCode::Tab);
    }
    assert_eq!(chooser.focus(), region, "Tab never reached {region:?}");
}

// --- Candidate list model ------------------------------------------------

#[test]
fn the_no_wallpaper_entry_is_always_first() {
    let chooser = sample_chooser();
    assert_eq!(chooser.candidates()[0].choice, WallpaperChoice::None);
    assert_eq!(chooser.candidates()[0].label, NONE_LABEL);
    assert_eq!(chooser.candidates()[0].thumbnail, Thumbnail::Backdrop);
}

#[test]
fn the_chooser_opens_on_the_settings_current_wallpaper() {
    let chooser = sample_chooser();
    // "alpha.png" is the first catalog entry, so index 1 (after "no
    // wallpaper").
    assert_eq!(chooser.selected(), 1);
    assert_eq!(chooser.candidates()[1].label, "alpha.png");
}

#[test]
fn no_wallpaper_settings_open_the_chooser_on_the_none_entry() {
    let settings = PinboardSettings {
        wallpaper: WallpaperChoice::None,
        ..PinboardSettings::default()
    };
    let chooser = Chooser::new(sample_catalog(), &settings);
    assert_eq!(chooser.selected(), 0);
}

#[test]
fn a_current_wallpaper_outside_the_catalog_is_appended_and_selected() {
    let settings = settings_selecting("/Users/ada/Pictures/mine.png");
    let chooser = Chooser::new(sample_catalog(), &settings);
    // "no wallpaper" + 3 catalog entries + the synthetic current entry.
    assert_eq!(chooser.candidates().len(), 5);
    assert_eq!(chooser.selected(), 4);
    assert_eq!(chooser.candidates()[4].label, "mine.png");
    assert_eq!(chooser.candidates()[4].thumbnail, Thumbnail::Pending);
}

#[test]
fn a_refused_thumbnail_is_remembered_and_never_retried() {
    let mut chooser = sample_chooser();
    assert_eq!(chooser.next_pending(), Some(1));
    chooser.mark_thumbnail_refused(1);
    assert_eq!(chooser.candidates()[1].thumbnail, Thumbnail::Refused);
    // The refused candidate is no longer offered as pending work.
    assert_eq!(chooser.next_pending(), Some(2));
}

#[test]
fn a_ready_thumbnail_replaces_the_pending_state() {
    let mut chooser = sample_chooser();
    let surface = Surface::new(THUMB_WIDTH, THUMB_HEIGHT).expect("a small surface allocates");
    chooser.set_thumbnail(1, surface);
    assert!(matches!(
        chooser.candidates()[1].thumbnail,
        Thumbnail::Ready(_)
    ));
    assert_eq!(chooser.next_pending(), Some(2));
}

#[test]
fn next_pending_is_none_once_every_candidate_is_resolved() {
    let mut chooser = sample_chooser();
    for index in 0..chooser.candidates().len() {
        chooser.mark_thumbnail_refused(index);
    }
    assert_eq!(chooser.next_pending(), None);
}

#[test]
fn candidate_path_is_none_for_the_no_wallpaper_entry() {
    let chooser = sample_chooser();
    assert_eq!(chooser.candidate_path(0), None);
    assert!(chooser.candidate_path(1).is_some());
}

// --- Selection and focus movement ----------------------------------------

#[test]
fn tab_cycles_focus_through_every_region_and_wraps() {
    let mut chooser = sample_chooser();
    let order = [
        Focus::Fit,
        Focus::Backdrop,
        Focus::Icons,
        Focus::Sort,
        Focus::Apply,
        Focus::Close,
        Focus::Grid,
    ];
    for expected in order {
        assert_eq!(
            chooser.handle_key(NamedKeyCode::Tab, false),
            ChooserAction::Changed
        );
        assert_eq!(chooser.focus(), expected);
    }
}

#[test]
fn shift_tab_cycles_focus_backward_and_wraps() {
    let mut chooser = sample_chooser();
    assert_eq!(chooser.focus(), Focus::Grid);
    assert_eq!(
        chooser.handle_key(NamedKeyCode::Tab, true),
        ChooserAction::Changed
    );
    assert_eq!(chooser.focus(), Focus::Close);
    assert_eq!(
        chooser.handle_key(NamedKeyCode::Tab, true),
        ChooserAction::Changed
    );
    assert_eq!(chooser.focus(), Focus::Apply);
}

#[test]
fn enter_activates_the_focused_action_and_escape_always_closes() {
    let mut chooser = sample_chooser();
    // From the grid, an option row, or the Apply button, Enter applies.
    for region in [Focus::Grid, Focus::Fit, Focus::Backdrop, Focus::Sort] {
        focus_on(&mut chooser, region);
        assert_eq!(
            chooser.handle_key(NamedKeyCode::Enter, false),
            ChooserAction::Apply
        );
    }
    focus_on(&mut chooser, Focus::Apply);
    assert_eq!(
        chooser.handle_key(NamedKeyCode::Enter, false),
        ChooserAction::Apply
    );
    // A focused Close button closes: a button carrying the focus ring must
    // do what it says rather than the primary action.
    focus_on(&mut chooser, Focus::Close);
    assert_eq!(
        chooser.handle_key(NamedKeyCode::Enter, false),
        ChooserAction::Close
    );
    // Escape closes from every region.
    for region in [Focus::Grid, Focus::Icons, Focus::Apply, Focus::Close] {
        focus_on(&mut chooser, region);
        assert_eq!(
            chooser.handle_key(NamedKeyCode::Escape, false),
            ChooserAction::Close
        );
    }
}

#[test]
fn arrow_keys_move_the_grid_selection_and_stop_at_the_edges() {
    let mut chooser = sample_chooser();
    chooser.relayout(2000, 2000); // one row, every candidate visible
    assert_eq!(chooser.selected(), 1);
    assert_eq!(
        chooser.handle_key(NamedKeyCode::Right, false),
        ChooserAction::Changed
    );
    assert_eq!(chooser.selected(), 2);
    assert_eq!(
        chooser.handle_key(NamedKeyCode::Left, false),
        ChooserAction::Changed
    );
    assert_eq!(chooser.selected(), 1);
    assert_eq!(
        chooser.handle_key(NamedKeyCode::Left, false),
        ChooserAction::Changed
    );
    assert_eq!(chooser.selected(), 0);
    // Already at the first candidate: Left is a no-op.
    assert_eq!(
        chooser.handle_key(NamedKeyCode::Left, false),
        ChooserAction::None
    );
}

#[test]
fn down_from_an_incomplete_last_row_lands_on_the_last_candidate() {
    let mut chooser = sample_chooser();
    // Lay the grid out exactly 3 columns wide: 4 candidates ("no
    // wallpaper" plus 3 catalog entries) over 3 columns is row 0 =
    // [0,1,2], row 1 = [3] alone.
    let columns = 3u32;
    let grid_width = CELL_WIDTH * columns;
    chooser.relayout(grid_width + MARGIN * 2, 2000);
    assert_eq!(chooser.columns, usize::try_from(columns).unwrap());
    // From index 1, Down has no full cell below it but a shorter next row
    // exists, so it lands on the last candidate rather than doing nothing.
    chooser.selected = 1;
    assert_eq!(
        chooser.handle_key(NamedKeyCode::Down, false),
        ChooserAction::Changed
    );
    assert_eq!(chooser.selected(), 3);
    // Already on the last row: Down is now a no-op.
    assert_eq!(
        chooser.handle_key(NamedKeyCode::Down, false),
        ChooserAction::None
    );
}

#[test]
fn up_from_the_top_row_is_a_no_op() {
    let mut chooser = sample_chooser();
    chooser.relayout(2000, 2000);
    chooser.selected = 0;
    assert_eq!(
        chooser.handle_key(NamedKeyCode::Up, false),
        ChooserAction::None
    );
}

#[test]
fn a_non_arrow_non_tab_key_is_a_no_op() {
    let mut chooser = sample_chooser();
    assert_eq!(
        chooser.handle_key(NamedKeyCode::F1, false),
        ChooserAction::None
    );
}

// --- Option groups ---------------------------------------------------------

#[test]
fn fit_cycles_through_every_value_and_wraps_both_ways() {
    let mut chooser = sample_chooser();
    focus_on(&mut chooser, Focus::Fit);
    assert_eq!(chooser.fit(), WallpaperFit::Fill);
    for expected in [
        WallpaperFit::Fit,
        WallpaperFit::Stretch,
        WallpaperFit::Centre,
        WallpaperFit::Tile,
        WallpaperFit::Fill,
    ] {
        press(&mut chooser, NamedKeyCode::Right);
        assert_eq!(chooser.fit(), expected);
    }
    press(&mut chooser, NamedKeyCode::Left);
    assert_eq!(chooser.fit(), WallpaperFit::Tile);
}

#[test]
fn icon_flow_cycles_between_its_two_values() {
    let mut chooser = sample_chooser();
    focus_on(&mut chooser, Focus::Icons);
    assert_eq!(chooser.icons(), IconFlow::Leading);
    press(&mut chooser, NamedKeyCode::Down);
    assert_eq!(chooser.icons(), IconFlow::Trailing);
    press(&mut chooser, NamedKeyCode::Up);
    assert_eq!(chooser.icons(), IconFlow::Leading);
}

#[test]
fn sort_cycles_through_every_value() {
    let mut chooser = sample_chooser();
    focus_on(&mut chooser, Focus::Sort);
    assert_eq!(chooser.sort(), IconSort::Name);
    press(&mut chooser, NamedKeyCode::Right);
    assert_eq!(chooser.sort(), IconSort::Kind);
    press(&mut chooser, NamedKeyCode::Right);
    assert_eq!(chooser.sort(), IconSort::Size);
    press(&mut chooser, NamedKeyCode::Right);
    assert_eq!(chooser.sort(), IconSort::Date);
    press(&mut chooser, NamedKeyCode::Right);
    assert_eq!(chooser.sort(), IconSort::Name);
}

#[test]
fn arrows_over_apply_or_close_change_nothing() {
    let mut chooser = sample_chooser();
    focus_on(&mut chooser, Focus::Apply);
    assert_eq!(
        chooser.handle_key(NamedKeyCode::Right, false),
        ChooserAction::None
    );
    focus_on(&mut chooser, Focus::Close);
    assert_eq!(
        chooser.handle_key(NamedKeyCode::Left, false),
        ChooserAction::None
    );
}

#[test]
fn the_backdrop_row_offers_the_whole_palette_and_cycles_both_ways() {
    let mut chooser = sample_chooser();
    assert_eq!(chooser.backdrops().len(), BACKDROP_PALETTE.len());
    for (offered, (label, backdrop)) in chooser.backdrops().iter().zip(BACKDROP_PALETTE) {
        assert_eq!(offered.label, label);
        assert_eq!(offered.backdrop, backdrop);
    }

    focus_on(&mut chooser, Focus::Backdrop);
    assert_eq!(chooser.backdrop(), Backdrop::Theme);
    for (_, expected) in BACKDROP_PALETTE.iter().skip(1) {
        press(&mut chooser, NamedKeyCode::Right);
        assert_eq!(chooser.backdrop(), *expected);
    }
    // One more step wraps back to the theme's own colour, and a backward
    // step returns to the last palette entry.
    press(&mut chooser, NamedKeyCode::Right);
    assert_eq!(chooser.backdrop(), Backdrop::Theme);
    press(&mut chooser, NamedKeyCode::Left);
    assert_eq!(
        Some(chooser.backdrop()),
        BACKDROP_PALETTE.last().map(|(_, backdrop)| *backdrop)
    );
}

#[test]
fn a_current_colour_outside_the_palette_is_offered_and_selected() {
    let custom = Backdrop::Colour(tairix_wallpaper::Rgb::new(0x0a, 0x14, 0x1e));
    let settings = PinboardSettings {
        backdrop: custom,
        ..PinboardSettings::default()
    };
    let chooser = Chooser::new(sample_catalog(), &settings);
    assert_eq!(chooser.backdrops().len(), BACKDROP_PALETTE.len() + 1);
    assert_eq!(chooser.backdrop(), custom);
    // It is labelled by the same bare spelling the settings document uses,
    // so the user sees which colour is in effect.
    assert_eq!(
        chooser
            .backdrops()
            .last()
            .map(|option| option.label.as_str()),
        Some("0a141e")
    );
}

#[test]
fn backdrop_options_always_offer_the_current_backdrop() {
    for current in [
        Backdrop::Theme,
        Backdrop::Colour(tairix_wallpaper::Rgb::new(1, 2, 3)),
        BACKDROP_PALETTE[1].1,
    ] {
        let options = backdrop_options(current);
        assert!(options.iter().any(|option| option.backdrop == current));
    }
}

#[test]
fn changing_the_fit_re_renders_previews_but_remembers_refusals() {
    let mut chooser = sample_chooser();
    let surface = Surface::new(THUMB_WIDTH, THUMB_HEIGHT).expect("a small surface allocates");
    chooser.set_thumbnail(1, surface);
    chooser.mark_thumbnail_refused(2);

    focus_on(&mut chooser, Focus::Fit);
    press(&mut chooser, NamedKeyCode::Right);

    // The rendered preview is stale under the new fit and is asked for
    // again; the refused candidate is not.
    assert_eq!(chooser.candidates()[1].thumbnail, Thumbnail::Pending);
    assert_eq!(chooser.candidates()[2].thumbnail, Thumbnail::Refused);
    assert_eq!(chooser.candidates()[0].thumbnail, Thumbnail::Backdrop);
    assert_eq!(chooser.next_pending(), Some(1));
}

// --- Settings document -----------------------------------------------------

#[test]
fn the_rendered_document_matches_the_current_ui_state_exactly() {
    let mut chooser = sample_chooser();
    focus_on(&mut chooser, Focus::Fit);
    press(&mut chooser, NamedKeyCode::Right); // Fill -> Fit
    focus_on(&mut chooser, Focus::Backdrop);
    press(&mut chooser, NamedKeyCode::Right); // Theme -> the first colour
    focus_on(&mut chooser, Focus::Icons);
    press(&mut chooser, NamedKeyCode::Down); // Leading -> Trailing
    focus_on(&mut chooser, Focus::Sort);
    press(&mut chooser, NamedKeyCode::Right); // Name -> Kind
    let expected = PinboardSettings {
        wallpaper: WallpaperChoice::Image(
            WallpaperPath::new("/System/Graphics/Wallpapers/alpha.png").expect("a valid path"),
        ),
        fit: WallpaperFit::Fit,
        backdrop: BACKDROP_PALETTE[1].1,
        icons: IconFlow::Trailing,
        sort: IconSort::Kind,
    };
    assert_eq!(chooser.to_settings(), expected);
    assert_eq!(
        chooser.settings_document(),
        tairix_wallpaper::settings::render(&expected)
    );
}

#[test]
fn selecting_no_wallpaper_renders_a_none_document() {
    let mut chooser = sample_chooser();
    chooser.selected = 0;
    let settings = chooser.to_settings();
    assert_eq!(settings.wallpaper, WallpaperChoice::None);
    assert!(chooser.settings_document().contains("wallpaper none\n"));
}

#[test]
fn the_backdrop_colour_the_chooser_opened_with_is_carried_through_unchanged() {
    let settings = PinboardSettings {
        backdrop: Backdrop::Colour(tairix_wallpaper::Rgb::new(10, 20, 30)),
        ..PinboardSettings::default()
    };
    let chooser = Chooser::new(sample_catalog(), &settings);
    assert_eq!(chooser.to_settings().backdrop, settings.backdrop);
}

// --- Apply outcome -----------------------------------------------------

#[test]
fn apply_outcome_starts_absent_and_reports_exactly_what_was_set() {
    let mut chooser = sample_chooser();
    assert_eq!(chooser.apply_outcome(), None);
    chooser.set_apply_outcome(ApplyOutcome::Applied);
    assert_eq!(chooser.apply_outcome(), Some(&ApplyOutcome::Applied));
    chooser.set_apply_outcome(ApplyOutcome::Refused(String::from("no permission")));
    assert_eq!(
        chooser.apply_outcome(),
        Some(&ApplyOutcome::Refused(String::from("no permission")))
    );
    chooser.set_apply_outcome(ApplyOutcome::NoDesktop);
    assert_eq!(chooser.apply_outcome(), Some(&ApplyOutcome::NoDesktop));
}

#[test]
fn every_apply_outcome_renders_a_distinct_surface_from_no_outcome() {
    let themes = ThemeRegistry::with_builtins();
    let theme = themes.active();
    let mut chooser = sample_chooser();
    let before = chooser
        .render(theme, WIN_WIDTH, WIN_HEIGHT)
        .expect("renders");
    for outcome in [
        ApplyOutcome::Applied,
        ApplyOutcome::Refused(String::from("denied")),
        ApplyOutcome::NoDesktop,
    ] {
        chooser.set_apply_outcome(outcome);
        let after = chooser
            .render(theme, WIN_WIDTH, WIN_HEIGHT)
            .expect("renders");
        assert_ne!(before.pixels(), after.pixels());
    }
}

// --- Layout -----------------------------------------------------------------

#[test]
fn layout_regions_never_overlap_or_leave_the_window_at_a_small_size() {
    for (w, h) in [
        (MIN_WIN_WIDTH, MIN_WIN_HEIGHT),
        (0, 0),
        (1, 1),
        (WIN_WIDTH, WIN_HEIGHT),
        (2000, 30),
        (30, 2000),
    ] {
        let layout = Layout::compute(w, h);
        let regions = layout.regions();
        let window = Rect::new(0, 0, w, h);
        for region in regions {
            assert!(
                region.is_empty() || window.intersection(&region) == region,
                "region {region:?} escapes the {w}x{h} window"
            );
        }
        for i in 0..regions.len() {
            for j in (i + 1)..regions.len() {
                let overlap = regions[i].intersection(&regions[j]);
                assert!(
                    overlap.is_empty(),
                    "regions {:?} and {:?} overlap at {w}x{h}",
                    regions[i],
                    regions[j]
                );
            }
        }
    }
}

#[test]
fn a_chooser_renders_a_window_sized_surface_at_every_size() {
    let themes = ThemeRegistry::with_builtins();
    let theme = themes.active();
    let chooser = sample_chooser();
    for (w, h) in [(MIN_WIN_WIDTH, MIN_WIN_HEIGHT), (WIN_WIDTH, WIN_HEIGHT)] {
        let surface = chooser.render(theme, w, h).expect("renders");
        assert_eq!((surface.width(), surface.height()), (w, h));
    }
}

#[test]
fn relayout_updates_the_column_count_from_the_new_grid_width() {
    let mut chooser = sample_chooser();
    chooser.relayout(WIN_WIDTH, WIN_HEIGHT);
    let wide_columns = chooser.columns;
    chooser.relayout(MIN_WIN_WIDTH, MIN_WIN_HEIGHT);
    let narrow_columns = chooser.columns;
    assert!(narrow_columns <= wide_columns);
    assert!(narrow_columns >= 1);
}
