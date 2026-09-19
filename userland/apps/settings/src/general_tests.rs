//! Tests for General: the machine readings, the staged-apply model, and the
//! credential question the elevated run is offered through.
//!
//! The elevation seam is injected by driving the shell's own outcome — the
//! shell asks for a run and adopts the verdict it is told — so the whole
//! staged model is exercised with no broker, no account database, and no
//! spawn anywhere near it.

use alloc::string::{String, ToString};
use alloc::vec::Vec;

use tairix_abi::sysinfo::{SystemIdentity, Uptime};
use tairix_abi::time::{Duration64, Time64, WallClockReading, WallTimeState};
use tairix_geometry::{to_i32, Point, Rect, Scale};
use tairix_input::{InputEvent, Key, Modifiers, NamedKey, PointerButton};
use tairix_sysconfig::{CacheMode, CacheSwitch, LoginType, SystemConfig};
use tairix_theme::Theme;
use tairix_wallpaper::DesktopSettings;

use crate::facts::MachineFacts;
use crate::shell::{ElevateRefusal, Elevation, Shell, ShellOutcome};
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

/// A shell showing `pane`, with the machine's store already read.
fn showing(pane: &str, config: SystemConfig) -> Shell {
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

/// Choose the second value of group `group`'s row `row`.
fn choose_next(shell: &mut Shell, group: usize, row: usize) {
    assert!(
        shell.choose_for_test(group, row, 1),
        "a machine row stages its change"
    );
}

/// Press the band's command at `index` (`0` Revert, `1` Apply), where the
/// band actually drew it.
fn press_command(shell: &mut Shell, index: usize) -> ShellOutcome {
    let theme = theme();
    let rects = shell.action_rects(WIDE, Scale::ONE, &theme);
    let rect = *rects.get(index).expect("the band drew its commands");
    let at = Point::new(
        rect.left() + to_i32(rect.width / 2),
        rect.top() + to_i32(rect.height / 2),
    );
    let mut sink = damage();
    let mut concluded = ShellOutcome::Idle;
    for event in [
        InputEvent::PointerMoved { to: at },
        InputEvent::PointerPressed {
            button: PointerButton::Primary,
        },
        InputEvent::PointerReleased {
            button: PointerButton::Primary,
        },
    ] {
        let acted = shell.on_pointer(&event, WIDE, Scale::ONE, &theme, &mut sink);
        if acted.changed() {
            concluded = acted;
        }
    }
    concluded
}

/// Press the band's applying command.
fn press_action(shell: &mut Shell) -> ShellOutcome {
    press_command(shell, 1)
}

/// Type `text` into whichever field of the credential question holds the
/// keyboard, then `Tab` or `Enter`.
fn type_into(shell: &mut Shell, text: &str) {
    let theme = theme();
    let mut sink = damage();
    for ch in text.chars() {
        shell.on_key(
            Key::Char(ch),
            Modifiers::default(),
            WIDE,
            Scale::ONE,
            &theme,
            &mut sink,
        );
    }
}

fn press(shell: &mut Shell, key: NamedKey) -> ShellOutcome {
    let theme = theme();
    let mut sink = damage();
    shell.on_key(
        Key::Named(key),
        Modifiers::default(),
        WIDE,
        Scale::ONE,
        &theme,
        &mut sink,
    )
}

// --- The staged model ---------------------------------------------------

#[test]
fn a_staged_row_edits_a_working_copy_and_asks_for_nothing() {
    // The whole point of a staged pane: a choice is not a write, so no
    // password is asked for and no store is touched until Apply.
    let mut shell = showing("login-startup", SystemConfig::default());
    choose_next(&mut shell, 0, 0);
    assert!(!shell.asking(), "a choice asks for no account");
    let form = shell.form_for_test().expect("a composed pane");
    assert_eq!(
        form.pending(),
        alloc::vec![(tairix_sysconfig::Key::LoginType, "text")]
    );
}

#[test]
fn reverting_puts_the_working_copy_back() {
    let mut shell = showing("login-startup", SystemConfig::default());
    choose_next(&mut shell, 0, 0);
    assert_eq!(shell.form_for_test().expect("a form").pending().len(), 1);

    // The leading command in the band is Revert.
    press_command(&mut shell, 0);
    assert!(shell.form_for_test().expect("a form").pending().is_empty());
}

#[test]
fn applying_asks_for_an_account_and_runs_the_tool_that_owns_the_store() {
    let mut shell = showing("caching", SystemConfig::default());
    // Two rows, so the one run carries both changes together.
    choose_next(&mut shell, 0, 0);
    choose_next(&mut shell, 1, 0);
    assert_eq!(shell.form_for_test().expect("a form").pending().len(), 2);

    press_action(&mut shell);
    assert!(shell.asking(), "applying asks for an account");

    type_into(&mut shell, "root");
    press(&mut shell, NamedKey::Tab);
    type_into(&mut shell, "hunter2");
    let ShellOutcome::Elevate(asked) = press(&mut shell, NamedKey::Enter) else {
        panic!("offering an account asks for the run");
    };
    assert_eq!(asked.account, "root");
    assert_eq!(asked.password, b"hunter2");
    assert_eq!(asked.program, "/System/Commands/configure.app/Run");
    assert!(asked.wait, "a store write is waited for");
    // One invocation carrying every changed key, so the document is
    // rendered once and cannot be left half written.
    assert_eq!(
        asked.argv,
        alloc::vec![
            "cache.all".to_string(),
            "off".to_string(),
            "cache.filesystem".to_string(),
            "off".to_string(),
        ]
    );
}

#[test]
fn a_refused_account_keeps_the_question_and_the_working_copy() {
    let mut shell = showing("login-startup", SystemConfig::default());
    choose_next(&mut shell, 0, 0);
    press_action(&mut shell);
    type_into(&mut shell, "root");
    press(&mut shell, NamedKey::Tab);
    type_into(&mut shell, "wrong");
    press(&mut shell, NamedKey::Enter);

    shell.adopt_elevation(Err(ElevateRefusal::Credentials));
    assert!(
        shell.asking(),
        "a refusal leaves the question up to correct"
    );
    // And the working copy stands, so the reader corrects the password
    // rather than retyping the change.
    assert_eq!(shell.form_for_test().expect("a form").pending().len(), 1);
}

#[test]
fn an_accepted_run_takes_the_question_down_and_re_reads_the_store() {
    let mut shell = showing("login-startup", SystemConfig::default());
    choose_next(&mut shell, 0, 0);
    press_action(&mut shell);
    type_into(&mut shell, "root");
    press(&mut shell, NamedKey::Tab);
    type_into(&mut shell, "hunter2");
    press(&mut shell, NamedKey::Enter);

    shell.adopt_elevation(Ok(0));
    assert!(!shell.asking());
    // Persist-then-adopt: what is durable is whatever the store now says,
    // so the window asks for it again rather than declaring its own
    // working copy the truth.
    assert!(shell.config_wanted());

    shell.adopt_config(Some(SystemConfig {
        login_type: LoginType::Text,
        ..SystemConfig::default()
    }));
    assert!(shell.form_for_test().expect("a form").pending().is_empty());
}

#[test]
fn a_run_that_did_not_take_the_change_is_not_reported_as_applied() {
    // A non-zero exit is the tool refusing, which is not a success however
    // cleanly the account authenticated.
    let mut shell = showing("login-startup", SystemConfig::default());
    choose_next(&mut shell, 0, 0);
    press_action(&mut shell);
    type_into(&mut shell, "root");
    press(&mut shell, NamedKey::Tab);
    type_into(&mut shell, "hunter2");
    press(&mut shell, NamedKey::Enter);

    shell.adopt_elevation(Ok(2));
    assert!(shell.asking(), "the question stays up to try again");
    assert_eq!(shell.form_for_test().expect("a form").pending().len(), 1);
    assert!(!shell.config_wanted(), "nothing was written to re-read");
}

#[test]
fn the_date_and_time_pane_launches_the_application_that_owns_the_clock() {
    let mut shell = showing("date-time", SystemConfig::default());
    press_action(&mut shell);
    assert!(shell.asking());
    type_into(&mut shell, "root");
    press(&mut shell, NamedKey::Tab);
    type_into(&mut shell, "hunter2");
    let ShellOutcome::Elevate(asked) = press(&mut shell, NamedKey::Enter) else {
        panic!("offering an account asks for the run");
    };
    assert_eq!(asked.program, "/System/Applications/datetime.app/Run");
    // Started and left running: the reader then works in it, and a window
    // that waited for its exit would stop drawing for the whole session.
    assert!(!asked.wait);
    assert!(
        asked.argv.is_empty(),
        "an interactive program takes no argv"
    );
}

#[test]
fn cancelling_the_question_changes_nothing_anywhere() {
    let mut shell = showing("login-startup", SystemConfig::default());
    choose_next(&mut shell, 0, 0);
    press_action(&mut shell);
    type_into(&mut shell, "root");
    assert_eq!(press(&mut shell, NamedKey::Escape), ShellOutcome::Changed);
    assert!(!shell.asking());
    assert!(!shell.config_wanted());
    assert_eq!(shell.form_for_test().expect("a form").pending().len(), 1);
}

#[test]
fn the_question_is_modal_while_it_is_up() {
    // A press behind the question must not change a pane the reader is
    // about to authenticate for.
    let mut shell = showing("caching", SystemConfig::default());
    choose_next(&mut shell, 0, 0);
    press_action(&mut shell);
    assert!(shell.asking());
    let staged = shell.form_for_test().expect("a form").pending();

    let theme = theme();
    let frame = shell.frame(WIDE, Scale::ONE, &theme);
    let at = Point::new(
        frame.content.left() + to_i32(frame.content.width / 2),
        frame.content.top() + to_i32(frame.content.height / 4),
    );
    let mut sink = damage();
    for event in [
        InputEvent::PointerMoved { to: at },
        InputEvent::PointerPressed {
            button: PointerButton::Primary,
        },
        InputEvent::PointerReleased {
            button: PointerButton::Primary,
        },
    ] {
        shell.on_pointer(&event, WIDE, Scale::ONE, &theme, &mut sink);
    }
    assert_eq!(shell.form_for_test().expect("a form").pending(), staged);
}

#[test]
fn an_offered_secret_is_never_rendered() {
    // A derived `Debug` would print the password into whatever rendered
    // the request — a diagnostic, a log line, a test failure.
    let mut shell = showing("login-startup", SystemConfig::default());
    choose_next(&mut shell, 0, 0);
    press_action(&mut shell);
    type_into(&mut shell, "root");
    press(&mut shell, NamedKey::Tab);
    type_into(&mut shell, "hunter2");
    let ShellOutcome::Elevate(asked) = press(&mut shell, NamedKey::Enter) else {
        panic!("offering an account asks for the run");
    };
    let rendered = alloc::format!("{asked:?}");
    assert!(rendered.contains("root"), "the account is not the secret");
    assert!(
        !rendered.contains("hunter2"),
        "a rendered request must not carry the offered password: {rendered}"
    );
}

#[test]
fn erasing_a_request_zeroes_the_offered_secret() {
    // The one eraser `Drop` runs, checked on a live value: a holder that
    // outlives the exchange calls it explicitly, and every other path is
    // the drop.
    let mut asked = Elevation {
        account: String::from("root"),
        password: b"hunter2".to_vec(),
        program: "/x",
        argv: Vec::new(),
        wait: true,
    };
    asked.erase();
    assert!(
        asked.password.iter().all(|byte| *byte == 0),
        "the secret is zeroed in place, not merely dropped"
    );
    // The account is not a secret and is left as typed, so a refusal can
    // be corrected without retyping it.
    assert_eq!(asked.account, "root");
}

// --- The readings -------------------------------------------------------

#[test]
fn a_machine_row_with_no_reading_says_so_rather_than_showing_a_default() {
    // An unread store is not a store of defaults: showing one would be a
    // value the reader could not account for and could not have set.
    let mut shell = Shell::new(DesktopSettings::default()).expect("a registry");
    let mut sink = damage();
    shell.go_to_pane("caching", WIDE, Scale::ONE, &theme(), &mut sink);
    assert!(shell.config_wanted(), "the pane asks for the reading");
    let form = shell.form_for_test().expect("a composed pane");
    assert!(
        form.pending().is_empty(),
        "there is nothing to apply against a store that was never read"
    );
}

#[test]
fn the_master_switch_restates_the_rows_it_is_a_ceiling_over() {
    // Turning caching off for the machine leaves each class's own value
    // standing — that is what the store says — but a row that only said
    // `Automatic` would read as a cache that is running.
    let mut shell = showing("caching", SystemConfig::default());
    let before = pane_text(&shell);
    choose_next(&mut shell, 0, 0);
    let after = pane_text(&shell);
    assert!(
        after.contains("no effect") && !before.contains("no effect"),
        "the ceiling is stated on the rows it takes away"
    );
}

/// Every word the pane on show draws in its rows, for a test that asks what
/// a reader would actually see.
fn pane_text(shell: &Shell) -> String {
    let Some(form) = shell.form_for_test() else {
        return String::new();
    };
    let mut text = String::new();
    for group in form.groups() {
        for row in group.rows() {
            text.push_str(row.label());
            text.push(' ');
            if let Some(description) = row.description() {
                text.push_str(description);
                text.push(' ');
            }
        }
    }
    text
}

#[test]
fn the_about_pane_states_every_reading_it_could_not_take() {
    // Each query is its own, so one refusal costs one reading — and the
    // row says so rather than borrowing a neighbour's success.
    let mut shell = Shell::new(DesktopSettings::default()).expect("a registry");
    let mut sink = damage();
    shell.go_to_pane("about", WIDE, Scale::ONE, &theme(), &mut sink);
    let unmeasured = stated(&shell);
    assert_eq!(unmeasured.len(), 7, "one row per About reading");
    assert!(
        unmeasured.iter().all(|value| value == "not measured"),
        "a reading that did not arrive is stated, never fabricated: {unmeasured:?}"
    );

    // One reading lands; only that row moves, because each is its own.
    shell.adopt_machine(MachineFacts {
        memory_bytes: Some(2 << 30),
        ..MachineFacts::default()
    });
    let partly = stated(&shell);
    assert_eq!(partly[6], "2.0 GiB");
    assert!(partly[..6].iter().all(|value| value == "not measured"));

    // And the rest.
    shell.adopt_machine(MachineFacts {
        identity: Some(
            SystemIdentity::new(*b"0123456789abcdef", 0, 1, 0, b"tairix").expect("an identity"),
        ),
        uptime: Some(Uptime {
            since_boot: Duration64::from_nanos(90 * 60 * 1_000_000_000),
            boot_time: Time64::UNIX_EPOCH,
        }),
        cpus: Vec::new(),
        memory_bytes: Some(2 << 30),
        clock: None,
    });
    let measured = stated(&shell);
    assert_eq!(measured[0], "tairix");
    assert_eq!(measured[1], "30313233343536373839616263646566");
    assert_eq!(measured[2], "0.1.0");
    assert_eq!(measured[3], "1:30");
    // No processor was reported, so the pane says so rather than naming one.
    assert_eq!(measured[4], "not measured");
    assert_eq!(measured[5], "not measured");
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

#[test]
fn a_clock_that_was_never_set_says_so_rather_than_showing_the_epoch() {
    let mut shell = Shell::new(DesktopSettings::default()).expect("a registry");
    let mut sink = damage();
    shell.go_to_pane("date-time", WIDE, Scale::ONE, &theme(), &mut sink);
    shell.adopt_machine(MachineFacts {
        clock: Some(WallClockReading::UNSET),
        ..MachineFacts::default()
    });
    let unset = stated(&shell);
    assert_eq!(unset[0], "not set");
    assert_eq!(unset[1], "nothing yet");

    // A clock somebody set says where the reading came from, so a user can
    // tell a validated network sample from this machine's own chip.
    shell.adopt_machine(MachineFacts {
        clock: Some(WallClockReading::new(
            Time64::from_secs(1_700_000_000),
            WallTimeState::Trusted,
        )),
        ..MachineFacts::default()
    });
    let set = stated(&shell);
    assert_eq!(set[1], "the network");
    assert_ne!(set[0], "not set");
}

#[test]
fn a_store_whose_master_switch_is_off_still_reports_each_class_value() {
    let shell = showing(
        "caching",
        SystemConfig {
            cache_all: CacheSwitch::Off,
            cache_block: CacheMode::Off,
            ..SystemConfig::default()
        },
    );
    let form = shell.form_for_test().expect("a composed pane");
    // Nothing differs from what is in effect, because nothing was edited:
    // the ceiling is a reading, not a staged change.
    assert!(form.pending().is_empty());
}
