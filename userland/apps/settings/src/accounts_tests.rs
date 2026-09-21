//! Tests for Users & Groups: the three readings of different authority the
//! pane composes, the per-account edits it stages, and the one elevated run
//! that applies them.
//!
//! No transport and no broker anywhere: the shell is told what the ungated
//! readings answered and what an elevated run came to, exactly as the
//! networking tests drive the same seams.

use alloc::string::{String, ToString};
use alloc::vec::Vec;

use tairix_abi::sysinfo::{SelfAccountRecord, SelfAccountText};
use tairix_abi::users_admin::AccountStateCode;
use tairix_abi::CapabilityId;
use tairix_controls::{FieldControl, ValidationState};
use tairix_font::install_test_transport;
use tairix_geometry::Scale;
use tairix_icon::NoArtwork;
use tairix_raster::Surface;
use tairix_theme::Theme;
use tairix_useradmin::{listing, Account, Group};
use tairix_users::{PasswordRecord, Salt, MAX_PASSWORD_LEN};
use tairix_wallpaper::DesktopSettings;

use crate::accounts::{AccountFacts, OwnAccount, Roster};
use crate::form::Composition;
use crate::registry::{Pane, PaneBacking, PaneContent};
use crate::shell::{ElevateRefusal, Elevated, Elevation, RunMode, Shell};
use crate::test_support::{
    band_line, captions, damage, labels, offer_account, press_band, row_at, row_for, rows, showing,
    stated, theme, value_of, WIDE,
};

/// The caption of the plate stating the caller's own record.
const OWN: &str = "YOUR ACCOUNT";
/// The caption of the public roster plate.
const ROSTER: &str = "ACCOUNTS ON THIS SYSTEM";
/// The caption of the group-directory plate.
const GROUPS: &str = "GROUPS";

/// A salt fixed so a record is reproducible; the production caller draws
/// one from the kernel CSPRNG.
const SALT: Salt = [7u8; 16];

/// The group directory a machine with two user groups answers.
fn directory() -> Vec<(u32, String)> {
    alloc::vec![
        (0, "system".to_string()),
        (100, "storage".to_string()),
        (1000, "ada".to_string()),
    ]
}

/// The caller's own record, as the ungated self-scoped query answers it.
fn own_record() -> SelfAccountRecord {
    SelfAccountRecord::new(
        1000,
        1000,
        &[100],
        SelfAccountText {
            name: b"ada",
            display_name: b"Ada Lovelace",
            home: b"/Users/ada",
            shell: b"/System/Commands/elsh.app/Run",
        },
    )
    .expect("a record within the shared bounds")
}

/// One account as a listing states it.
fn account(name: &str, uid: u32, state: AccountStateCode) -> Account {
    Account {
        username: String::from(name),
        uid,
        primary_gid: 1000,
        supplementary_gids: alloc::vec![100],
        display_name: String::from("Ada Lovelace"),
        home: alloc::format!("/Users/{name}"),
        shell: String::from("/System/Commands/elsh.app/Run"),
        grants: alloc::vec![CapabilityId::FS_ACCESS],
        state,
    }
}

/// A service identity: no home, no shell, no password.
fn service(name: &str, uid: u32) -> Account {
    Account {
        username: String::from(name),
        uid,
        primary_gid: 0,
        supplementary_gids: Vec::new(),
        display_name: String::from("Device manager"),
        home: String::from("none"),
        shell: String::from("none"),
        grants: alloc::vec![CapabilityId::DRV_LOAD],
        state: AccountStateCode::NoLogin,
    }
}

/// The readings the ungated desk answers for a provisioned machine.
fn facts() -> AccountFacts {
    AccountFacts {
        own: OwnAccount::Known(alloc::boxed::Box::new(own_record())),
        users: Some(alloc::vec![
            (0, "system".to_string()),
            (1000, "ada".to_string())
        ]),
        groups: Some(directory()),
        roster: Roster::Unasked,
    }
}

/// What an authenticated `users --list` prints for `accounts`.
fn capture(accounts: &[Account]) -> Vec<u8> {
    listing::render(
        accounts,
        &[Group {
            name: String::from("storage"),
            gid: 100,
        }],
    )
    .into_bytes()
}

/// A shell on the Users pane with the ungated readings and a salt in hand.
fn showing_users() -> Shell {
    let mut shell = showing("users");
    shell.adopt_accounts(facts());
    shell.adopt_salt(Some(SALT));
    shell.lay_out(WIDE, Scale::ONE, &theme());
    shell
}

