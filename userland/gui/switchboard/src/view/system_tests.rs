//! Unit tests for the System section: its sidebar, its header readings,
//! its eight pages, and its action rail.

use alloc::string::String;
use alloc::vec::Vec;

use tairix_controls::testkit::high_contrast;
use tairix_controls::{
    ControlDisposition, ControlRole, MeterValue, MetricInstrument, PressureKind, PressureState,
};
use tairix_geometry::{Rect, Scale};
use tairix_input::{Key, NamedKey};
use tairix_raster::Surface;
use tairix_theme::Theme;

use super::{PageLine, SystemSection};
use crate::view::system_data::{Reading, SystemPage, TileInstrument, Unmeasured};
use crate::view::test_support::{activate, bounds, font, has_ink, model, system_report};
use crate::view::{
    ActionVerdict, Section, SectionOutcome, SectionView, Switchboard, SwitchboardAction,
    SwitchboardModel, SystemAction,
};

/// A Switchboard showing the System section with the fixture's readings.
fn shown() -> Switchboard {
    let mut sb = Switchboard::new(&model());
    sb.select_section(Section::System);
    sb
}

/// The section's page lines as plain text, for a test that asks what the
/// page actually says rather than where it draws.
fn page_text(section: &SystemSection) -> Vec<String> {
    section
        .lines
        .iter()
        .map(|line| match line {
            PageLine::Heading(text) | PageLine::Absence(text) => text.clone(),
            PageLine::Fact(fact) => alloc::format!("{}: {}", fact.label(), fact.value()),
        })
        .collect()
}

/// Whether any of the section's page lines contains `needle`.
fn says(section: &SystemSection, needle: &str) -> bool {
    page_text(section).iter().any(|line| line.contains(needle))
}

/// Paint the section once, so a render test exercises the real path.
fn paint(sb: &mut Switchboard, theme: &Theme) -> Surface {
    let b = bounds();
    let mut surface = Surface::new(b.width, b.height).expect("surface");
    sb.render(&mut surface, b, Scale::ONE, theme, font());
    surface
}

// --- The section frame -------------------------------------------------

#[test]
fn overview_resource_cards_still_render_from_the_extended_model() {
    let theme = Theme::dark();
    let mut sb = shown();
    let surface = paint(&mut sb, &theme);
    let layout = sb.compute_layout(bounds(), Scale::ONE, &theme);
    // The readings that were the Overview's resource cards are now the
    // header band's four tiles, so the block that must paint is the band.
    let band = Rect::new(
        layout.content.left(),
        layout.content.top(),
        layout.content.width,
        Scale::ONE.scale_length(40),
    );
    assert!(
        has_ink(&surface, band),
        "the header reading block must still paint"
    );
}

#[test]
fn the_section_asks_for_a_sidebar_a_header_and_a_rail() {
    let anatomy = shown().system.anatomy();
    assert!(anatomy.sidebar_width > 0, "the pages need a sidebar");
    assert!(anatomy.header_height > 0, "the readings need a header");
    assert!(anatomy.rail_width > 0, "the machine's actions need a rail");
    assert_eq!(
        anatomy.detail_width, 0,
        "the section commands one subject, so it has no per-row detail"
    );
}

// --- The header readings -----------------------------------------------

#[test]
fn the_header_carries_four_readings_in_a_fixed_order() {
    let sb = shown();
    let names: Vec<&str> = sb
        .system
        .report
        .headline
        .iter()
        .map(|tile| tile.name.as_str())
        .collect();
    assert_eq!(names, alloc::vec!["CPU", "Memory", "Disk", "Network"]);
}

#[test]
fn cpu_and_network_trend_while_memory_and_disk_track() {
    let sb = shown();
    let instruments: Vec<bool> = sb
        .system
        .report
        .headline
        .iter()
        .map(|tile| matches!(tile.instrument, TileInstrument::Trend(_)))
        .collect();
    assert_eq!(
        instruments,
        alloc::vec![true, false, false, true],
        "a rate trends; a fraction of a fixed whole tracks"
    );
}

#[test]
fn a_measured_header_reading_shows_its_figure() {
    let sb = shown();
    let headline = &sb.system.report.headline;
    assert_eq!(headline[0].value, Reading::measured("62%"));
    assert_eq!(headline[1].detail, Reading::measured("8.6 GiB of 16.0 GiB"));
    assert_eq!(
        sb.system.tiles.len(),
        headline.len(),
        "every reading is built into a tile"
    );
}

