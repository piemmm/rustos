//! Unit tests for the theme definition and registry.

use alloc::string::String;

use crate::motion::MotionInteraction;
use crate::{
    Appearance, Contrast, CursorKind, CursorSet, Density, FamilyKey, FontWeight, Fonts, Metrics,
    MotionTheme, Palette, Rgba, SignalRole, TextRole, Theme, ThemeError, ThemeId, ThemeRegistry,
};

#[test]
fn rgba_constructors_and_accessors() {
    assert_eq!(Rgba::rgb(1, 2, 3), Rgba::new(1, 2, 3, 255));
    assert!(Rgba::rgb(1, 2, 3).is_opaque());
    assert!(!Rgba::TRANSPARENT.is_opaque());
    assert_eq!(Rgba::rgb(1, 2, 3).with_alpha(0).a, 0);
    assert_eq!(Rgba::new(9, 8, 7, 6).to_array(), [9, 8, 7, 6]);
}

#[test]
fn builtins_have_their_reserved_ids_and_appearance() {
    let dark = Theme::dark();
    let light = Theme::light();
    assert_eq!(dark.id(), ThemeId::DARK);
    assert_eq!(light.id(), ThemeId::LIGHT);
    assert_eq!(dark.appearance(), Appearance::Dark);
    assert_eq!(light.appearance(), Appearance::Light);
    assert_ne!(dark.name(), light.name());
}

#[test]
fn dark_and_light_palettes_differ_on_every_role() {
    let d = *Theme::dark().palette();
    let l = *Theme::light().palette();
    // A theme switch must visibly change every colour role, otherwise the
    // switch would not apply consistently across the desktop.
    assert_ne!(d.desktop, l.desktop);
    assert_ne!(d.surface, l.surface);
    assert_ne!(d.surface_raised, l.surface_raised);
    assert_ne!(d.on_surface, l.on_surface);
    assert_ne!(d.on_surface_muted, l.on_surface_muted);
    assert_ne!(d.accent, l.accent);
    assert_ne!(d.border, l.border);
    // The Reactive Alloy control roles and semantic signals also differ per
    // appearance, so a theme switch retunes them consistently.
    assert_ne!(d.surface_hover, l.surface_hover);
    assert_ne!(d.surface_pressed, l.surface_pressed);
    assert_ne!(d.rim, l.rim);
    assert_ne!(d.rim_active, l.rim_active);
    assert_ne!(d.danger, l.danger);
    assert_ne!(d.scroll_track, l.scroll_track);
    assert_ne!(d.scroll_thumb, l.scroll_thumb);
    assert_ne!(d.frame, l.frame);
    for role in [
        SignalRole::Cpu,
        SignalRole::Memory,
        SignalRole::Disk,
        SignalRole::Network,
        SignalRole::Power,
        SignalRole::Thermal,
        SignalRole::Recovery,
        SignalRole::Success,
        SignalRole::Warning,
        SignalRole::Denied,
    ] {
        assert_ne!(
            d.signal(role),
            l.signal(role),
            "signal role {role:?} differs"
        );
    }
}

#[test]
fn accent_labels_stay_legible_on_the_accent_fill() {
    // A primary action is one treatment on both appearances - a warm white
    // label on the alloy-orange plate - so `on_accent` is deliberately shared
    // and the invariant worth asserting is legibility, not difference.
    assert_eq!(
        Theme::dark().palette().on_accent,
        Theme::light().palette().on_accent
    );
    for theme in [Theme::dark(), Theme::light()] {
        let p = theme.palette();
        assert!(
            luma(p.on_accent).abs_diff(luma(p.accent)) >= 96,
            "{}: accent label contrast too low",
            theme.name()
        );
        assert!(
            luma(p.on_surface).abs_diff(luma(p.surface)) >= 96,
            "{}: body text contrast too low",
            theme.name()
        );
        assert!(
            luma(p.on_surface_muted).abs_diff(luma(p.surface)) >= 48,
            "{}: muted text contrast too low",
            theme.name()
        );
    }
}