/// A shell on the Users pane with the listing already answered.
fn showing_listing(accounts: &[Account]) -> Shell {
    let mut shell = showing_users();
    press_band(&mut shell, 0);
    let _ = offer_account(&mut shell);
    shell.adopt_elevation(Elevated::Printed(0, capture(accounts)));
    shell.lay_out(WIDE, Scale::ONE, &theme());
    shell
}

/// What the plate captioned `caption` says beneath its rows.
fn footnote(shell: &Shell, caption: &str) -> String {
    let form = shell.form_for_test().expect("a composed pane");
    form.groups()
        .iter()
        .find(|plate| plate.caption() == caption)
        .and_then(|plate| plate.footnote().map(String::from))
        .unwrap_or_default()
}

/// Press Apply and offer an account for it, handing back the run asked
/// for.
fn apply(shell: &mut Shell) -> Elevation {
    press_band(shell, 1);
    offer_account(shell)
}

// --- the readings -------------------------------------------------------

#[test]
fn the_pane_composes_its_three_readings_rather_than_naming_somewhere_else() {
    let row = row_for(Pane::Users);
    assert_eq!(
        row.backing,
        PaneBacking::Composed(PaneContent::Form(Composition::Users)),
        "the pane draws its own readings"
    );
    assert_eq!(row.action(), Some("Show Accounts…"));
    assert!(
        !row.settings.is_empty(),
        "a composed pane's rows are reachable from the search field"
    );
}

#[test]
fn opening_the_pane_asks_for_the_public_readings_and_no_account() {
    let mut shell = showing("users");
    assert!(
        shell.accounts_wanted(),
        "coming on show asks for the ungated readings"
    );
    assert!(!shell.asking(), "looking at the pane asks for no account");
    shell.adopt_accounts(facts());
    assert!(
        !shell.accounts_wanted(),
        "an answered reading is not asked for again"
    );
    // A second visit re-reads, because accounts and groups come and go.
    let mut sink = damage();
    shell.go_to_pane("storage", WIDE, Scale::ONE, &theme(), &mut sink);
    shell.go_to_pane("users", WIDE, Scale::ONE, &theme(), &mut sink);
    assert!(shell.accounts_wanted(), "coming back asks again");
}

#[test]
fn your_own_record_renders_its_groups_as_names_without_any_authority() {
    let shell = showing_users();
    let (group, _) = row_at(&shell, OWN, "Primary group");
    let form = shell.form_for_test().expect("a composed pane");
    let said: Vec<String> = form.groups()[group].rows().iter().map(value_of).collect();
    assert_eq!(said[0], "ada", "the account's own name");
    assert_eq!(said[1], "Ada Lovelace");
    assert_eq!(said[2], "1000");
    // The gid the record carries, rendered through the ungated directory.
    assert_eq!(said[3], "ada");
    assert_eq!(said[4], "storage");
    assert_eq!(said[5], "/Users/ada");
    assert!(
        form.groups()[group]
            .rows()
            .iter()
            .all(|row| matches!(row.control(), FieldControl::Reading(_))),
        "a principal reads its own record and changes nothing from it here"
    );
}

#[test]
fn a_uid_no_database_holds_is_stated_apart_from_a_reading_that_failed() {
    // Two different facts: one is the machine's state, the other this
    // window's failure. Collapsing them would report the first for the
    // second.
    let mut shell = showing("users");
    shell.adopt_accounts(AccountFacts {
        own: OwnAccount::Unknown,
        ..facts()
    });
    let unknown = stated(&shell);
    shell.adopt_accounts(AccountFacts {
        own: OwnAccount::Unmeasured,
        ..facts()
    });
    let unmeasured = stated(&shell);
    assert_eq!(
        unknown[0], "this session's user id names no account in any database",
        "the machine answered, and this is what it answered"
    );
    assert_eq!(unmeasured[0], "not measured");
    assert_ne!(unknown[0], unmeasured[0]);
}

#[test]
fn a_directory_that_could_not_be_read_says_so_rather_than_reporting_none() {
    let mut shell = showing("users");
    shell.adopt_accounts(AccountFacts {
        own: OwnAccount::Unmeasured,
        users: None,
        groups: None,
        roster: Roster::Unasked,
    });
    let said = stated(&shell);
    assert!(
        said.iter()
            .any(|value| value == "not measured — the account directory could not be read"),
        "an unread account directory is not an empty machine: {said:?}"
    );
    assert!(
        said.iter()
            .any(|value| value == "not measured — the group directory could not be read"),
        "nor is an unread group directory: {said:?}"
    );
}