#[test]
fn an_unmeasured_header_reading_names_why_rather_than_showing_a_zero() {
    let sb = shown();
    let network = &sb.system.report.headline[3];
    assert_eq!(network.value, Reading::Absent(Unmeasured::NotPermitted));
    assert_eq!(
        SystemSection::tile_text(&network.value),
        "unknown — not permitted",
        "an absent rate names its reason instead of reading as nought"
    );
}

#[test]
fn an_unmeasured_track_stays_unmeasured_rather_than_filling_to_nought() {
    assert!(matches!(
        SystemSection::tile_instrument(&TileInstrument::Track(None), PressureKind::Memory),
        MetricInstrument::Track(MeterValue::Unmeasured)
    ));
    assert!(matches!(
        SystemSection::tile_instrument(&TileInstrument::Track(Some(538)), PressureKind::Memory),
        MetricInstrument::Track(MeterValue::Measured(_))
    ));
}

#[test]
fn an_unmeasured_trend_plots_nothing_rather_than_a_flat_floor() {
    let MetricInstrument::Trend(empty) = SystemSection::tile_instrument(
        &TileInstrument::Trend(alloc::vec![]),
        PressureKind::Network,
    ) else {
        panic!("a trend instrument must carry a chart");
    };
    // A history with no readings plots no trace at all. A single fabricated
    // nought would draw a line along the floor, which reads as a measured
    // idle rather than as nothing recorded.
    assert!(empty.is_empty());
    let MetricInstrument::Trend(plotted) = SystemSection::tile_instrument(
        &TileInstrument::Trend(alloc::vec![100, 300]),
        PressureKind::Network,
    ) else {
        panic!("a trend instrument must carry a chart");
    };
    assert!(!plotted.is_empty());
}

#[test]
fn a_pressured_reading_carries_its_pressure_to_the_tile() {
    let sb = shown();
    assert!(sb.system.report.headline[0].pressured);
    assert!(!sb.system.report.headline[1].pressured);
    assert_eq!(
        SystemSection::tile_pressure(&sb.system.report.headline[0]),
        PressureState::Under(PressureKind::Cpu)
    );
    assert_eq!(
        SystemSection::tile_pressure(&sb.system.report.headline[1]),
        PressureState::None
    );
}

// --- The sidebar and its pages -----------------------------------------

#[test]
fn the_sidebar_lists_every_page_in_order() {
    let sb = shown();
    let labels: Vec<&str> = sb
        .system
        .sidebar
        .tabs()
        .iter()
        .map(tairix_controls::Tab::label)
        .collect();
    assert_eq!(
        labels,
        alloc::vec![
            "Overview",
            "Resources",
            "Storage",
            "Network",
            "Session",
            "Permissions",
            "Services",
            "Power"
        ]
    );
}

#[test]
fn the_section_opens_on_overview() {
    assert_eq!(shown().system.page, SystemPage::Overview);
}

#[test]
fn the_overview_page_states_the_machine_its_services_and_its_permissions() {
    let sb = shown();
    assert!(says(&sb.system, "Machine"));
    assert!(says(&sb.system, "Hostname: tairix"));
    assert!(says(&sb.system, "Active Services"));
    assert!(says(&sb.system, "Permissions"));
}

#[test]
fn the_resources_page_states_each_core_and_the_memory_detail() {
    let mut sb = shown();
    show(&mut sb, SystemPage::Resources);
    assert!(says(&sb.system, "Core 0"));
    assert!(says(&sb.system, "Core 1"));
    assert!(says(&sb.system, "Installed: 16.0 GiB"));
}

#[test]
fn the_resources_page_states_what_the_desktops_last_frame_cost() {
    let mut sb = shown();
    show(&mut sb, SystemPage::Resources);
    assert!(says(&sb.system, "Desktop"));
    assert!(says(
        &sb.system,
        "Last frame: 3.2k px of 2.0M px recomposed"
    ));
    assert!(
        says(&sb.system, "Blended: 42.0k px, 13.1x damaged"),
        "the blend against the damage is why a reader opens this page"
    );
}

#[test]
fn the_storage_page_states_each_volume_with_its_capacity_and_health() {
    let mut sb = shown();
    show(&mut sb, SystemPage::Storage);
    assert!(says(&sb.system, "System:"));
    assert!(says(&sb.system, "Filesystem: arxfs"));
    assert!(says(&sb.system, "Medium: solid state"));
    assert!(says(&sb.system, "60.0 GiB of 200.0 GiB used"));
    assert!(
        says(&sb.system, "Health: 3 medium errors"),
        "a measured fault must reach the page a reader opens about a failing disk"
    );
}

