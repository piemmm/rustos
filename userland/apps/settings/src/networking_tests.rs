//! Tests for Networking: the stack-wide options the TCP/IP pane stages, the
//! resolver set the DNS pane states, the per-interface addressing an
//! authenticated run answers, and the change the two panes stage over it.
//!
//! No transport and no broker anywhere: the shell is told what a reading
//! answered and what an elevated run came to, exactly as the General tests
//! drive the same seams.

use alloc::string::{String, ToString};
use alloc::vec::Vec;
use core::fmt::Write as _;

use tairix_abi::net_ipc::{NetAddrFamily, NetServerAddr};
use tairix_controls::StatusPill;
use tairix_geometry::Scale;
use tairix_netconfig::Ipv4Method;
use tairix_sysconfig::{Key, NetToggle, SynCookies, SystemConfig};
use tairix_wallpaper::DesktopSettings;

use crate::form::Composition;
use crate::registry::{strip_rows, Category, Pane, PaneBacking, PaneContent, StripRow};
use crate::shell::{ElevateRefusal, Elevated, Elevation, RunMode, Shell};
use crate::test_support::{
    band_line, captions, damage, labels, offer_account, press_band, row_at, row_for, showing,
    stated, theme, WIDE,
};

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

/// A listing of both registries, as `configure` prints one.
const LISTING: &[u8] = b"os.loginType graphical\n\
    net.ipv4.enabled true\n\
    wan.kind ethernet\n\
    wan.match.mac 52:54:00:12:34:56\n\
    wan.ipv4.method static\n\
    wan.ipv4.address 10.0.0.7/24\n\
    wan.ipv4.gateway 10.0.0.1\n\
    wan.dns.servers 10.0.0.53\n\
    lan0.match.node 0x3f201000\n\
    lan0.ipv4.method dhcp\n";