#[test]
fn the_pane_states_that_the_rest_is_not_public_until_it_is_asked_for() {
    let shell = showing_users();
    assert_eq!(captions(&shell), alloc::vec![OWN, ROSTER, GROUPS]);
    // The public directory is listed; what it does *not* carry is named in
    // the plate's own footnote rather than fabricated into a row.
    assert_eq!(
        footnote(&shell, ROSTER),
        "Only each account's name and user id are public. Its full name, home, shell, groups, \
         whether it may log in and what it is allowed to do are read by authenticating below."
    );
    // No lock state, no shell, no home and no grant ceiling anywhere yet.
    let shown = labels(&shell);
    assert!(
        !shown
            .iter()
            .any(|label| label == "Login" || label == "Capabilities"),
        "an ungated pane states no lock state and no ceiling: {shown:?}"
    );
    assert_eq!(
        band_line(&shell),
        "",
        "the reading band has nothing to count"
    );
}

#[test]
fn the_group_plate_is_the_ungated_directory() {
    let shell = showing_users();
    let (group, _) = row_at(&shell, GROUPS, "storage");
    let form = shell.form_for_test().expect("a composed pane");
    let said: Vec<(String, String)> = form.groups()[group]
        .rows()
        .iter()
        .map(|row| (String::from(row.label()), value_of(row)))
        .collect();
    assert_eq!(
        said,
        alloc::vec![
            ("system".to_string(), "0".to_string()),
            ("storage".to_string(), "100".to_string()),
            ("ada".to_string(), "1000".to_string()),
        ]
    );
}

// --- the authenticated listing -----------------------------------------

#[test]
fn the_reading_band_asks_for_the_non_interactive_listing() {
    let mut shell = showing_users();
    press_band(&mut shell, 0);
    let asked = offer_account(&mut shell);
    assert_eq!(asked.program, "/System/Commands/users.app/Run");
    assert_eq!(asked.argv, alloc::vec![String::from("-l")]);
    assert_eq!(
        asked.mode,
        RunMode::Capture,
        "what the run printed is the answer"
    );
}

#[test]
fn a_landed_listing_becomes_one_plate_per_account_with_its_gated_fields() {
    let shell = showing_listing(&[account("ada", 1000, AccountStateCode::Active)]);
    assert_eq!(captions(&shell), alloc::vec![OWN, "ada (1000)", GROUPS]);
    let (group, _) = row_at(&shell, "ada (1000)", "Login");
    let form = shell.form_for_test().expect("a composed pane");
    let said: Vec<(String, String)> = form.groups()[group]
        .rows()
        .iter()
        .map(|row| (String::from(row.label()), value_of(row)))
        .collect();
    assert_eq!(
        said,
        alloc::vec![
            ("Full name".to_string(), "Ada Lovelace".to_string()),
            ("Primary group".to_string(), "ada".to_string()),
            ("Other groups".to_string(), "storage".to_string()),
            ("Home folder".to_string(), "/Users/ada".to_string()),
            (
                "Shell".to_string(),
                "/System/Commands/elsh.app/Run".to_string()
            ),
            ("Login".to_string(), "active".to_string()),
            ("Capabilities".to_string(), "CAP_FS_ACCESS".to_string()),
            ("New password".to_string(), String::new()),
        ]
    );
    assert_eq!(band_line(&shell), "No changes");
}

#[test]
fn a_service_identity_offers_no_home_shell_password_or_lock() {
    // The database refuses a record shaped otherwise, so offering those
    // rows would be offering a change that can only ever be refused.
    let shell = showing_listing(&[service("devmgr", 60)]);
    let (group, _) = row_at(&shell, "devmgr (60)", "Home folder");
    let form = shell.form_for_test().expect("a composed pane");
    let plate = &form.groups()[group];
    for label in ["Home folder", "Shell", "Login", "New password"] {
        let row = plate
            .rows()
            .iter()
            .find(|row| row.label() == label)
            .expect("the row is still stated");
        assert!(
            matches!(row.control(), FieldControl::Reading(_)),
            "`{label}` is a reading on a service identity"
        );
    }
    for label in ["Full name", "Primary group", "Other groups", "Capabilities"] {
        let row = plate
            .rows()
            .iter()
            .find(|row| row.label() == label)
            .expect("the row is stated");
        assert!(
            matches!(row.control(), FieldControl::Text(_)),
            "`{label}` is settable on a service identity"
        );
    }
    assert_eq!(
        plate.footnote().unwrap_or_default(),
        "This is a service identity. It never starts a session, so it has no home, no shell and \
         no password."
    );
}