/// Rec. 601 luma, the cheap perceptual brightness the contrast checks compare.
fn luma(c: Rgba) -> u32 {
    (u32::from(c.r) * 299 + u32::from(c.g) * 587 + u32::from(c.b) * 114) / 1000
}

#[test]
fn pointer_plates_step_away_from_the_bar_fill_in_the_appearance_direction() {
    // A control seated in the taskbar wears no perimeter, so its plate is the
    // *only* thing that can report hover or press: a hover that resolved to
    // the bar's own fill would leave such an icon with no feedback at all.
    // Both plates must therefore separate from `surface_raised` (the bar) and
    // from each other, and the hover must move in the direction the
    // appearance calls for — brighter on a dark theme, deeper on a light one.
    // The light band is the tighter of the two, so the floor is the smallest
    // step that still reads rather than a flattering number.
    const MIN_STEP: u32 = 4;
    for theme in [Theme::dark(), Theme::light()] {
        let p = theme.palette();
        let (bar, hover, pressed) = (
            luma(p.surface_raised),
            luma(p.surface_hover),
            luma(p.surface_pressed),
        );
        assert!(
            hover.abs_diff(bar) >= MIN_STEP,
            "{}: hover plate too close to the bar fill",
            theme.name()
        );
        assert!(
            hover.abs_diff(pressed) >= MIN_STEP,
            "{}: hover plate too close to the pressed plate",
            theme.name()
        );
        assert!(
            pressed < bar,
            "{}: a press must read as compression, never as lift",
            theme.name()
        );
        match theme.appearance() {
            Appearance::Dark => assert!(
                hover > bar,
                "{}: a dark hover lifts off the bar",
                theme.name()
            ),
            Appearance::Light => assert!(
                hover < bar,
                "{}: a light hover deepens into the bar",
                theme.name()
            ),
        }
    }
}

#[test]
fn signal_resolves_each_semantic_role_to_its_field() {
    let p = *Theme::dark().palette();
    assert_eq!(p.signal(SignalRole::Cpu), p.cpu_pressure);
    assert_eq!(p.signal(SignalRole::Memory), p.memory_pressure);
    assert_eq!(p.signal(SignalRole::Disk), p.disk_pressure);
    assert_eq!(p.signal(SignalRole::Network), p.network_activity);
    assert_eq!(p.signal(SignalRole::Power), p.power_pressure);
    assert_eq!(p.signal(SignalRole::Thermal), p.thermal_pressure);
    assert_eq!(p.signal(SignalRole::Recovery), p.recovery);
    assert_eq!(p.signal(SignalRole::Success), p.success);
    assert_eq!(p.signal(SignalRole::Warning), p.warning);
    assert_eq!(p.signal(SignalRole::Denied), p.denied);
}

#[test]
fn builtins_share_motion_and_default_density_contrast() {
    assert_eq!(Theme::dark().motion(), Theme::light().motion());
    assert_eq!(Theme::dark().density(), Density::Normal);
    assert_eq!(Theme::dark().contrast(), Contrast::Normal);
    // Tuned durations sit within the spec §9 bands.
    let m = Theme::dark().motion();
    assert_eq!(m.duration(MotionInteraction::HoverEnter), 100);
    assert!(!m.reduced_motion());
}

#[test]
fn reduced_motion_collapses_every_duration_to_zero() {
    let reduced = Theme::dark().motion().with_reduced_motion(true);
    assert!(reduced.reduced_motion());
    for interaction in [
        MotionInteraction::HoverEnter,
        MotionInteraction::HoverExit,
        MotionInteraction::PressCompress,
        MotionInteraction::ReleaseSettle,
        MotionInteraction::PanelOpen,
        MotionInteraction::MenuOpen,
        MotionInteraction::JobProgressPulse,
        MotionInteraction::RecoveryLatchReveal,
        MotionInteraction::WindowActivate,
        MotionInteraction::WindowSizeTransition,
        MotionInteraction::ScrollbarWake,
    ] {
        assert_eq!(reduced.duration(interaction), 0);
    }
}