#[test]
fn the_network_page_states_each_interface_its_link_and_its_rates() {
    let mut sb = shown();
    show(&mut sb, SystemPage::Network);
    assert!(says(&sb.system, "eth0"));
    assert!(says(&sb.system, "Link: up"));
    assert!(says(&sb.system, "Address: 10.0.2.15/24"));
    assert!(says(&sb.system, "Receiving: 1.0 KiB/s"));
}

#[test]
fn an_interface_with_no_address_says_so_rather_than_leaving_a_gap() {
    let mut sb = shown();
    show(&mut sb, SystemPage::Network);
    assert!(
        says(&sb.system, "No address is configured on this interface."),
        "an interface with no address is a fact, not a blank"
    );
}

#[test]
fn the_session_page_states_the_seats_and_the_census() {
    let mut sb = shown();
    show(&mut sb, SystemPage::Session);
    assert!(says(&sb.system, "Seat 0"));
    assert!(says(&sb.system, "Owner: task 7"));
    assert!(says(&sb.system, "Logged in: 2"));
}

#[test]
fn the_permissions_page_states_the_authority_and_the_limits() {
    let mut sb = shown();
    show(&mut sb, SystemPage::Permissions);
    assert!(says(&sb.system, "Process control: held"));
    assert!(says(&sb.system, "Open streams"));
    assert!(says(&sb.system, "Hard bound: unlimited"));
    assert!(says(&sb.system, "In use: 9"));
}

#[test]
fn services_and_power_state_that_no_interface_exists() {
    for page in [SystemPage::Services, SystemPage::Power] {
        let mut sb = shown();
        show(&mut sb, page);
        assert!(
            says(&sb.system, "no interface"),
            "{} must state the absence rather than show an empty list",
            page.title()
        );
    }
}

// --- Honest absence ----------------------------------------------------

#[test]
fn a_denied_reading_says_not_permitted_and_an_unavailable_one_says_unavailable() {
    let sb = shown();
    assert!(
        says(&sb.system, "Machine id: unknown — unavailable"),
        "a permitted reading the service could not answer is unavailable"
    );
    let mut denied = shown();
    show(&mut denied, SystemPage::Permissions);
    assert!(
        says(&denied.system, "Kernel readings: unknown — not permitted"),
        "a reading outside the ceiling is refused, not merely missing"
    );
}

#[test]
fn the_two_absences_are_never_worded_the_same() {
    assert_ne!(
        Unmeasured::NotPermitted.reason(),
        Unmeasured::Unavailable.reason(),
        "a refusal and a fault are different statements to a reader"
    );
    assert_ne!(
        Unmeasured::NoInterface.reason(),
        Unmeasured::Unavailable.reason()
    );
}

#[test]
fn an_absent_list_names_its_reason_rather_than_reading_as_empty() {
    let mut report = system_report();
    report.volumes.clear();
    report.volumes_absent = Some(Unmeasured::NotPermitted);
    let mut m = SwitchboardModel::new("Switchboard");
    m.system = report;
    let mut sb = Switchboard::new(&m);
    sb.select_section(Section::System);
    show(&mut sb, SystemPage::Storage);
    assert!(says(&sb.system, "the mount table — not permitted"));
}

// --- The rail ----------------------------------------------------------

#[test]
fn the_rail_carries_the_models_actions_with_their_verdicts() {
    let sb = shown();
    assert_eq!(sb.system.rail.len(), 2);
    assert_eq!(
        sb.system.rail.items()[0].state().disposition(),
        ControlDisposition::Interactive,
        "an allowed action is offered"
    );
    assert_eq!(
        sb.system.rail.items()[1].state().disposition(),
        ControlDisposition::DisabledByState,
        "a refused action fails closed rather than looking available"
    );
}

#[test]
fn a_refusal_names_the_kind_of_refusal_it_is() {
    // Acquiring the capability would make the first action available, so it
    // wears the Authority Mark. Nothing a reader can be granted would make
    // the second work, so it is plainly disabled instead of pointing them at
    // an authority that would change nothing.
    let denied = SystemAction {
        label: String::from("Shut Down"),
        role: ControlRole::Destructive,
        allowed: false,
        refusal: Some(Unmeasured::NotPermitted),
    };
    let absent = SystemAction {
        refusal: Some(Unmeasured::NoInterface),
        ..denied.clone()
    };
    let allowed = SystemAction {
        allowed: true,
        refusal: None,
        ..denied.clone()
    };
    assert_eq!(
        super::action_verdict(&denied),
        ActionVerdict::DeniedByAuthority
    );
    assert_eq!(
        super::action_verdict(&absent),
        ActionVerdict::DisabledByState
    );
    assert_eq!(super::action_verdict(&allowed), ActionVerdict::Ready);
}

// --- Keyboard reach ----------------------------------------------------