#[test]
fn a_capture_that_is_not_a_listing_is_refused_and_one_bad_line_keeps_the_rest() {
    let mut shell = showing_users();
    press_band(&mut shell, 0);
    let _ = offer_account(&mut shell);
    shell.adopt_elevation(Elevated::Printed(0, alloc::vec![0xff, 0xfe]));
    assert!(
        matches!(shell.roster_for_test(), Roster::Refused(_)),
        "bytes that are no listing at all are refused, never believed"
    );

    // Within valid text, a line the grammar does not admit is skipped so
    // one unreadable record never hides the rest.
    let mut shell = showing_users();
    press_band(&mut shell, 0);
    let _ = offer_account(&mut shell);
    let mut mixed = b"u:ada:notanumber:1000::::::a\n".to_vec();
    mixed.extend_from_slice(&capture(&[account("bob", 1001, AccountStateCode::Locked)]));
    shell.adopt_elevation(Elevated::Printed(0, mixed));
    shell.lay_out(WIDE, Scale::ONE, &theme());
    assert_eq!(captions(&shell), alloc::vec![OWN, "bob (1001)", GROUPS]);
}

#[test]
fn a_reply_too_large_for_the_seam_is_stated_and_never_asked_again_for_the_same_answer() {
    let mut shell = showing_users();
    press_band(&mut shell, 0);
    let _ = offer_account(&mut shell);
    shell.adopt_elevation(Elevated::Overran);
    assert_eq!(*shell.roster_for_test(), Roster::Overran);
    assert!(
        !shell.asking(),
        "the question came down: asking again answers the same"
    );
    assert!(
        footnote(&shell, ROSTER).contains("too large to show here"),
        "the pane says what happened, beside the names it does hold"
    );
    // And it still shows the public directory, which it did read.
    assert!(
        stated(&shell).iter().any(|said| said == "1000"),
        "a refused gated half never hides the public one"
    );
}

#[test]
fn a_refused_account_leaves_the_pane_as_it_was_and_states_why() {
    let mut shell = showing_users();
    press_band(&mut shell, 0);
    let _ = offer_account(&mut shell);
    shell.adopt_elevation(Elevated::Refused(ElevateRefusal::Credentials));
    assert_eq!(
        *shell.roster_for_test(),
        Roster::Unasked,
        "nothing was read, so nothing is shown"
    );
    assert!(shell.asking(), "the question stays up to be corrected");
}

#[test]
fn leaving_the_pane_drops_the_listing() {
    // Every account's home, shell, lock state and capability ceiling is a
    // privileged reading; one held while the reader browses elsewhere is
    // one this application had no business keeping.
    let mut shell = showing_listing(&[account("ada", 1000, AccountStateCode::Active)]);
    assert!(matches!(shell.roster_for_test(), Roster::Listed(_)));
    let mut sink = damage();
    shell.go_to_pane("storage", WIDE, Scale::ONE, &theme(), &mut sink);
    assert_eq!(*shell.roster_for_test(), Roster::Unasked);
}

#[test]
fn a_listing_that_lands_after_the_window_has_left_the_pane_is_dropped() {
    // The desktop can send the window elsewhere while a run is in flight,
    // and a privileged reading installed for a surface that never asked
    // for it is exactly what routing by "whatever is showing" would do.
    let mut shell = showing_users();
    press_band(&mut shell, 0);
    let _ = offer_account(&mut shell);
    let mut sink = damage();
    assert!(shell.go_to_pane("storage", WIDE, Scale::ONE, &theme(), &mut sink));
    shell.adopt_elevation(Elevated::Printed(
        0,
        capture(&[account("ada", 1000, AccountStateCode::Active)]),
    ));
    assert_eq!(
        *shell.roster_for_test(),
        Roster::Unasked,
        "the capture was dropped, not installed on the pane now showing"
    );
}