#[test]
fn builtin_surfaces_are_opaque_and_distinct() {
    for theme in [Theme::dark(), Theme::light()] {
        let p = theme.palette();
        assert!(p.desktop.is_opaque());
        assert!(p.surface.is_opaque());
        // The raised surface (taskbar/menus) must read as distinct from
        // the base surface in both themes.
        assert_ne!(p.surface, p.surface_raised);
    }
}

#[test]
fn builtins_share_metrics_fonts_and_cursors() {
    // Corner radii, fonts, and cursors are appearance-independent house
    // style, shared by both built-ins rather than restated.
    assert_eq!(Theme::dark().metrics(), Theme::light().metrics());
    assert_eq!(Theme::dark().fonts(), Theme::light().fonts());
    assert_eq!(Theme::dark().cursors(), Theme::light().cursors());
}

#[test]
fn instrument_lines_stay_thinner_than_the_row_that_carries_them() {
    let dark = Theme::dark();
    let m = dark.metrics();

    // A slider's groove is the thinnest track: the thumb marks the value, so
    // the line only has to be visible.
    assert!(m.measured_thickness < m.progress_thickness);
    // A chart is a box, not an instrument line: it must have room a track does
    // not, or a trend cannot rise far enough to be read.
    assert!(m.progress_thickness < m.chart_height);
    // A progress bar has no thumb, so it reads a little broader — but it stays
    // an instrument line, nowhere near a plate.
    assert!(m.progress_thickness < m.selector_extent);
    assert!(m.selector_extent < m.control_height);
    // Every track survives the thinnest sensible rounding.
    assert!(m.measured_thickness >= 1);
}

/// The key a test names a family by.
fn key(name: &str) -> FamilyKey {
    FamilyKey::new(name).expect("a well-formed family key")
}

#[test]
fn the_ladder_derives_every_role_from_one_base_size() {
    let fonts = Fonts::ladder(key("board-sans"), key("board-mono"), 18);

    assert_eq!(fonts.base_size_px(), 18);
    // Body is the base by definition, so a theme authors one number.
    assert_eq!(fonts.spec(TextRole::Body).size_px, 18);
    // The boards' ladder is tight but strictly ordered around the base.
    assert!(fonts.spec(TextRole::Display).size_px > fonts.spec(TextRole::Heading).size_px);
    assert!(fonts.spec(TextRole::Heading).size_px > fonts.spec(TextRole::ItemTitle).size_px);
    assert!(fonts.spec(TextRole::ItemTitle).size_px > fonts.spec(TextRole::Body).size_px);
    assert!(fonts.spec(TextRole::Body).size_px > fonts.spec(TextRole::Caption).size_px);
    assert!(fonts.spec(TextRole::Caption).size_px > fonts.spec(TextRole::SectionHeader).size_px);
    // The display rung breaks out of the cluster: a screen-filling readout is
    // dominant, not merely one step up from a panel heading.
    assert!(fonts.spec(TextRole::Display).size_px >= fonts.spec(TextRole::Body).size_px * 2);
    // Every rung is legible: no role rounds away to nothing.
    for role in TextRole::ALL {
        assert!(fonts.spec(role).size_px >= 1, "{role:?} rounded to zero");
    }
}