/// A shell showing `pane` with the addressing capture already answered.
fn showing_addressing(pane: &str) -> Shell {
    let mut shell = showing(pane);
    let _ = ask_for_addressing(&mut shell);
    shell.adopt_elevation(Elevated::Printed(0, LISTING.to_vec()));
    shell.lay_out(WIDE, Scale::ONE, &theme());
    shell
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
/// One `<key> <value>` pair as [`Form::pending`] answers it.
fn staged(key: Key, value: &str) -> (String, String) {
    (key.name().to_string(), value.to_string())
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
        alloc::vec![staged(Key::NetIpv4Enabled, "false")]
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
            alloc::vec![staged(key, value)],
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
        .flat_map(|(key, value)| [key, value])
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
    shell.adopt_resolvers(Some(alloc::vec![
        v4([10, 0, 0, 53]),
        v6([0x2001, 0x4860, 0x4860, 0, 0, 0, 0, 0x8888]),
    ]));
    // The live set is ungated and states itself; beneath it the pane says
    // that the per-interface lists have not been read.
    assert_eq!(
        &stated(&shell)[..2],
        ["10.0.0.53".to_string(), "2001:4860:4860::8888".to_string()],
        "one row per server, in the order the stack aggregated them"
    );
}

#[test]
fn dns_tells_an_empty_set_apart_from_a_reading_it_could_not_take() {
    // Before anything lands.
    let mut shell = showing("dns");
    assert_eq!(stated(&shell)[0], "not measured");

    // A refused or undecodable walk: still not measured, never "none".
    shell.adopt_resolvers(None);
    assert_eq!(stated(&shell)[0], "not measured");

    // The query answered, and what it answered was an empty set. That is a
    // machine that resolves nothing, which is a different fact.
    shell.adopt_resolvers(Some(Vec::new()));
    assert_eq!(stated(&shell)[0], "none — this machine resolves no names");
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
    shell.adopt_resolvers(Some(alloc::vec![v4([10, 0, 0, 53])]));
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
fn the_dns_pane_offers_the_same_reading_the_ethernet_pane_does() {
    // Its per-interface rows are discovered from the same capture, so it
    // offers the same command to get one rather than a second way of
    // asking.
    assert_eq!(row_for(Pane::Dns).action(), Some("Show Addressing…"));
}

// --- Ethernet: the reading an authenticated run answers -----------------

/// Press the reading band's one command and offer an account for it.
fn ask_for_addressing(shell: &mut Shell) -> Elevation {
    press_band(shell, 0);
    offer_account(shell)
}

/// Press the staged band's Apply and offer an account for it.
fn apply_addressing(shell: &mut Shell) -> Elevation {
    press_band(shell, 1);
    offer_account(shell)
}

#[test]
fn ethernet_states_that_its_reading_is_not_public_until_it_is_asked_for() {
    // An interface's hardware identity and this machine's address book are
    // gated readings and Settings holds no capability at all, so the pane
    // states that nothing has been read rather than drawing a row it
    // cannot back — and offers the one command that can answer it.
    let row = row_for(Pane::Ethernet);
    assert_eq!(
        row.backing,
        PaneBacking::Composed(PaneContent::Form(Composition::Ethernet))
    );
    assert_eq!(row.action(), Some("Show Addressing…"));

    let mut shell = showing("ethernet");
    let stated = stated(&shell).join(" | ");
    assert!(stated.contains("not read"), "{stated}");
    assert!(stated.contains("account that may"), "{stated}");
    // One command, and no Revert: there is no working copy to revert to.
    assert_eq!(shell.action_rects(WIDE, Scale::ONE, &theme()).len(), 1);
    // Nothing is asked of a desk for it: the resolver set is a live
    // reading, this is a store read an account has to authorise.
    assert!(!shell.network_wanted(), "no desk answers the addressing");
    let _ = &mut shell;
}

#[test]
fn the_pane_asks_for_a_capture_of_the_tool_that_owns_the_store() {
    let mut shell = showing("ethernet");
    let asked = ask_for_addressing(&mut shell);
    assert_eq!(asked.program, "/System/Commands/configure.app/Run");
    assert!(asked.argv.is_empty(), "the listing takes no operand");
    // A read, so the caller waits for what it printed rather than for an
    // exit code alone.
    assert_eq!(asked.mode, RunMode::Capture);
}

#[test]
fn a_captured_listing_states_one_plate_per_configured_interface() {
    let shell = showing_addressing("ethernet");
    assert_eq!(
        captions(&shell),
        alloc::vec!["wan".to_string(), "lan0".to_string()],
        "one plate per interface, in the order the document declares them"
    );
    let stated = stated(&shell).join(" | ");
    // The per-interface registry only: every machine setting in the same
    // listing is another registry's and is not an interface's addressing.
    assert!(stated.contains("10.0.0.7/24"), "{stated}");
    assert!(stated.contains("dhcp"), "{stated}");
    assert!(!stated.contains("graphical"), "{stated}");
    // And the reading is labelled in a reader's words, not in store keys.
    let labelled = labels(&shell).join(" | ");
    assert!(labelled.contains("IPv4 address"), "{labelled}");
    assert!(!labelled.contains("ipv4.address"), "{labelled}");
}

#[test]
fn the_addressing_rows_are_settable_and_the_hardware_rows_are_not() {
    // Which device an alias stands for, and how a bond is composed, are
    // not a settings pane's to change; how the interface is addressed is.
    let shell = showing_addressing("ethernet");
    let form = shell.form_for_test().expect("a composed pane");
    let settable = |label: &str| {
        let (group, row) = row_at(&shell, "wan", label);
        !matches!(
            form.groups()[group].rows()[row].control(),
            tairix_controls::FieldControl::Reading(_)
        )
    };
    for label in [
        "IPv4",
        "IPv4 address",
        "IPv4 gateway",
        "MTU",
        "Name servers",
    ] {
        assert!(settable(label), "`{label}` is settable");
    }
    for label in ["Kind", "Bound to hardware address"] {
        assert!(!settable(label), "`{label}` is a reading");
    }
    // An unset settable is still offered — an interface on DHCP has to be
    // reachable to give a static address to.
    let (group, row) = row_at(&shell, "lan0", "IPv4 address");
    assert!(matches!(
        form.groups()[group].rows()[row].control(),
        tairix_controls::FieldControl::Text(_)
    ));
    // An unset *reading* is not: there is nothing to read.
    assert!(
        !labels(&shell).iter().any(|label| label == "Bond mode"),
        "no interface declares a bond"
    );
}

#[test]
fn a_method_row_offers_the_spellings_the_engine_itself_admits() {
    // The choices come from the key's own shape, so the set a reader is
    // offered and the set the parser takes cannot drift — plus the one
    // choice only the document can express, that the key is not declared.
    let shell = showing_addressing("ethernet");
    let (group, row) = row_at(&shell, "wan", "IPv4");
    let form = shell.form_for_test().expect("a composed pane");
    let tairix_controls::FieldControl::Combo(combo) = form.groups()[group].rows()[row].control()
    else {
        panic!("a closed key is a choice list");
    };
    let offered: Vec<&str> = combo.choices().iter().map(String::as_str).collect();
    assert_eq!(offered[0], "not set");
    assert_eq!(&offered[1..], Ipv4Method::VALUES);
    assert_eq!(combo.selected_text(), Some("static"));
}

#[test]
fn typing_an_address_stages_one_key_of_one_interface() {
    let mut shell = showing_addressing("ethernet");
    let (group, row) = row_at(&shell, "wan", "IPv4 address");
    assert!(shell.type_for_test(group, row, "10.0.0.9/24"));
    assert_eq!(
        shell.form_for_test().expect("a form").pending(),
        alloc::vec![("wan.ipv4.address".to_string(), "10.0.0.9/24".to_string())],
        "one interface's one key, named as the tool takes it"
    );
    // And the plate holding it says so, so the band's count names a part
    // of the pane rather than the whole.
    let form = shell.form_for_test().expect("a form");
    assert_eq!(
        form.groups()[group].badge().map(StatusPill::label),
        Some("1 change")
    );
    assert!(
        form.groups()
            .iter()
            .filter(|plate| plate.caption() == "lan0")
            .all(|plate| plate.badge().is_none()),
        "an untouched interface is not marked"
    );
}

#[test]
fn clearing_an_entry_stages_the_removal_the_registry_spells_as_empty() {
    // That registry has no defaults, so a key is removed rather than reset
    // — and the empty value is the removal, which is what makes an
    // interface movable off a static address at all.
    let mut shell = showing_addressing("ethernet");
    let (group, row) = row_at(&shell, "wan", "IPv4 gateway");
    assert!(shell.type_for_test(group, row, ""));
    assert_eq!(
        shell.form_for_test().expect("a form").pending(),
        alloc::vec![("wan.ipv4.gateway".to_string(), String::new())]
    );
}

#[test]
fn a_value_the_store_would_refuse_is_marked_and_blocks_the_apply() {
    let mut shell = showing_addressing("ethernet");
    let (group, row) = row_at(&shell, "wan", "IPv4 address");
    shell.type_for_test(group, row, "10.0.0");
    let form = shell.form_for_test().expect("a form");
    assert_eq!(form.refused(), 1, "the row wears the refusal");
    assert_eq!(
        form.groups()[group].rows()[row].state().validation,
        tairix_controls::ValidationState::Invalid
    );
    // The text is left exactly as it was typed: the reader corrects it,
    // and nothing silently replaces what they wrote.
    assert!(stated(&shell).iter().any(|value| value == "10.0.0"));
    // And the change does not go part-way: Apply is not offered at all
    // while a row holds a value the store would not take.
    assert!(!shell.asking());
    press_band(&mut shell, 1);
    assert!(!shell.asking(), "no account is asked for a refused change");

    // Correcting it clears both.
    shell.type_for_test(group, row, "10.0.0.9/24");
    assert_eq!(shell.form_for_test().expect("a form").refused(), 0);
}

#[test]
fn moving_an_interface_off_a_static_address_takes_both_halves_together() {
    // Neither half is a document the parser accepts on its own, so a
    // working copy checked per key could never make the change. The staged
    // set holds both and the whole is checked once, when it is applied.
    let mut shell = showing_addressing("ethernet");
    let (group, row) = row_at(&shell, "wan", "IPv4 address");
    shell.type_for_test(group, row, "");
    let (group, row) = row_at(&shell, "wan", "IPv4 gateway");
    shell.type_for_test(group, row, "");
    let (group, row) = row_at(&shell, "wan", "IPv4");
    // Choice 0 is *not set*; the spellings follow in the engine's order.
    let dhcp = 1 + Ipv4Method::VALUES
        .iter()
        .position(|value| *value == "dhcp")
        .expect("the engine offers dhcp");
    assert!(shell.choose_for_test(group, row, dhcp));

    let asked = apply_addressing(&mut shell);
    assert_eq!(asked.program, "/System/Commands/configure.app/Run");
    assert_eq!(asked.mode, RunMode::Wait, "a write, not a read");
    assert_eq!(
        asked.argv,
        alloc::vec![
            "wan.ipv4.method".to_string(),
            "dhcp".to_string(),
            "wan.ipv4.address".to_string(),
            String::new(),
            "wan.ipv4.gateway".to_string(),
            String::new(),
        ],
        "one invocation carrying every key, in registry order"
    );
}

#[test]
fn a_half_made_change_is_refused_here_rather_than_by_the_run() {
    // Dropping the address without moving the method leaves a document
    // the store would not take. The pane checks the whole before asking
    // for a password, so the reader is told what is wrong.
    let mut shell = showing_addressing("ethernet");
    let (group, row) = row_at(&shell, "wan", "IPv4 address");
    shell.type_for_test(group, row, "");
    press_band(&mut shell, 1);
    assert!(
        !shell.asking(),
        "no account is asked for a refused document"
    );
    assert!(
        band_line(&shell).to_lowercase().contains("interface"),
        "the band names what is inconsistent: {}",
        band_line(&shell)
    );
}

#[test]
fn a_change_larger_than_one_request_is_refused_before_a_password_is_typed() {
    // The seam bounds what an unprivileged caller may hand a privileged
    // run, and the change goes in one invocation or not at all — so the
    // pane says so where it costs nothing rather than after the reader
    // has authenticated.
    let mut shell = showing("ethernet");
    let _ = ask_for_addressing(&mut shell);
    let mut listing = String::new();
    for index in 0..9 {
        let _ = write!(
            listing,
            "if{index}.match.node 0x3f20{index}000\nif{index}.ipv4.method dhcp\n"
        );
    }
    shell.adopt_elevation(Elevated::Printed(0, listing.into_bytes()));
    shell.lay_out(WIDE, Scale::ONE, &theme());
    for index in 0..9 {
        let (group, row) = row_at(&shell, &alloc::format!("if{index}"), "MTU");
        assert!(shell.type_for_test(group, row, "9000"));
    }
    assert!(
        shell.form_for_test().expect("a form").pending().len() > 8,
        "more pairs than one request carries"
    );
    press_band(&mut shell, 1);
    assert!(
        !shell.asking(),
        "no account is asked for a change that cannot go"
    );
    assert!(
        band_line(&shell).contains("one interface at a time"),
        "{}",
        band_line(&shell)
    );
}

#[test]
fn an_applied_change_is_recorded_rather_than_re_read_or_forgotten() {
    let mut shell = showing_addressing("ethernet");
    let (group, row) = row_at(&shell, "wan", "IPv4 address");
    shell.type_for_test(group, row, "10.0.0.9/24");
    let _ = apply_addressing(&mut shell);

    shell.adopt_elevation(Elevated::Finished(0));
    assert!(!shell.asking(), "the question is answered");
    let form = shell.form_for_test().expect("a form");
    assert!(
        form.pending().is_empty(),
        "what the run accepted is no longer a pending change"
    );
    assert!(
        stated(&shell).iter().any(|value| value == "10.0.0.9/24"),
        "the row keeps the value the run wrote"
    );
    assert!(
        shell
            .form_for_test()
            .expect("a form")
            .groups()
            .iter()
            .all(|plate| plate.badge().is_none()),
        "no plate still claims a staged change"
    );
}

#[test]
fn a_refused_apply_keeps_the_working_copy_and_says_why() {
    let mut shell = showing_addressing("ethernet");
    let (group, row) = row_at(&shell, "wan", "IPv4 address");
    shell.type_for_test(group, row, "10.0.0.9/24");
    let _ = apply_addressing(&mut shell);

    shell.adopt_elevation(Elevated::Refused(ElevateRefusal::NotRun(String::from(
        "The account was accepted, but nothing ran.",
    ))));
    assert!(shell.asking(), "the reader can correct and try again");
    assert_eq!(
        shell.form_for_test().expect("a form").pending(),
        alloc::vec![("wan.ipv4.address".to_string(), "10.0.0.9/24".to_string())],
        "the staged change is still there to retry"
    );
}

#[test]
fn reverting_puts_every_row_back_to_what_the_capture_stated() {
    let mut shell = showing_addressing("ethernet");
    let (group, row) = row_at(&shell, "wan", "IPv4 address");
    shell.type_for_test(group, row, "10.0.0.9/24");
    press_band(&mut shell, 0);
    let form = shell.form_for_test().expect("a form");
    assert!(form.pending().is_empty());
    assert!(stated(&shell).iter().any(|value| value == "10.0.0.7/24"));
}

#[test]
fn leaving_the_networking_panes_drops_the_capture_but_moving_between_them_does_not() {
    let mut shell = showing_addressing("ethernet");
    let mut sink = damage();
    // DNS is discovered from the same capture, so one authentication
    // serves both panes.
    shell.go_to_pane("dns", WIDE, Scale::ONE, &theme(), &mut sink);
    assert!(
        captions(&shell).iter().any(|caption| caption == "wan"),
        "the capture is still held"
    );
    // Anywhere else and it is dropped as the pane is left, not once the
    // next one after that is: the machine's address book is a privileged
    // reading with no business sitting here while the reader is elsewhere.
    shell.go_to_pane("about", WIDE, Scale::ONE, &theme(), &mut sink);
    assert!(
        shell.addressing_for_test().document().is_none(),
        "the capture is dropped on leaving, not on arriving somewhere else"
    );
    shell.go_to_pane("ethernet", WIDE, Scale::ONE, &theme(), &mut sink);
    let stated = stated(&shell).join(" | ");
    assert!(stated.contains("not read"), "{stated}");
}

#[test]
fn a_listing_that_names_no_interface_says_so_rather_than_drawing_nothing() {
    let mut shell = showing("ethernet");
    let _ = ask_for_addressing(&mut shell);
    shell.adopt_elevation(Elevated::Printed(0, b"os.loginType text\n".to_vec()));
    let stated = stated(&shell).join(" | ");
    assert!(stated.contains("no interface is configured"), "{stated}");
}

#[test]
fn an_interface_no_device_can_bind_to_says_so_on_its_own_plate() {
    // `configure` states the same limit when such an interface is
    // written, onto a console a desktop reader never sees. The pane can
    // see it in the document it is editing, so it says so first.
    let mut shell = showing("ethernet");
    let _ = ask_for_addressing(&mut shell);
    shell.adopt_elevation(Elevated::Printed(
        0,
        b"free.ipv4.method dhcp\nwan.match.mac 52:54:00:12:34:56\n".to_vec(),
    ));
    let form = shell.form_for_test().expect("a form");
    let footnote = |caption: &str| {
        form.groups()
            .iter()
            .find(|plate| plate.caption() == caption)
            .and_then(|plate| plate.footnote().map(String::from))
    };
    assert!(
        footnote("free").is_some_and(|said| said.contains("no device is ever bound")),
        "an interface with no hardware match says so"
    );
    assert!(footnote("wan").is_none(), "a bound interface says nothing");
}

#[test]
fn a_run_that_printed_past_the_bound_states_that_and_shows_no_part_of_it() {
    let mut shell = showing("ethernet");
    let _ = ask_for_addressing(&mut shell);
    shell.adopt_elevation(Elevated::Overran);
    let stated = stated(&shell).join(" | ");
    assert!(stated.contains("too large"), "{stated}");
    assert!(
        !shell.asking(),
        "the question is answered, not left standing"
    );
}

#[test]
fn a_refused_read_states_the_refusal_and_shows_nothing() {
    let mut shell = showing("ethernet");
    let _ = ask_for_addressing(&mut shell);
    shell.adopt_elevation(Elevated::Refused(ElevateRefusal::NotRun(String::from(
        "The account was accepted, but nothing ran.",
    ))));
    // The question stays up with the reason on it, and the pane still
    // states that nothing has been read — never a half answer.
    assert!(shell.asking(), "the reader can correct and try again");
    let stated = stated(&shell).join(" | ");
    assert!(stated.contains("not read"), "{stated}");
}

#[test]
fn a_run_that_failed_is_not_read_as_an_empty_configuration() {
    let mut shell = showing("ethernet");
    let _ = ask_for_addressing(&mut shell);
    shell.adopt_elevation(Elevated::Printed(2, Vec::new()));
    let stated = stated(&shell).join(" | ");
    assert!(!stated.contains("no interface is configured"), "{stated}");
    assert!(shell.asking(), "a failed run is a refusal, not an answer");
}

#[test]
fn a_listing_shaped_like_anything_at_all_is_read_or_refused_and_never_believed() {
    // The caller chooses the program and the argv, so the relayed bytes
    // are attacker-influenced by construction: every shape has to answer
    // either a document or a refusal, and nothing in between.
    for output in [
        &b"\xff\xfe not utf-8"[..],
        b"",
        b"\n\n\n",
        b"nospace",
        b"noalias value",
        b".leadingdot value",
        b"wan. value",
        b"0bad.mtu 1500",
        b"wan.nosuch 1",
        b"wan.mtu",
        b"wan.mtu 1500 extra",
    ] {
        let mut shell = showing("ethernet");
        let _ = ask_for_addressing(&mut shell);
        shell.adopt_elevation(Elevated::Printed(0, output.to_vec()));
        let stated = stated(&shell).join(" | ");
        assert!(
            stated.contains("no interface is configured") || stated.contains("could not read"),
            "{output:?} answered `{stated}`"
        );
    }
}

#[test]
fn a_listing_the_engine_will_not_take_whole_is_no_document_at_all() {
    // Fail closed: a line whose value the shared engine refuses means the
    // window and the tool disagree about the store, so nothing is shown
    // rather than the part that happened to parse.
    let mut shell = showing("ethernet");
    let _ = ask_for_addressing(&mut shell);
    shell.adopt_elevation(Elevated::Printed(0, b"wan.mtu 3\n".to_vec()));
    let stated = stated(&shell).join(" | ");
    assert!(stated.contains("could not read"), "{stated}");
}

// --- DNS: the live reading, and the per-interface resolvers -------------

#[test]
fn dns_states_the_live_set_above_each_interfaces_own() {
    let mut shell = showing_addressing("dns");
    shell.adopt_resolvers(Some(alloc::vec![v4([10, 0, 0, 53])]));
    assert_eq!(
        captions(&shell),
        alloc::vec![
            "NAME SERVERS IN USE".to_string(),
            "wan".to_string(),
            "lan0".to_string()
        ],
        "the aggregated set the stack answered, then what each interface asks for"
    );
    // The interface plates carry their resolver list and nothing else of
    // the addressing, which is the Ethernet pane's.
    let labelled = labels(&shell);
    assert!(!labelled.iter().any(|label| label == "IPv4 address"));
    let (group, row) = row_at(&shell, "wan", "Name servers");
    assert!(shell.type_for_test(group, row, "10.0.0.53,10.0.0.54"));
    assert_eq!(
        shell.form_for_test().expect("a form").pending(),
        alloc::vec![(
            "wan.dns.servers".to_string(),
            "10.0.0.53,10.0.0.54".to_string()
        )]
    );
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
fn the_three_composed_networking_panes_declare_what_they_draw() {
    assert_eq!(
        row_for(Pane::TcpIp).content(),
        Some(PaneContent::Form(Composition::TcpIp))
    );
    assert_eq!(
        row_for(Pane::Dns).content(),
        Some(PaneContent::Form(Composition::Dns))
    );
    assert_eq!(
        row_for(Pane::Ethernet).content(),
        Some(PaneContent::Form(Composition::Ethernet))
    );
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