#[test]
fn an_addressing_capture_never_lands_on_the_users_pane() {
    // The same rule the other way round: two panes drive the same seam and
    // each capture is adopted only by the pane that asked for it.
    let mut shell = showing("ethernet");
    press_band(&mut shell, 0);
    let _ = offer_account(&mut shell);
    let mut sink = damage();
    assert!(shell.go_to_pane("users", WIDE, Scale::ONE, &theme(), &mut sink));
    shell.adopt_elevation(Elevated::Printed(0, b"wan.ipv4.method dhcp\n".to_vec()));
    assert_eq!(*shell.roster_for_test(), Roster::Unasked);
    assert!(
        shell.addressing_for_test().document().is_none(),
        "and it did not install itself behind the pane it left either"
    );
}

// --- staging and applying -----------------------------------------------

#[test]
fn one_accounts_fields_apply_as_one_usermod_run() {
    let mut shell = showing_listing(&[account("ada", 1000, AccountStateCode::Active)]);
    let (group, row) = row_at(&shell, "ada (1000)", "Full name");
    assert!(shell.type_for_test(group, row, "Ada B Lovelace"));
    let (group, row) = row_at(&shell, "ada (1000)", "Home folder");
    assert!(shell.type_for_test(group, row, "/Users/ada2"));
    assert_eq!(band_line(&shell), "2 changes not applied");

    let asked = apply(&mut shell);
    assert_eq!(asked.program, "/System/Commands/usermod.app/Run");
    assert_eq!(
        asked.argv,
        alloc::vec![
            String::from("--comment"),
            String::from("Ada B Lovelace"),
            String::from("--home"),
            String::from("/Users/ada2"),
            String::from("--"),
            String::from("ada"),
        ],
        "one invocation, so the fields are never split across two runs"
    );
    assert_eq!(asked.mode, RunMode::Wait);
}

#[test]
fn the_lock_row_spells_its_two_settable_states_as_usermods_own_switches() {
    let mut shell = showing_listing(&[account("ada", 1000, AccountStateCode::Active)]);
    let (group, row) = row_at(&shell, "ada (1000)", "Login");
    // The second choice is `locked`; the third is offered only where it is
    // already in effect.
    assert!(shell.choose_for_test(group, row, 1));
    let asked = apply(&mut shell);
    assert_eq!(
        asked.argv,
        alloc::vec![
            String::from("--lock"),
            String::from("--"),
            String::from("ada")
        ]
    );

    let mut shell = showing_listing(&[account("bob", 1001, AccountStateCode::Locked)]);
    let (group, row) = row_at(&shell, "bob (1001)", "Login");
    assert!(shell.choose_for_test(group, row, 0));
    let asked = apply(&mut shell);
    assert_eq!(
        asked.argv,
        alloc::vec![
            String::from("--unlock"),
            String::from("--"),
            String::from("bob")
        ]
    );
}

#[test]
fn a_service_identitys_own_state_is_read_rather_than_offered() {
    let shell = showing_listing(&[service("devmgr", 60)]);
    let (group, _) = row_at(&shell, "devmgr (60)", "Login");
    let form = shell.form_for_test().expect("a composed pane");
    let row = form.groups()[group]
        .rows()
        .iter()
        .find(|row| row.label() == "Login")
        .expect("the row");
    assert_eq!(
        value_of(row),
        "nologin",
        "a reading of what it is, because no tool can set it"
    );

    // A settable lock row offers exactly the two `usermod` spells: a list
    // carrying the third would be offering a change that can only ever be
    // refused.
    let mut lockable = showing_listing(&[account("ada", 1000, AccountStateCode::Active)]);
    let (group, row) = row_at(&lockable, "ada (1000)", "Login");
    let form = lockable.form_for_test().expect("a composed pane");
    let FieldControl::Combo(combo) = form.groups()[group].rows()[row].control() else {
        panic!("a settable lock state is a choice list");
    };
    assert_eq!(
        combo.choices(),
        &[String::from("active"), String::from("locked")]
    );
    assert!(
        !lockable.choose_for_test(group, row, 2),
        "an index past the list this surface built stages nothing"
    );
}

#[test]
fn the_group_rows_are_read_by_name_and_applied_by_number() {
    let mut shell = showing_listing(&[account("ada", 1000, AccountStateCode::Active)]);
    let (group, row) = row_at(&shell, "ada (1000)", "Other groups");
    // Names, numbers, and a mixture of the two all resolve; a word that is
    // neither does not.
    assert!(shell.type_for_test(group, row, "storage,1000"));
    let asked = apply(&mut shell);
    assert_eq!(
        asked.argv,
        alloc::vec![
            String::from("--groups"),
            String::from("100,1000"),
            String::from("--"),
            String::from("ada"),
        ]
    );
}