#[test]
fn the_ladder_carries_the_boards_weights_and_families() {
    let fonts = Fonts::ladder(key("board-sans"), key("board-mono"), 18);

    // On the boards the hierarchy is carried mostly by weight: titling text
    // is medium, column headers and metric readouts are bold, and running
    // text stays regular.
    assert_eq!(fonts.spec(TextRole::Heading).weight, FontWeight::Medium);
    assert_eq!(fonts.spec(TextRole::ItemTitle).weight, FontWeight::Medium);
    // The display rung is the exception: at that size a medium weight reads
    // heavy, so it states its hierarchy on size alone.
    assert_eq!(fonts.spec(TextRole::Display).weight, FontWeight::Regular);
    assert_eq!(fonts.spec(TextRole::WindowTitle).weight, FontWeight::Medium);
    assert_eq!(fonts.spec(TextRole::SectionHeader).weight, FontWeight::Bold);
    assert_eq!(fonts.spec(TextRole::Metric).weight, FontWeight::Bold);
    assert_eq!(fonts.spec(TextRole::Body).weight, FontWeight::Regular);
    assert_eq!(fonts.spec(TextRole::Caption).weight, FontWeight::Regular);
    assert_eq!(fonts.spec(TextRole::Monospace).weight, FontWeight::Regular);

    // Only the fixed-width role leaves the UI family.
    assert_eq!(fonts.ui_family(), key("board-sans"));
    assert_eq!(fonts.monospace_family(), key("board-mono"));
    assert_eq!(fonts.spec(TextRole::Monospace).family, key("board-mono"));
    for role in TextRole::ALL {
        if role != TextRole::Monospace {
            assert_eq!(fonts.spec(role).family, key("board-sans"), "{role:?}");
        }
    }
}

#[test]
fn a_chosen_ui_family_replaces_every_role_but_the_fixed_width_one() {
    let chosen = key("noto-serif");
    let dark = Theme::dark();
    let shipped = *dark.fonts();
    let fonts = shipped.with_ui_family(chosen);

    assert_eq!(fonts.ui_family(), chosen);
    assert_eq!(fonts.monospace_family(), shipped.monospace_family());
    assert_eq!(fonts.base_size_px(), shipped.base_size_px());
    for role in TextRole::ALL {
        let expected = if role == TextRole::Monospace {
            shipped.monospace_family()
        } else {
            chosen
        };
        assert_eq!(fonts.spec(role).family, expected, "{role:?}");
        // Choosing a family retunes nothing else about the ladder.
        assert_eq!(fonts.spec(role).size_px, shipped.spec(role).size_px);
        assert_eq!(fonts.spec(role).weight, shipped.spec(role).weight);
    }
}

#[test]
fn the_shipped_themes_name_families_the_store_installs() {
    let dark = Theme::dark();
    let fonts = dark.fonts();
    // The spellings must survive the key grammar, or the desktop would fall
    // back to the fixed-pitch family for its interface text.
    assert_eq!(fonts.ui_family().as_str(), "inter");
    assert_eq!(fonts.monospace_family().as_str(), "mono");
    let light = Theme::light();
    assert_eq!(light.fonts(), fonts);
}

#[test]
fn an_out_of_range_base_size_is_clamped_rather_than_accepted() {
    assert_eq!(
        Fonts::ladder(key("s"), key("m"), 0).base_size_px(),
        Fonts::MIN_BASE_SIZE_PX
    );
    assert_eq!(
        Fonts::ladder(key("s"), key("m"), u16::MAX).base_size_px(),
        Fonts::MAX_BASE_SIZE_PX
    );
}

#[test]
fn the_tallest_rung_survives_the_largest_base_at_a_high_dpi_scale() {
    // The bound on the base size exists so the ladder's tallest rung still
    // rasterises whole at a doubled density instead of being clamped short.
    let fonts = Fonts::ladder(key("s"), key("m"), Fonts::MAX_BASE_SIZE_PX);
    let tallest = u32::from(fonts.spec(TextRole::Display).size_px);
    assert!(
        tallest * 2 <= 512,
        "tallest rung {tallest} outgrows the ceiling"
    );
}

#[test]
fn cursor_set_resolves_every_kind() {
    let dark = Theme::dark();
    let cursors = dark.cursors();
    for kind in [
        CursorKind::Arrow,
        CursorKind::Text,
        CursorKind::Pointer,
        CursorKind::Move,
        CursorKind::Busy,
    ] {
        assert!(!cursors.asset(kind).is_empty());
    }
    assert_eq!(cursors.asset(CursorKind::Arrow), "cursor.arrow");
    assert_eq!(cursors.asset(CursorKind::Move), "cursor.move");
}

