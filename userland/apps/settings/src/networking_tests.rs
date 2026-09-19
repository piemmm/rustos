//! Tests for Networking: the stack-wide options the TCP/IP pane stages, the
//! resolver set the DNS pane states, and the two readings Settings
//! deliberately does not take.
//!
//! No transport and no broker anywhere: the shell is told what a reading
//! answered and what an elevated run came to, exactly as the General tests
//! drive the same seams.

use alloc::string::{String, ToString};
use alloc::vec::Vec;

use tairix_abi::net_ipc::{NetAddrFamily, NetServerAddr};
use tairix_geometry::{Rect, Scale};
use tairix_sysconfig::{Key, NetToggle, SynCookies, SystemConfig};
use tairix_theme::Theme;
use tairix_wallpaper::DesktopSettings;

use crate::facts::NetworkFacts;
use crate::registry::{strip_rows, Category, Pane, PaneBacking, PaneContent, StripRow, CATEGORIES};
use crate::shell::Shell;
use tairix_font::install_test_transport;

/// A window wide enough to seat the strip and a full content column.
const WIDE: Rect = Rect::new(0, 0, 900, 640);

fn theme() -> Theme {
    install_test_transport();
    Theme::dark()
}

fn damage() -> tairix_geometry::Region {
    tairix_controls::damage::sink()
}

/// A shell showing `pane`.
fn showing(pane: &str) -> Shell {
    let mut shell = Shell::new(DesktopSettings::default()).expect("a registry");
    let mut sink = damage();
    assert!(
        shell.go_to_pane(pane, WIDE, Scale::ONE, &theme(), &mut sink),
        "the registry carries the pane"
    );
    shell.lay_out(WIDE, Scale::ONE, &theme());
    shell
}

/// A shell showing `pane` with the machine's store already read.
fn showing_with(pane: &str, config: SystemConfig) -> Shell {
    let mut shell = Shell::new(DesktopSettings::default()).expect("a registry");
    shell.adopt_config(Some(config));
    let mut sink = damage();
    assert!(
        shell.go_to_pane(pane, WIDE, Scale::ONE, &theme(), &mut sink),
        "the registry carries the pane"
    );
    shell.lay_out(WIDE, Scale::ONE, &theme());
    shell
}

/// Every value the pane on show states, in listing order.
fn stated(shell: &Shell) -> Vec<String> {
    let Some(facts) = shell.facts_for_test() else {
        return Vec::new();
    };
    facts
        .rows()
        .iter()
        .map(|row| match row.control() {
            tairix_controls::FieldControl::Reading(value) => value.clone(),
            _ => String::new(),
        })
        .collect()
}

/// A V4 resolver at `octets`.
fn v4(octets: [u8; 4]) -> NetServerAddr {
    let mut addr = [0u8; 16];
    addr[..4].copy_from_slice(&octets);
    NetServerAddr {
        family: NetAddrFamily::V4,
        addr,
    }
}

/// A V6 resolver at `groups`.
fn v6(groups: [u16; 8]) -> NetServerAddr {
    let mut addr = [0u8; 16];
    for (index, group) in groups.iter().enumerate() {
        addr[index * 2..index * 2 + 2].copy_from_slice(&group.to_be_bytes());
    }
    NetServerAddr {
        family: NetAddrFamily::V6,
        addr,
    }
}

/// The registry row for `pane`.
fn row_for(pane: Pane) -> &'static crate::registry::PaneRow {
    CATEGORIES
        .iter()
        .flat_map(|category| category.panes)
        .find(|row| row.pane == pane)
        .expect("every pane has a row")
}

// --- TCP/IP: the stack-wide options -------------------------------------

#[test]
fn tcp_ip_states_what_the_store_holds_and_stages_a_change() {
    let mut shell = showing_with("tcp-ip", SystemConfig::default());
    // Nothing is written by looking, and nothing by choosing either.
    assert!(!shell.asking(), "opening the pane asks for no account");
    assert!(
        shell
            .form_for_test()
            .expect("a composed pane")
            .pending()
            .is_empty(),
        "an untouched pane has staged nothing"
    );

    // The second choice of the first row turns IPv4 off.
    assert!(shell.choose_for_test(0, 0, 1), "the row stages its change");
    assert!(!shell.asking(), "a choice is not a write");
    assert_eq!(
        shell.form_for_test().expect("a form").pending(),
        alloc::vec![(Key::NetIpv4Enabled, "false")]
    );
}

#[test]
fn every_stack_wide_option_is_reachable_and_writes_its_own_key() {
    // One row per `net.*` key, each staging exactly the key it owns: a row
    // that wrote a neighbour's key would be a setting the reader cannot
    // account for.
    let expected = [
        (0usize, 0usize, Key::NetIpv4Enabled, "false"),
        (0, 1, Key::NetIpv6Enabled, "false"),
        (0, 2, Key::NetIpv6Privacy, "false"),
        (1, 0, Key::NetTcpSynCookies, "always"),
        (1, 1, Key::NetTcpKeepalive, "false"),
        (1, 2, Key::NetTcpEcn, "false"),
    ];
    for (group, row, key, value) in expected {
        // Start each row at the choice the other one is not, so picking
        // index 1 is always a change.
        let config = SystemConfig {
            net_ipv6_privacy: NetToggle::Enabled,
            net_tcp_keepalive: NetToggle::Enabled,
            net_tcp_ecn: NetToggle::Enabled,
            ..SystemConfig::default()
        };
        let mut shell = showing_with("tcp-ip", config);
        assert!(
            shell.choose_for_test(group, row, 1),
            "group {group} row {row} stages its change"
        );
        assert_eq!(
            shell.form_for_test().expect("a form").pending(),
            alloc::vec![(key, value)],
            "group {group} row {row} writes only its own key"
        );
    }
}