#[test]
fn a_value_the_store_would_refuse_marks_its_row_and_blocks_the_apply() {
    let mut shell = showing_listing(&[account("ada", 1000, AccountStateCode::Active)]);
    let (group, row) = row_at(&shell, "ada (1000)", "Home folder");
    // A relative path is not storable, and neither is a group nobody holds.
    assert!(shell.type_for_test(group, row, "Users/ada"));
    let form = shell.form_for_test().expect("a composed pane");
    assert_eq!(
        form.groups()[group].rows()[row].state().validation,
        ValidationState::Invalid
    );
    assert_eq!(band_line(&shell), "1 value this cannot be saved with");
    press_band(&mut shell, 1);
    assert!(
        !shell.asking(),
        "a refused value is corrected or reverted, never applied around"
    );

    let (group, row) = row_at(&shell, "ada (1000)", "Capabilities");
    assert!(shell.type_for_test(group, row, "CAP_NOT_A_CAPABILITY"));
    let form = shell.form_for_test().expect("a composed pane");
    assert_eq!(
        form.groups()[group].rows()[row].state().validation,
        ValidationState::Invalid,
        "a ceiling is not applied approximately"
    );
}

#[test]
fn a_change_spanning_two_accounts_is_refused_before_a_password_is_typed() {
    // One command changes one account, so a change split across two runs
    // could leave half of it durable.
    let mut shell = showing_listing(&[
        account("ada", 1000, AccountStateCode::Active),
        account("bob", 1001, AccountStateCode::Active),
    ]);
    let (group, row) = row_at(&shell, "ada (1000)", "Full name");
    assert!(shell.type_for_test(group, row, "Ada B Lovelace"));
    let (group, row) = row_at(&shell, "bob (1001)", "Full name");
    assert!(shell.type_for_test(group, row, "Bobby Tables"));
    press_band(&mut shell, 1);
    assert!(!shell.asking(), "nothing is run");
    assert_eq!(
        band_line(&shell),
        "Changes to more than one account. One command changes one account, so apply them one \
         account at a time."
    );
}

#[test]
fn a_password_beside_other_fields_is_refused_for_the_same_reason() {
    let mut shell = showing_listing(&[account("ada", 1000, AccountStateCode::Active)]);
    let (group, row) = row_at(&shell, "ada (1000)", "Full name");
    assert!(shell.type_for_test(group, row, "Ada B Lovelace"));
    let (group, row) = row_at(&shell, "ada (1000)", "New password");
    assert!(shell.type_for_test(group, row, "correct horse"));
    press_band(&mut shell, 1);
    assert!(!shell.asking());
    assert_eq!(
        band_line(&shell),
        "A password and other fields together. The password is set by its own command, so apply \
         it on its own."
    );
}

#[test]
fn a_password_is_hashed_here_and_only_the_record_rides_the_command_line() {
    let mut shell = showing_listing(&[account("ada", 1000, AccountStateCode::Active)]);
    let (group, row) = row_at(&shell, "ada (1000)", "New password");
    assert!(shell.type_for_test(group, row, "correct horse"));
    assert_eq!(band_line(&shell), "1 change not applied");
    let asked = apply(&mut shell);
    assert_eq!(asked.program, "/System/Commands/passwd.app/Run");
    assert_eq!(asked.argv.len(), 4);
    assert_eq!(asked.argv[0], "--record");
    assert_eq!(asked.argv[2], "--");
    assert_eq!(asked.argv[3], "ada");
    let record = PasswordRecord::decode(&asked.argv[1]).expect("the tool's own decoder takes it");
    assert!(
        record.verify(b"correct horse"),
        "the record is of the password that was typed"
    );
    assert!(
        !asked.argv.iter().any(|arg| arg.contains("correct horse")),
        "no plaintext rides the command line: {:?}",
        asked.argv
    );
}