#[test]
fn registry_defaults_to_dark_and_holds_both_builtins() {
    let themes = ThemeRegistry::with_builtins();
    assert_eq!(themes.active_id(), ThemeId::DARK);
    assert_eq!(themes.active().appearance(), Appearance::Dark);
    assert_eq!(themes.len(), 2);
    assert!(!themes.is_empty());
    assert!(themes.get(ThemeId::DARK).is_some());
    assert!(themes.get(ThemeId::LIGHT).is_some());
    assert!(themes.get(ThemeId(999)).is_none());
}

#[test]
fn runtime_switch_changes_the_active_theme() {
    let mut themes = ThemeRegistry::with_builtins();
    assert_eq!(
        themes.active().palette().desktop,
        Theme::dark().palette().desktop
    );

    themes
        .set_active(ThemeId::LIGHT)
        .expect("light is built in");
    assert_eq!(themes.active_id(), ThemeId::LIGHT);
    assert_eq!(
        themes.active().palette().desktop,
        Theme::light().palette().desktop
    );

    themes.set_active(ThemeId::DARK).expect("dark is built in");
    assert_eq!(themes.active().appearance(), Appearance::Dark);
}

#[test]
fn set_active_unknown_fails_closed() {
    let mut themes = ThemeRegistry::with_builtins();
    let err = themes.set_active(ThemeId(42)).unwrap_err();
    assert_eq!(err, ThemeError::UnknownTheme(ThemeId(42)));
    // The active theme is unchanged by the rejected switch.
    assert_eq!(themes.active_id(), ThemeId::DARK);
}

#[test]
fn register_custom_theme_then_activate_it() {
    let mut themes = ThemeRegistry::with_builtins();
    let custom = sample_theme(ThemeId(100));
    themes.register(custom.clone()).expect("fresh id");
    assert_eq!(themes.len(), 3);
    assert_eq!(themes.get(ThemeId(100)), Some(&custom));

    themes.set_active(ThemeId(100)).expect("registered");
    assert_eq!(themes.active().name(), "Test");
    // Custom themes follow the built-ins in iteration order.
    let ids: alloc::vec::Vec<ThemeId> = themes.themes().map(Theme::id).collect();
    assert_eq!(ids, [ThemeId::DARK, ThemeId::LIGHT, ThemeId(100)]);
}

#[test]
fn register_duplicate_id_fails_closed() {
    let mut themes = ThemeRegistry::with_builtins();

    // A built-in id is already taken.
    let dup_builtin = themes.register(sample_theme(ThemeId::DARK)).unwrap_err();
    assert_eq!(dup_builtin, ThemeError::DuplicateId(ThemeId::DARK));

    // A custom id cannot be reused either.
    themes.register(sample_theme(ThemeId(7))).expect("fresh id");
    let dup_custom = themes.register(sample_theme(ThemeId(7))).unwrap_err();
    assert_eq!(dup_custom, ThemeError::DuplicateId(ThemeId(7)));
    assert_eq!(themes.len(), 3);
}

#[test]
fn default_registry_matches_with_builtins() {
    assert_eq!(ThemeRegistry::default(), ThemeRegistry::with_builtins());
}

#[test]
fn set_appearance_selects_the_matching_builtin() {
    let mut themes = ThemeRegistry::with_builtins();

    assert_eq!(themes.set_appearance(Appearance::Light), ThemeId::LIGHT);
    assert_eq!(themes.active_id(), ThemeId::LIGHT);
    assert_eq!(themes.active().appearance(), Appearance::Light);

    assert_eq!(themes.set_appearance(Appearance::Dark), ThemeId::DARK);
    assert_eq!(themes.active_id(), ThemeId::DARK);
    assert_eq!(themes.active().appearance(), Appearance::Dark);
}