#[test]
fn applying_runs_the_one_tool_that_owns_the_store_with_every_changed_key() {
    let mut shell = showing_with("tcp-ip", SystemConfig::default());
    assert!(shell.choose_for_test(0, 1, 1), "IPv6 off");
    assert!(shell.choose_for_test(1, 0, 1), "cookies always");

    let argv: Vec<String> = shell
        .form_for_test()
        .expect("a form")
        .pending()
        .into_iter()
        .flat_map(|(key, value)| [key.name().to_string(), value.to_string()])
        .collect();
    // One invocation carrying both, in registry order, so the document is
    // rendered once and cannot be left holding half the change.
    assert_eq!(
        argv,
        alloc::vec![
            "net.ipv6.enabled".to_string(),
            "false".to_string(),
            "net.tcp.syncookies".to_string(),
            "always".to_string(),
        ]
    );
}

#[test]
fn a_row_whose_effect_something_above_it_has_taken_says_so() {
    // The privacy row keeps its own value — that is what the store says —
    // but a reader who saw `On` alone would believe temporary addresses
    // were being formed.
    let config = SystemConfig {
        net_ipv6_enabled: NetToggle::Disabled,
        net_ipv6_privacy: NetToggle::Enabled,
        ..SystemConfig::default()
    };
    let shell = showing_with("tcp-ip", config);
    let form = shell.form_for_test().expect("a form");
    let privacy = form.groups()[0].rows()[2]
        .description()
        .expect("the row states what it does");
    assert!(
        privacy.contains("IPv6 is off for this machine"),
        "the ceiling is stated: {privacy}"
    );
    assert!(
        form.pending().is_empty(),
        "stating a ceiling changes nothing"
    );

    // With IPv6 on, the same row says only what it does.
    let shell = showing_with("tcp-ip", SystemConfig::default());
    let plain = shell.form_for_test().expect("a form").groups()[0].rows()[2]
        .description()
        .expect("the row states what it does");
    assert!(!plain.contains("has no effect"), "no ceiling to state");
}

#[test]
fn the_tcp_rows_say_so_when_neither_address_family_is_on() {
    let config = SystemConfig {
        net_ipv4_enabled: NetToggle::Disabled,
        net_ipv6_enabled: NetToggle::Disabled,
        ..SystemConfig::default()
    };
    let shell = showing_with("tcp-ip", config.clone());
    let form = shell.form_for_test().expect("a form");
    for row in form.groups()[1].rows() {
        let stated = row.description().expect("the row states what it does");
        assert!(
            stated.contains("makes no connections at all"),
            "a TCP row with no family states it: {stated}"
        );
    }
    // One family back on and the statement goes: the rows apply again.
    let config = SystemConfig {
        net_ipv4_enabled: NetToggle::Enabled,
        ..config
    };
    let shell = showing_with("tcp-ip", config);
    for row in shell.form_for_test().expect("a form").groups()[1].rows() {
        let stated = row.description().expect("the row states what it does");
        assert!(!stated.contains("no connections at all"), "{stated}");
    }
}

#[test]
fn an_unread_store_states_that_rather_than_offering_defaults() {
    // A store that has not been read is not a store of defaults: offering
    // `On` would be a value the reader could not have set.
    let shell = showing("tcp-ip");
    let form = shell.form_for_test().expect("a composed pane");
    for group in form.groups() {
        for row in group.rows() {
            assert!(
                matches!(row.control(), tairix_controls::FieldControl::Unmeasured(_)),
                "row `{}` offers nothing until the store is read",
                row.label()
            );
        }
    }
}

#[test]
fn the_syncookie_choice_says_what_always_costs() {
    // The only place the trade-off is stated, so the choice list has to
    // carry it rather than leaving `Always` looking strictly better.
    let shell = showing_with("tcp-ip", SystemConfig::default());
    let row = &shell.form_for_test().expect("a form").groups()[1].rows()[0];
    let tairix_controls::FieldControl::Combo(combo) = row.control() else {
        panic!("the defence policy is a choice list");
    };
    assert_eq!(combo.choices(), ["Automatic", "Always, keeping no queue"]);
    assert_eq!(
        combo.selected(),
        Some(0),
        "an absent store implies the bounded default"
    );
    assert_eq!(SynCookies::default(), SynCookies::Auto);
}

// --- DNS: the one network reading Settings may take ---------------------

