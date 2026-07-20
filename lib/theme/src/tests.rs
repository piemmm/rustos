//! Unit tests for the theme definition and registry.

use alloc::string::String;

use crate::motion::MotionInteraction;
use crate::{
    Appearance, Contrast, CursorKind, CursorSet, Density, FontSpec, FontWeight, Fonts, Metrics,
    MotionTheme, Palette, Rgba, SignalRole, Theme, ThemeError, ThemeId, ThemeRegistry,
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
    assert_ne!(d.on_accent, l.on_accent);
    assert_ne!(d.border, l.border);
    // The Reactive Alloy control roles and semantic signals also differ per
    // appearance, so a theme switch retunes them consistently.
    assert_ne!(d.surface_pressed, l.surface_pressed);
    assert_ne!(d.rim, l.rim);
    assert_ne!(d.rim_active, l.rim_active);
    assert_ne!(d.danger, l.danger);
    assert_ne!(d.scroll_track, l.scroll_track);
    assert_ne!(d.scroll_thumb, l.scroll_thumb);
    assert_ne!(d.frame_active, l.frame_active);
    assert_ne!(d.frame_inactive, l.frame_inactive);
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
            frame_active: Rgba::rgb(80, 140, 255),
            frame_inactive: Rgba::rgb(60, 60, 60),
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
            title_bar_height: 24,
            frame_inset: 1,
            window_control_extent: 18,
            resize_grabber_extent: 14,
            hit_slop: 3,
        },
        Fonts {
            ui: FontSpec::new("Test Sans", 12, FontWeight::Regular),
            monospace: FontSpec::new("Test Mono", 12, FontWeight::Bold),
        },
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