#[test]
fn toggle_appearance_flips_between_builtins() {
    let mut themes = ThemeRegistry::with_builtins();
    assert_eq!(themes.active().appearance(), Appearance::Dark);

    assert_eq!(themes.toggle_appearance(), ThemeId::LIGHT);
    assert_eq!(themes.active().appearance(), Appearance::Light);

    assert_eq!(themes.toggle_appearance(), ThemeId::DARK);
    assert_eq!(themes.active().appearance(), Appearance::Dark);
}

#[test]
fn toggle_appearance_from_a_custom_theme_lands_on_the_opposite_builtin() {
    let mut themes = ThemeRegistry::with_builtins();
    // A custom dark-appearance theme becomes the active one.
    themes
        .register(sample_theme(ThemeId(100)))
        .expect("fresh id");
    themes.set_active(ThemeId(100)).expect("registered");
    assert_eq!(themes.active().appearance(), Appearance::Dark);

    // Toggling from a (custom) dark theme lands on the light built-in.
    assert_eq!(themes.toggle_appearance(), ThemeId::LIGHT);
    assert_eq!(themes.active().appearance(), Appearance::Light);
}

fn sample_theme(id: ThemeId) -> Theme {
    Theme::new(
        id,
        "Test",
        Appearance::Dark,
        Palette {
            desktop: Rgba::rgb(0, 0, 0),
            surface: Rgba::rgb(10, 10, 10),
            surface_raised: Rgba::rgb(20, 20, 20),
            on_surface: Rgba::rgb(240, 240, 240),
            on_surface_muted: Rgba::rgb(160, 160, 160),
            accent: Rgba::rgb(80, 140, 255),
            on_accent: Rgba::rgb(0, 0, 0),
            border: Rgba::rgb(60, 60, 60),
            surface_hover: Rgba::rgb(30, 30, 30),
            surface_pressed: Rgba::rgb(5, 5, 5),
            rim: Rgba::rgb(70, 70, 70),
            rim_active: Rgba::rgb(120, 170, 255),
            danger: Rgba::rgb(255, 90, 90),
            cpu_pressure: Rgba::rgb(240, 160, 48),
            memory_pressure: Rgba::rgb(176, 108, 240),
            disk_pressure: Rgba::rgb(48, 192, 176),
            network_activity: Rgba::rgb(64, 176, 255),
            power_pressure: Rgba::rgb(139, 212, 80),
            thermal_pressure: Rgba::rgb(255, 122, 60),
            recovery: Rgba::rgb(255, 106, 176),
            success: Rgba::rgb(76, 208, 122),
            warning: Rgba::rgb(245, 197, 66),
            denied: Rgba::rgb(200, 90, 90),
            scroll_track: Rgba::rgb(35, 40, 48),
            scroll_thumb: Rgba::rgb(74, 81, 92),
            frame: Rgba::rgb(60, 60, 60),
        },
        Metrics {
            window_corner_radius: 4,
            taskbar_corner_radius: 4,
            popup_corner_radius: 4,
            border_thickness: 1,
            scrollbar_breadth: 12,
            min_thumb_length: 20,
            control_height: 24,
            control_inset: 8,
            control_gap: 6,
            control_corner_radius: 4,
            seam_thickness: 2,
            rail_thickness: 2,
            bead_size: 6,
            measured_thickness: 4,
            progress_thickness: 6,
            chart_height: 40,
            selector_extent: 14,
            toggle_track_length: 24,
            title_bar_height: 24,
            frame_inset: 1,
            window_control_extent: 18,
            resize_grabber_extent: 14,
            hit_slop: 3,
        },
        Fonts::ladder(key("test-sans"), key("test-mono"), 15),
        CursorSet {
            arrow: String::from("c.arrow"),
            text: String::from("c.text"),
            pointer: String::from("c.pointer"),
            move_: String::from("c.move"),
            busy: String::from("c.busy"),
        },
        MotionTheme::new(90, 80, 60, 90, 180, 120, 120, 180, 90, 160, 70),
        Density::Normal,
        Contrast::Normal,
    )
}