#[test]
fn the_cursor_reaches_every_page_and_then_every_rail_action() {
    let sb = shown();
    assert_eq!(
        sb.system.focus_span(),
        SystemPage::ALL.len() + sb.system.rail.len(),
        "the pages come first, the actions after them"
    );
}

#[test]
fn enter_on_a_sidebar_stop_shows_that_page() {
    let mut sb = shown();
    sb.system.set_content_focus(SystemPage::Storage.index());
    assert!(activate(&mut sb, Key::Named(NamedKey::Enter)).is_none());
    assert_eq!(sb.system.page, SystemPage::Storage);
}

#[test]
fn enter_on_a_rail_stop_reports_the_action_for_the_service_to_authorise() {
    let mut sb = shown();
    sb.system.set_content_focus(SystemPage::ALL.len());
    let outcome = activate(&mut sb, Key::Named(NamedKey::Enter));
    assert!(
        matches!(
            outcome,
            Some(SectionOutcome::Action(SwitchboardAction::System {
                index: 0
            }))
        ),
        "the view performs no privileged work; it reports the intent"
    );
}

#[test]
fn enter_on_a_refused_rail_stop_dispatches_nothing() {
    // The keyboard must reach the same verdict the pointer does: a command
    // the screen is showing as refused cannot be dispatched by moving the
    // cursor onto it and pressing Enter. Both refusals are checked, because
    // they fail closed for different reasons and a route that consulted
    // only one of them would still be open on the other.
    for refusal in [Unmeasured::NoInterface, Unmeasured::NotPermitted] {
        let mut report = system_report();
        let Some(action) = report.actions.get_mut(1) else {
            panic!("the fixture's rail carries a second, refused action");
        };
        action.allowed = false;
        action.refusal = Some(refusal);
        let mut m = model();
        m.system = report;
        let mut sb = Switchboard::new(&m);
        sb.select_section(Section::System);

        sb.system.set_content_focus(SystemPage::ALL.len() + 1);
        assert!(
            activate(&mut sb, Key::Named(NamedKey::Enter)).is_none(),
            "a refused command must not fire from the keyboard ({refusal:?})"
        );
        assert!(
            activate(&mut sb, Key::Char(' ')).is_none(),
            "neither commit key may bypass the refusal ({refusal:?})"
        );
    }
}

#[test]
fn a_key_that_is_not_a_commit_does_nothing() {
    let mut sb = shown();
    sb.system.set_content_focus(SystemPage::Storage.index());
    assert!(activate(&mut sb, Key::Char('x')).is_none());
    assert_eq!(
        sb.system.page,
        SystemPage::Overview,
        "only a commit key changes the page"
    );
}

// --- Painting ----------------------------------------------------------

#[test]
fn both_themes_and_the_heavier_contrast_path_render() {
    for theme in [Theme::dark(), Theme::light(), high_contrast()] {
        let mut sb = shown();
        let surface = paint(&mut sb, &theme);
        assert!(
            has_ink(&surface, bounds()),
            "the System screen must paint under every theme"
        );
    }
}

#[test]
fn every_page_paints() {
    let theme = Theme::dark();
    for page in SystemPage::ALL {
        let mut sb = shown();
        show(&mut sb, page);
        let surface = paint(&mut sb, &theme);
        assert!(
            has_ink(&surface, bounds()),
            "{} must paint something",
            page.title()
        );
    }
}

/// Move the cursor to `page`'s sidebar stop and commit it, exactly as a
/// reader would.
fn show(sb: &mut Switchboard, page: SystemPage) {
    sb.system.set_content_focus(page.index());
    activate(sb, Key::Named(NamedKey::Enter));
    assert_eq!(sb.system.page, page, "the page must have been committed");
}

#[test]
fn a_page_switch_keeps_the_readings_it_was_given() {
    let mut sb = shown();
    let before = sb.system.report.headline.clone();
    show(&mut sb, SystemPage::Network);
    assert_eq!(
        sb.system.report.headline, before,
        "switching page must not need a fresh sample"
    );
}

#[test]
fn the_pages_round_trip_through_their_index() {
    for page in SystemPage::ALL {
        assert_eq!(SystemPage::from_index(page.index()), Some(page));
    }
    assert_eq!(
        SystemPage::from_index(SystemPage::ALL.len()),
        None,
        "an index past the last page fails closed"
    );
}

#[test]
fn the_sections_own_title_is_system() {
    assert_eq!(
        Section::System.title(),
        "System",
        "the variant and the title a reader sees must agree"
    );
    assert_eq!(
        Section::from_index(Section::System.index()),
        Some(Section::System)
    );
}