#[test]
fn a_password_is_held_in_its_masked_entry_and_in_no_staged_copy() {
    let mut shell = showing_listing(&[account("ada", 1000, AccountStateCode::Active)]);
    let (group, row) = row_at(&shell, "ada (1000)", "New password");
    let form = shell.form_for_test().expect("a composed pane");
    let FieldControl::Text(entry) = form.groups()[group].rows()[row].control() else {
        panic!("the password row is an entry");
    };
    assert!(
        entry.is_secret(),
        "a visible field would be shoulder-surfable"
    );
    assert!(shell.type_for_test(group, row, "correct horse"));
    let form = shell.form_for_test().expect("a composed pane");
    assert!(
        !form
            .staged_accounts()
            .iter()
            .any(|(_, value)| value.contains("correct horse")),
        "a plaintext in a growable string is a copy no wipe can reach"
    );
}

#[test]
fn a_password_longer_than_the_record_will_hash_is_refused_on_its_row() {
    let mut shell = showing_listing(&[account("ada", 1000, AccountStateCode::Active)]);
    let (group, row) = row_at(&shell, "ada (1000)", "New password");
    // The entry bounds itself in *characters*, so an ASCII password can
    // never reach the record's byte bound through it.
    let ascii = "x".repeat(MAX_PASSWORD_LEN + 1);
    assert!(shell.type_for_test(group, row, &ascii));
    let form = shell.form_for_test().expect("a composed pane");
    assert_eq!(
        value_of(&form.groups()[group].rows()[row]).len(),
        MAX_PASSWORD_LEN,
        "the entry took what it could hold"
    );
    assert_eq!(
        form.groups()[group].rows()[row].state().validation,
        ValidationState::Valid
    );
    // A script that needs more than one byte a character does reach it,
    // and the row says so rather than leaving the tool to refuse a
    // password the reader has already typed.
    let wide = "é".repeat(MAX_PASSWORD_LEN);
    assert!(shell.type_for_test(group, row, &wide));
    let form = shell.form_for_test().expect("a composed pane");
    assert_eq!(
        form.groups()[group].rows()[row].state().validation,
        ValidationState::Invalid,
        "the bound the record enforces is stated on the row, not by the tool afterwards"
    );
}

#[test]
fn a_password_apply_with_no_randomness_refuses_rather_than_salting_predictably() {
    let mut shell = showing_listing(&[account("ada", 1000, AccountStateCode::Active)]);
    shell.adopt_salt(None);
    let (group, row) = row_at(&shell, "ada (1000)", "New password");
    assert!(shell.type_for_test(group, row, "correct horse"));
    press_band(&mut shell, 1);
    assert!(!shell.asking(), "nothing is hashed and nothing is run");
    assert_eq!(
        band_line(&shell),
        "No randomness was available to salt the new password with, so it was not hashed and \
         nothing was sent."
    );
}

#[test]
fn a_password_apply_spends_the_salt_and_asks_for_another() {
    let mut shell = showing_listing(&[account("ada", 1000, AccountStateCode::Active)]);
    assert!(shell.has_salt_for_test());
    let (group, row) = row_at(&shell, "ada (1000)", "New password");
    assert!(shell.type_for_test(group, row, "correct horse"));
    let _ = apply(&mut shell);
    assert!(
        !shell.has_salt_for_test(),
        "a salt is used once, so the next password is hashed under a fresh one"
    );
    assert!(shell.salt_wanted(), "and the caller is asked for it");
}

#[test]
fn reverting_puts_every_row_back_and_erases_the_password_entry() {
    let mut shell = showing_listing(&[account("ada", 1000, AccountStateCode::Active)]);
    let (group, row) = row_at(&shell, "ada (1000)", "Full name");
    assert!(shell.type_for_test(group, row, "Ada B Lovelace"));
    let (secret_group, secret_row) = row_at(&shell, "ada (1000)", "New password");
    assert!(shell.type_for_test(secret_group, secret_row, "correct horse"));
    assert_eq!(band_line(&shell), "2 changes not applied");

    press_band(&mut shell, 0);
    assert_eq!(band_line(&shell), "No changes");
    let form = shell.form_for_test().expect("a composed pane");
    assert_eq!(value_of(&form.groups()[group].rows()[row]), "Ada Lovelace");
    assert_eq!(
        value_of(&form.groups()[secret_group].rows()[secret_row]),
        String::new(),
        "a reverted pane holds no password"
    );
}