#[test]
fn dns_states_every_server_the_stack_answered() {
    let mut shell = showing("dns");
    shell.adopt_network(NetworkFacts {
        resolvers: Some(alloc::vec![
            v4([10, 0, 0, 53]),
            v6([0x2001, 0x4860, 0x4860, 0, 0, 0, 0, 0x8888]),
        ]),
    });
    assert_eq!(
        stated(&shell),
        alloc::vec!["10.0.0.53".to_string(), "2001:4860:4860::8888".to_string()],
        "one row per server, in the order the stack aggregated them"
    );
}

#[test]
fn dns_tells_an_empty_set_apart_from_a_reading_it_could_not_take() {
    // Before anything lands.
    let mut shell = showing("dns");
    assert_eq!(stated(&shell), alloc::vec!["not measured".to_string()]);

    // A refused or undecodable walk: still not measured, never "none".
    shell.adopt_network(NetworkFacts { resolvers: None });
    assert_eq!(stated(&shell), alloc::vec!["not measured".to_string()]);

    // The query answered, and what it answered was an empty set. That is a
    // machine that resolves nothing, which is a different fact.
    shell.adopt_network(NetworkFacts {
        resolvers: Some(Vec::new()),
    });
    assert_eq!(
        stated(&shell),
        alloc::vec!["none — this machine resolves no names".to_string()]
    );
}

#[test]
fn the_dns_pane_asks_for_its_reading_when_it_comes_on_show() {
    let mut shell = Shell::new(DesktopSettings::default()).expect("a registry");
    assert!(
        !shell.network_wanted(),
        "the pane it opens on states no network reading"
    );

    let mut sink = damage();
    shell.go_to_pane("dns", WIDE, Scale::ONE, &theme(), &mut sink);
    assert!(shell.network_wanted(), "the pane asks for its reading");

    // Answering it clears the want, and adopting does not re-arm it — a
    // rebuild that asked again would spend a round trip on the reading it
    // was just handed.
    shell.adopt_network(NetworkFacts {
        resolvers: Some(alloc::vec![v4([10, 0, 0, 53])]),
    });
    assert!(
        !shell.network_wanted(),
        "an answered reading is not re-asked"
    );

    // Leaving and coming back does re-arm it: the set moves as leases come
    // and go, so a returning reader sees what the stack holds now.
    shell.go_to_pane("about", WIDE, Scale::ONE, &theme(), &mut sink);
    assert!(!shell.network_wanted(), "About states no network reading");
    shell.go_to_pane("dns", WIDE, Scale::ONE, &theme(), &mut sink);
    assert!(shell.network_wanted(), "coming back asks again");
}

#[test]
fn the_dns_pane_offers_no_command_of_its_own() {
    // It is a reading in this stage; the write is the stage that grows the
    // per-interface registry. A band offering Apply here would offer a
    // change the pane cannot make.
    assert_eq!(row_for(Pane::Dns).action(), None);
}

// --- What Settings deliberately does not read ---------------------------

#[test]
fn ethernet_states_where_its_privileged_readings_live() {
    // An interface's hardware identity and this machine's address book are
    // gated readings, and Settings holds no capability at all. The pane
    // says where they are read and who writes the addressing, rather than
    // drawing rows that would be refused on every machine for ever.
    let row = row_for(Pane::Ethernet);
    let PaneBacking::Elsewhere { shows, elsewhere } = row.backing else {
        panic!("Ethernet states where its readings live");
    };
    assert!(shows.contains("addressed"), "{shows}");
    assert!(elsewhere.contains("Switchboard"), "{elsewhere}");
    assert!(elsewhere.contains("configure"), "{elsewhere}");
    assert_eq!(row.action(), None, "a stated absence commands nothing");
}

#[test]
fn wifi_still_states_the_absence_of_a_driver() {
    let PaneBacking::None { missing, needs } = row_for(Pane::WiFi).backing else {
        panic!("Wi-Fi states an absent subsystem");
    };
    assert!(missing.contains("wireless"), "{missing}");
    assert!(needs.contains("802.11"), "{needs}");
}

#[test]
fn the_two_composed_networking_panes_declare_what_they_draw() {
    assert_eq!(
        row_for(Pane::TcpIp).content(),
        Some(PaneContent::Form(crate::form::Composition::TcpIp))
    );
    assert_eq!(row_for(Pane::Dns).content(), Some(PaneContent::Dns));
}

#[test]
fn every_stack_wide_option_is_searchable_by_its_own_label() {
    // A reader looking for `IPv6` finds the pane that holds it, which is
    // the whole contract between a pane's rows and the search index.
    for term in row_for(Pane::TcpIp).settings {
        let rows = strip_rows(Category::General, term);
        assert!(
            rows.iter()
                .any(|row| matches!(row, StripRow::Pane(_, pane) if *pane == Pane::TcpIp)),
            "`{term}` reaches TCP/IP"
        );
    }
}

#[test]
fn the_dns_pane_is_reachable_by_the_subject_a_reader_searches_for() {
    let rows = strip_rows(Category::General, "name servers");
    assert!(
        rows.iter()
            .any(|row| matches!(row, StripRow::Pane(_, pane) if *pane == Pane::Dns)),
        "the resolver pane is reachable by what it states"
    );
}