#[test]
fn a_public_reading_landing_keeps_the_listing_and_a_password_being_typed() {
    // The directories are free and re-read on their own schedule; one
    // landing must not throw away a listing that cost a password, nor a
    // secret the reader is halfway through.
    let mut shell = showing_listing(&[account("ada", 1000, AccountStateCode::Active)]);
    let (group, row) = row_at(&shell, "ada (1000)", "New password");
    assert!(shell.type_for_test(group, row, "correct horse"));
    shell.adopt_accounts(AccountFacts {
        groups: Some(alloc::vec![(100, "storage".to_string())]),
        ..facts()
    });
    assert!(
        matches!(shell.roster_for_test(), Roster::Listed(_)),
        "the listing survives a public reading"
    );
    let (group, row) = row_at(&shell, "ada (1000)", "New password");
    let form = shell.form_for_test().expect("a composed pane");
    assert_eq!(
        value_of(&form.groups()[group].rows()[row]),
        "correct horse",
        "and so does the entry the reader is typing into"
    );
}

#[test]
fn an_applied_change_moves_the_listing_on_and_leaves_the_reader_in_place() {
    let mut shell = showing_listing(&[account("ada", 1000, AccountStateCode::Active)]);
    let (group, row) = row_at(&shell, "ada (1000)", "Full name");
    assert!(shell.type_for_test(group, row, "Ada B Lovelace"));
    let _ = apply(&mut shell);
    shell.adopt_elevation(Elevated::Finished(0));
    assert_eq!(band_line(&shell), "Applied");
    // The tool applied every field it was given or refused the run, so the
    // listing holds the change: re-reading it would cost a second password
    // for an answer already known, and dropping it would leave the pane
    // with no rows and a band offering Revert.
    let form = shell.form_for_test().expect("a composed pane");
    assert_eq!(
        value_of(&form.groups()[group].rows()[row]),
        "Ada B Lovelace"
    );
    assert_eq!(form.changes(), 0, "and nothing is left staged");
    assert!(
        shell.accounts_wanted(),
        "the public readings are free, so they are taken again at once"
    );
    // A second change needs no second reading, only a second authentication.
    assert!(shell.type_for_test(group, row, "Ada Lovelace"));
    assert_eq!(band_line(&shell), "1 change not applied");
}

#[test]
fn an_applied_password_leaves_no_secret_behind() {
    let mut shell = showing_listing(&[account("ada", 1000, AccountStateCode::Active)]);
    let (group, row) = row_at(&shell, "ada (1000)", "New password");
    assert!(shell.type_for_test(group, row, "correct horse"));
    let _ = apply(&mut shell);
    shell.adopt_elevation(Elevated::Finished(0));
    let form = shell.form_for_test().expect("a composed pane");
    assert_eq!(
        value_of(&form.groups()[group].rows()[row]),
        String::new(),
        "a password now in effect is one this window has no reason to hold"
    );
    assert_eq!(form.changes(), 0);
}

#[test]
fn a_refused_run_leaves_the_change_staged_so_it_can_be_corrected() {
    let mut shell = showing_listing(&[account("ada", 1000, AccountStateCode::Active)]);
    let (group, row) = row_at(&shell, "ada (1000)", "Full name");
    assert!(shell.type_for_test(group, row, "Ada B Lovelace"));
    let _ = apply(&mut shell);
    shell.adopt_elevation(Elevated::Refused(ElevateRefusal::NotRun(String::from(
        "the kernel would not widen that grant",
    ))));
    assert_eq!(band_line(&shell), "the kernel would not widen that grant");
    let form = shell.form_for_test().expect("a composed pane");
    assert_eq!(
        value_of(&form.groups()[group].rows()[row]),
        "Ada B Lovelace",
        "the reader corrects their change rather than retyping it"
    );
}

#[test]
fn the_pane_draws_in_both_themes_and_at_both_densities() {
    // Every plate of it, listing and all: a density or a theme this
    // surface could not draw a privileged reading at is a pane a reader
    // cannot use.
    for scale in [Scale::ONE, Scale::from_percent(200).expect("a scale")] {
        for palette in [Theme::dark(), Theme::light()] {
            install_test_transport();
            let mut shell = Shell::new(DesktopSettings::default()).expect("a registry");
            let mut sink = damage();
            assert!(shell.go_to_pane("users", WIDE, scale, &palette, &mut sink));
            shell.adopt_accounts(facts());
            shell.adopt_salt(Some(SALT));
            shell.lay_out(WIDE, scale, &palette);
            let mut surface = Surface::new(WIDE.width, WIDE.height).expect("a surface");
            shell.render(&mut surface, WIDE, scale, &palette, &mut NoArtwork);
            assert!(
                !rows(&shell).is_empty(),
                "the pane states its readings at every density"
            );
        }
    }
}
