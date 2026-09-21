//! The Users & Groups pane's three readings, and the one elevated run a
//! staged account change becomes.
//!
//! The fourth store this surface edits, and the only one read at three
//! different authorities at once. A principal may read *its own* record and
//! the public uid/gid-to-name directories without holding anything; every
//! other account's fields, its lock state and its capability ceiling are
//! `CAP_USER_ADMIN`'s, so they arrive only as the relayed output of a
//! `users` run an administrator authorised — the same elevated-read seam the
//! networking panes state their addressing through.
//!
//! The plates are therefore *discovered* from that listing rather than
//! declared in a table here, exactly as an interface's are: which accounts
//! exist is what the run answers, and the pane is built before anyone has
//! asked. How many there can be is bounded by the seam rather than by this
//! surface: a reply larger than the supervisor carries answers `Overran`,
//! so the pane either holds a listing small enough to draw or says plainly
//! that it holds none.
//!
//! Nothing here performs I/O or holds authority. A row reports the value the
//! reader typed or chose; the shell turns the difference into the one
//! elevated command that writes it, and the kernel decides whether the
//! authenticated account may.

use alloc::string::String;
use alloc::vec::Vec;

use tairix_abi::sysinfo::SelfAccountRecord;
use tairix_abi::users_admin::AccountStateCode;
use tairix_controls::{
    ComboBox, ControlState, FieldControl, FieldGroup, FieldRow, TextField, ValidationState,
};
use tairix_useradmin::listing::{known_capability, Listing};
use tairix_useradmin::{render_grants, state_word, Account};
use tairix_users::{
    valid_display_name, valid_path, Salt, MAX_PASSWORD_LEN, MAX_SUPPLEMENTARY_GIDS, NO_PATH_MARKER,
};

/// What this pane contributes to the search index.
///
/// The three subjects a reader searches for rather than one term per
/// account: which accounts exist is what an authenticated run answers, and
/// the index is built before anyone has asked.
pub(crate) const ACCOUNT_FACTS: &[&str] = &["Your account", "Accounts", "Groups"];

/// The caption of the plate stating the caller's own record.
const OWN_CAPTION: &str = "YOUR ACCOUNT";

/// The caption of the plate stating the roster, or why there is none.
const ROSTER_CAPTION: &str = "ACCOUNTS ON THIS SYSTEM";

/// The caption of the plate stating the group directory.
const GROUPS_CAPTION: &str = "GROUPS";

/// The label every roster row carries before a listing has landed.
const ACCOUNT_LABEL: &str = "Account";

/// What a reading states when the caller could not take it.
const UNMEASURED: &str = "not measured";

/// What the own-account plate states for a uid no account database holds.
///
/// Distinct from a failed reading: the query answered, and what it answered
/// was that this uid names no account. A session running as an identity the
/// database does not carry is worth saying plainly.
const NO_OWN_ACCOUNT: &str = "this session's user id names no account in any database";

/// What the pane states before anyone has asked for the listing.
///
/// Only each account's name and user id are public, so this is what the
/// plate says it is *missing* rather than a row it fabricates.
const NOT_ASKED: &str = "Only each account's name and user id are public. Its full name, home, \
                         shell, groups, whether it may log in and what it is allowed to do are \
                         read by authenticating below.";

/// What it states when the listing is larger than the reply carries.
const TOO_LARGE: &str = "too large to show here — run `users list` from a shell to read it in full";

/// What it states when the run answered something that is not a listing.
const UNREADABLE_LISTING: &str = "the command answered something this window could not read";

/// What it states when the listing itself named no account.
///
/// Distinct from nobody having asked: the run happened and this is what it
/// answered, which on a provisioned machine means something is wrong with
/// the database rather than with the reading.
const NO_LISTED_ACCOUNTS: &str = "the listing named no account at all";

/// What the roster plate states when no account directory could be read.
const NO_DIRECTORY: &str = "not measured — the account directory could not be read";

/// What the groups plate states when no group directory could be read.
const NO_GROUP_DIRECTORY: &str = "not measured — the group directory could not be read";

/// What the groups plate states when the machine holds no group at all.
const NO_GROUPS: &str = "none — this machine holds no group";

/// What the roster plate states when the machine holds no account at all.
const NO_ACCOUNTS: &str = "none — this machine holds no account";

/// The footnote beneath a service identity's plate.
const NOLOGIN_FOOTNOTE: &str = "This is a service identity. It never starts a session, so it has \
                                no home, no shell and no password.";

/// What an entry shows in place of a value the account does not carry.
const UNSET: &str = "not set";

/// The tool that owns every account field but the password.
const USERMOD_RUN_PATH: &str = "/System/Commands/usermod.app/Run";

/// The tool that owns an account's password.
const PASSWD_RUN_PATH: &str = "/System/Commands/passwd.app/Run";

/// The caller's own account, which is three different facts rather than an
/// [`Option`] of one.
///
/// A reading that has not landed, a uid the databases do not carry, and a
/// record are three answers a reader needs told apart: the first is this
/// window's failure, the second is the machine's state, and only the third
/// is an account.
#[derive(Clone, Debug, Default, Eq, PartialEq)]
pub enum OwnAccount {
    /// The read has not landed, or could not be taken.
    #[default]
    Unmeasured,
    /// The uid the kernel attested holds no record in any database.
    Unknown,
    /// The record the service answered for the attested uid.
    ///
    /// Boxed because the frame is inline and large: the readings this
    /// variant is held inside are cloned on every rebuild, and an enum
    /// sized to its largest variant would carry the whole frame through
    /// each of them.
    Known(alloc::boxed::Box<SelfAccountRecord>),
}

/// What the pane knows of the accounts only an administrator may read.
///
/// Mirrors the networking panes' addressing exactly, and for the same
/// reason: the reading is privileged, so it arrives as the relayed output
/// of an authenticated run or not at all, and each way of not arriving is
/// a different thing to say.
#[derive(Clone, Debug, Default, Eq, PartialEq)]
pub enum Roster {
    /// Nobody has asked yet. The pane states what it would take.
    #[default]
    Unasked,
    /// The listing the run stated, which is what every account plate is
    /// discovered from and what a staged change is measured against.
    Listed(Listing),
    /// The run happened, but printed more than the seam carries.
    Overran,
    /// Nothing was read, and this is why.
    Refused(String),
}

impl Roster {
    /// The roster a `users --list` capture states.
    ///
    /// The line form and its parser are one definition in `lib/useradmin`,
    /// so the tool that printed the lines and this surface cannot disagree
    /// about what a field means. Output that is no listing at all is a
    /// refusal; a single line the grammar does not admit is skipped by the
    /// parser, so one unreadable record never hides the rest.
    #[must_use]
    pub fn from_capture(output: &[u8]) -> Self {
        tairix_useradmin::listing::parse(output).map_or_else(
            || Self::Refused(String::from(UNREADABLE_LISTING)),
            Self::Listed,
        )
    }

    /// The accounts this roster states, which is none until a listing has
    /// landed.
    pub(crate) fn accounts(&self) -> &[Account] {
        match self {
            Self::Listed(listing) => &listing.accounts,
            Self::Unasked | Self::Overran | Self::Refused(_) => &[],
        }
    }

    /// The account at `index` of the listing the plates were discovered
    /// from.
    pub(crate) fn account(&self, index: usize) -> Option<&Account> {
        self.accounts().get(index)
    }

    /// What the pane says instead of the account plates, or `None` once
    /// there are plates to draw.
    fn statement(&self) -> Option<&str> {
        match self {
            Self::Unasked => Some(NOT_ASKED),
            Self::Overran => Some(TOO_LARGE),
            Self::Refused(reason) => Some(reason.as_str()),
            Self::Listed(listing) if listing.accounts.is_empty() => Some(NO_LISTED_ACCOUNTS),
            Self::Listed(_) => None,
        }
    }
}

/// The account readings the Users pane draws, as the caller answered them.
///
/// Each directory is an [`Option`] because a walk that failed is not an
/// empty machine: a pane that reported "no accounts" for a reading it could
/// not take would be inventing the answer.
#[derive(Clone, Debug, Default, Eq, PartialEq)]
pub struct AccountFacts {
    /// The caller's own record, read ungated against the uid the kernel
    /// attested.
    pub own: OwnAccount,
    /// The ungated account directory: every account's uid and name.
    pub users: Option<Vec<(u32, String)>>,
    /// The ungated group directory: every group's gid and name.
    pub groups: Option<Vec<(u32, String)>>,
    /// What an administrator-authenticated listing answered.
    pub roster: Roster,
}

impl AccountFacts {
    /// The group directory as a slice, for a row builder that only reads
    /// it.
    pub(crate) fn groups_slice(&self) -> Option<&[(u32, String)]> {
        self.groups.as_deref()
    }

    /// Write every staged change into the listing this holds, after a run
    /// that exited cleanly.
    ///
    /// The listing stays and moves on rather than being dropped and
    /// re-read: re-reading costs another password, and `usermod` applied
    /// every field it was given or refused the run, so a clean exit stored
    /// exactly what was staged. An account index the listing no longer
    /// holds is skipped.
    pub(crate) fn adopt_applied(&mut self, staged: &[(AccountSetting, String)]) {
        let groups = self.groups.as_deref();
        let Roster::Listed(listing) = &mut self.roster else {
            return;
        };
        for (setting, value) in staged {
            if let Some(account) = listing.accounts.get_mut(setting.account) {
                setting.field.apply_to(account, value, groups);
            }
        }
    }

    /// Take `public`'s readings, keeping the listing this one holds.
    ///
    /// The listing cost a password and the readings beside it are free and
    /// re-read on their own schedule, so one landing must never throw the
    /// other away. One definition, because the window and the form it
    /// built both hold a copy and a second spelling would drift.
    pub(crate) fn adopt_public(&mut self, public: Self) {
        let roster = core::mem::take(&mut self.roster);
        *self = public;
        self.roster = roster;
    }
}

/// One editable field of one account.
///
/// Exactly the fields the account tools spell, so a row cannot offer a
/// change no tool can carry out.
#[derive(Copy, Clone, Debug, Eq, PartialEq)]
pub(crate) enum AccountField {
    /// The account comment: the person's name as it is shown.
    DisplayName,
    /// The home directory.
    Home,
    /// The login shell.
    Shell,
    /// The primary group.
    PrimaryGroup,
    /// The whole supplementary group set.
    Groups,
    /// Whether the account may log in.
    Lock,
    /// The capability grant ceiling.
    Grants,
    /// A replacement password.
    Password,
}

/// Every field an account plate lists, in the order it lists them.
const FIELDS: [AccountField; 8] = [
    AccountField::DisplayName,
    AccountField::PrimaryGroup,
    AccountField::Groups,
    AccountField::Home,
    AccountField::Shell,
    AccountField::Lock,
    AccountField::Grants,
    AccountField::Password,
];

impl AccountField {
    /// The reader's name for this field.
    pub(crate) const fn label(self) -> &'static str {
        match self {
            Self::DisplayName => "Full name",
            Self::Home => "Home folder",
            Self::Shell => "Shell",
            Self::PrimaryGroup => "Primary group",
            Self::Groups => "Other groups",
            Self::Lock => "Login",
            Self::Grants => "Capabilities",
            Self::Password => "New password",
        }
    }

    /// The sentence beneath the label: what this field actually decides.
    const fn purpose(self) -> &'static str {
        match self {
            Self::DisplayName => "The person's name, as it is shown beside the account.",
            Self::Home => "Where this account's own files live. An absolute path.",
            Self::Shell => "The program a login session starts. An absolute path.",
            Self::PrimaryGroup => "The group files this account creates belong to.",
            Self::Groups => {
                "Every other group this account is a member of, by name or by number, separated \
                 by commas. Empty leaves it in none."
            }
            Self::Lock => "Whether this account may start a session.",
            Self::Grants => {
                "The capability ceiling every program this account runs is bounded by, separated \
                 by commas. Empty is an account that confers none. No account can be granted \
                 more than the administrator applying the change already holds."
            }
            Self::Password => {
                "Leave empty to keep the current password. It is hashed in this window, so the \
                 password itself never leaves it."
            }
        }
    }

    /// Whether this field is a secret, and so drawn masked and never held
    /// anywhere but its own self-erasing entry.
    pub(crate) const fn is_secret(self) -> bool {
        matches!(self, Self::Password)
    }

    /// Whether `state` can hold this field at all.
    ///
    /// A service identity carries no home, no shell and no password, and
    /// cannot be barred from a session it never starts; the database
    /// refuses a record shaped otherwise. Offering those rows as controls
    /// would be offering a change that can only ever be refused, so they
    /// are readings on such an account and the plate says why.
    const fn settable_on(self, state: AccountStateCode) -> bool {
        match state {
            AccountStateCode::Active | AccountStateCode::Locked => true,
            AccountStateCode::NoLogin => {
                !matches!(self, Self::Home | Self::Shell | Self::Password | Self::Lock)
            }
        }
    }

    /// What `account` holds for this field, spelled as the row shows it and
    /// takes it back.
    ///
    /// One spelling for both directions, so a value the reader did not
    /// touch can never read as a change.
    fn held(self, account: &Account, groups: Option<&[(u32, String)]>) -> String {
        match self {
            Self::DisplayName => account.display_name.clone(),
            Self::Home => account.home.clone(),
            Self::Shell => account.shell.clone(),
            Self::PrimaryGroup => group_word(account.primary_gid, groups),
            Self::Groups => render_gids(&account.supplementary_gids, groups),
            Self::Lock => String::from(state_word(account.state)),
            Self::Grants => render_grants(&account.grants),
            // A password is never read back, so there is nothing an entry
            // could show and an empty one is what "unchanged" looks like.
            Self::Password => String::new(),
        }
    }

    /// Write `value` into `account`, `held`'s inverse.
    ///
    /// What a pane does with a run that exited cleanly: `usermod` replaces
    /// the whole field set or refuses, and both sides spell a field the
    /// same way, so a clean exit stored exactly this. The two directions
    /// sit together here so neither can drift from the other unnoticed. A
    /// password is not among them — the listing carries none, so there is
    /// nothing to write back.
    fn apply_to(self, account: &mut Account, value: &str, groups: Option<&[(u32, String)]>) {
        match self {
            Self::DisplayName => account.display_name = String::from(value),
            Self::Home => account.home = String::from(value),
            Self::Shell => account.shell = String::from(value),
            Self::PrimaryGroup => {
                if let Some(gid) = resolve_gid(value, groups) {
                    account.primary_gid = gid;
                }
            }
            Self::Groups => {
                if let Some(gids) = resolve_gids(value, groups) {
                    account.supplementary_gids = gids;
                }
            }
            Self::Lock => {
                if let Some((_, code)) = LOCK_STATES.iter().find(|(word, _)| *word == value) {
                    account.state = *code;
                }
            }
            Self::Grants => {
                if let Ok(grants) = tairix_useradmin::parse_grants(value) {
                    account.grants = grants;
                }
            }
            Self::Password => {}
        }
    }

    /// Whether the store would take `value` for this field, where empty is
    /// whatever emptiness means for it.
    pub(crate) fn admits(self, value: &str, groups: Option<&[(u32, String)]>) -> bool {
        match self {
            Self::DisplayName => valid_display_name(value),
            Self::Home | Self::Shell => value == NO_PATH_MARKER || valid_path(value),
            Self::PrimaryGroup => resolve_gid(value, groups).is_some(),
            Self::Groups => resolve_gids(value, groups).is_some(),
            Self::Lock => LOCK_STATES.iter().any(|(word, _)| *word == value),
            Self::Grants => value
                .split(',')
                .filter(|word| !word.is_empty())
                .all(known_capability),
            // The record bounds the password it will derive a hash from, so
            // an over-long offering is refused on the row rather than by
            // the tool after a password has been typed.
            Self::Password => value.len() <= MAX_PASSWORD_LEN,
        }
    }
}

/// Every lock state an account may hold, with the word each is spelled by.
///
/// The spelling is the shared one every account tool prints, so a row and
/// the `users list` table cannot name the same state differently. All three
/// are *read*; only the first two are offered as a choice, because a
/// service identity's state is not settable at all and so never reaches a
/// list.
const LOCK_STATES: [(&str, AccountStateCode); 3] = [
    ("active", AccountStateCode::Active),
    ("locked", AccountStateCode::Locked),
    ("nologin", AccountStateCode::NoLogin),
];

/// How many of [`LOCK_STATES`] a row offers: the two `usermod` spells.
const SETTABLE_LOCK_STATES: usize = 2;

/// The lock states a row offers.
fn lock_choices() -> Vec<String> {
    LOCK_STATES[..SETTABLE_LOCK_STATES]
        .iter()
        .map(|(word, _)| String::from(*word))
        .collect()
}

/// One field of one account: which account, by its index in the captured
/// listing, and which of its fields.
///
/// An index rather than the name itself because the owner table a form
/// keeps beside its rows is `Copy`; the name it stands for is read back
/// from the listing the plates were discovered from.
#[derive(Copy, Clone, Debug, Eq, PartialEq)]
pub(crate) struct AccountSetting {
    /// Which account of the captured listing.
    pub(crate) account: usize,
    /// Which of its fields.
    pub(crate) field: AccountField,
}

/// The one elevated run a staged account change becomes.
pub(crate) struct AccountRun {
    /// The absolute path of the tool that owns the change.
    pub(crate) program: &'static str,
    /// The arguments to hand it, in the order its own command line takes
    /// them.
    pub(crate) argv: Vec<String>,
}

/// Why a staged change is not one elevated run, and so cannot be applied.
///
/// The seam carries one program and one argv, and a change split over two
/// runs can leave half of it durable — so each of these is stated before
/// anyone is asked for a password, rather than discovered after.
#[derive(Copy, Clone, Debug, Eq, PartialEq)]
pub(crate) enum Unappliable {
    /// Rows on more than one account are staged.
    ManyAccounts,
    /// One account, but the change needs both tools at once.
    TwoTools,
    /// A password is staged and no randomness was available to salt it
    /// with.
    NoRandomness,
    /// A staged value reached the argv builder that no tool can carry,
    /// which is a routing defect rather than anything the reader did.
    Unspellable,
}

impl Unappliable {
    /// What the band says about this.
    pub(crate) const fn reason(self) -> &'static str {
        match self {
            Self::ManyAccounts => {
                "Changes to more than one account. One command changes one account, so apply them \
                 one account at a time."
            }
            Self::TwoTools => {
                "A password and other fields together. The password is set by its own command, so \
                 apply it on its own."
            }
            Self::NoRandomness => {
                "No randomness was available to salt the new password with, so it was not hashed \
                 and nothing was sent."
            }
            Self::Unspellable => "This change cannot be expressed as a command.",
        }
    }
}

/// The plate stating the caller's own record, rendered through the ungated
/// directories so a gid reads as a group.
pub(crate) fn own_group(own: &OwnAccount, groups: Option<&[(u32, String)]>) -> FieldGroup {
    let record = match own {
        OwnAccount::Unmeasured => {
            return unmeasured_group(OWN_CAPTION, ACCOUNT_LABEL, UNMEASURED);
        }
        OwnAccount::Unknown => {
            return unmeasured_group(OWN_CAPTION, ACCOUNT_LABEL, NO_OWN_ACCOUNT);
        }
        OwnAccount::Known(record) => record,
    };
    let display = field_text(record.display_name_bytes());
    FieldGroup::new(
        OWN_CAPTION,
        alloc::vec![
            reading("Account", field_text(record.name_bytes())),
            reading(
                AccountField::DisplayName.label(),
                if display.is_empty() {
                    String::from(UNSET)
                } else {
                    display
                },
            ),
            reading("User ID", decimal(record.uid)),
            reading(
                AccountField::PrimaryGroup.label(),
                group_word(record.primary_gid, groups),
            ),
            reading(
                AccountField::Groups.label(),
                membership_text(record.supplementary_gids(), groups),
            ),
            reading(AccountField::Home.label(), path_text(record.home_bytes())),
            reading(AccountField::Shell.label(), path_text(record.shell_bytes())),
        ],
    )
}

/// The account plates `roster` implies, with the field behind each row.
///
/// Before a listing lands that is the one public plate — every account's
/// name and uid, and a footnote naming what authenticating adds — or the
/// single statement of why there is not even that. After it, one plate per
/// account, whose values come from the staged edits over the listing.
pub(crate) fn roster_groups(
    facts: &AccountFacts,
    staged: &[(AccountSetting, String)],
) -> (Vec<FieldGroup>, Vec<Vec<AccountSetting>>) {
    if let Some(said) = facts.roster.statement() {
        return (
            alloc::vec![directory_group(facts, said)],
            alloc::vec![Vec::new()],
        );
    }
    let groups = facts.groups_slice();
    let accounts = facts.roster.accounts();
    let mut plates = Vec::with_capacity(accounts.len());
    let mut owners = Vec::with_capacity(accounts.len());
    for (index, account) in accounts.iter().enumerate() {
        let shown: Vec<AccountSetting> = FIELDS
            .iter()
            .map(|field| AccountSetting {
                account: index,
                field: *field,
            })
            .collect();
        let rows = shown
            .iter()
            .map(|setting| row_of(*setting, account, groups, staged))
            .collect();
        let mut plate = FieldGroup::new(
            alloc::format!("{} ({})", account.username, account.uid),
            rows,
        );
        if account.state == AccountStateCode::NoLogin {
            plate = plate.with_footnote(NOLOGIN_FOOTNOTE);
        }
        plates.push(plate);
        owners.push(shown);
    }
    (plates, owners)
}

/// The plate stating the group directory, read ungated because a gid-to-name
/// pairing is public.
pub(crate) fn groups_group(groups: Option<&[(u32, String)]>) -> FieldGroup {
    let rows = match groups {
        None => alloc::vec![unmeasured(ACCOUNT_LABEL, NO_GROUP_DIRECTORY)],
        Some([]) => alloc::vec![reading("Group", String::from(NO_GROUPS))],
        Some(listed) => listed
            .iter()
            .map(|(gid, name)| reading_owned(name.clone(), decimal(*gid)))
            .collect(),
    };
    FieldGroup::new(GROUPS_CAPTION, rows)
}

/// The public roster plate: the account directory's own rows, with `said`
/// beneath them naming why the rest of each account is not there.
///
/// Both, always. The directory is public and the rest is not, so a plate
/// that showed the names alone would read as the whole truth, and one that
/// showed only the statement would hide a reading it *does* hold.
fn directory_group(facts: &AccountFacts, said: &str) -> FieldGroup {
    let rows = match facts.users.as_deref() {
        None => alloc::vec![unmeasured(ACCOUNT_LABEL, NO_DIRECTORY)],
        Some([]) => alloc::vec![reading(ACCOUNT_LABEL, String::from(NO_ACCOUNTS))],
        Some(listed) => listed
            .iter()
            .map(|(uid, name)| reading_owned(name.clone(), decimal(*uid)))
            .collect(),
    };
    FieldGroup::new(ROSTER_CAPTION, rows).with_footnote(String::from(said))
}

/// The row `setting` draws: what the reader has made it say, else what the
/// listing holds.
///
/// A secret is never read from the staged set, and so can never be *in*
/// it: its only home is the masked entry's own bounded, self-erasing
/// buffer, and a staged copy would be a plaintext password sitting in a
/// growable string the charter makes this process responsible for erasing.
/// The entry is carried across a rebuild instead of being rebuilt from a
/// copy.
fn row_of(
    setting: AccountSetting,
    account: &Account,
    groups: Option<&[(u32, String)]>,
    staged: &[(AccountSetting, String)],
) -> FieldRow {
    let value = if setting.field.is_secret() {
        String::new()
    } else {
        staged
            .iter()
            .find(|(held, _)| *held == setting)
            .map_or_else(
                || setting.field.held(account, groups),
                |(_, value)| value.clone(),
            )
    };
    row(setting.field, account, groups, value)
}

/// The row `field` draws holding `value`.
///
/// A field the account's own shape cannot carry is a reading; a closed one
/// is a choice list over the spellings the tools admit; a secret is a
/// masked entry bounded by what the record will hash; everything else is a
/// plain entry.
fn row(
    field: AccountField,
    account: &Account,
    groups: Option<&[(u32, String)]>,
    value: String,
) -> FieldRow {
    if !field.settable_on(account.state) {
        let shown = if value.is_empty() || value == NO_PATH_MARKER {
            String::from(UNSET)
        } else {
            value
        };
        return FieldRow::new(field.label(), FieldControl::Reading(shown))
            .with_description(field.purpose());
    }
    let admits = field.admits(&value, groups);
    let control = match field {
        AccountField::Lock => {
            let choices = lock_choices();
            let mut combo = ComboBox::new(choices.clone());
            combo.set_selected(
                choices
                    .iter()
                    .position(|word| *word == value)
                    .unwrap_or_default(),
            );
            FieldControl::Combo(combo)
        }
        AccountField::Password => FieldControl::Text(
            TextField::new()
                .secret(MAX_PASSWORD_LEN)
                .with_text(value)
                .with_placeholder(UNSET),
        ),
        _ => FieldControl::Text(TextField::new().with_text(value).with_placeholder(UNSET)),
    };
    FieldRow::new(field.label(), control)
        .with_description(field.purpose())
        .with_state(ControlState {
            validation: ValidationState::of(admits),
            ..ControlState::idle()
        })
}

/// Whether `value` differs from what `account` holds for `setting`.
///
/// Both sides are the field's own one spelling, so a value the reader
/// retyped identically is not a change and a password is a change exactly
/// when something was typed.
pub(crate) fn differs(
    setting: AccountSetting,
    value: &str,
    account: &Account,
    groups: Option<&[(u32, String)]>,
) -> bool {
    value != setting.field.held(account, groups)
}

/// The run `changes` to `account` imply, with `secret` the new password
/// where one was typed, salted under `salt`.
///
/// One program and one argv: `passwd` for a password on its own, `usermod`
/// for any set of the other fields together. Both at once is two runs, so
/// it is refused here rather than half applied.
///
/// The password arrives as a **borrow** of the entry that holds it, never
/// as an owned copy: a plaintext in a second buffer is one no erasure can
/// reach, so it is read once, hashed, and never stored.
pub(crate) fn run_of(
    account: &Account,
    changes: &[(AccountField, String)],
    secret: Option<&str>,
    salt: Option<Salt>,
) -> Result<AccountRun, Unappliable> {
    if let Some(secret) = secret {
        if !changes.is_empty() {
            return Err(Unappliable::TwoTools);
        }
        return password_run(&account.username, secret, salt);
    }
    let mut argv = Vec::with_capacity(changes.len().saturating_mul(2).saturating_add(2));
    for (field, value) in changes {
        let switch = switch_of(*field, value).ok_or(Unappliable::Unspellable)?;
        argv.push(String::from(switch.name));
        if let Some(value) = switch.value {
            argv.push(value);
        }
    }
    argv.push(String::from("--"));
    argv.push(account.username.clone());
    Ok(AccountRun {
        program: USERMOD_RUN_PATH,
        argv,
    })
}

/// `value` as the tool that applies `field` spells it, or `None` where the
/// row's own spelling is already the tool's.
///
/// The group fields are the only ones that differ: a reader reads a group
/// by name and every tool takes a numeric id, so the resolution belongs
/// here — once, against the same directory the row was rendered from —
/// rather than in the surface that stages the change.
pub(crate) fn spelled_for(
    field: AccountField,
    value: &str,
    groups: Option<&[(u32, String)]>,
) -> Option<String> {
    match field {
        AccountField::PrimaryGroup => resolve_gid(value, groups).map(decimal),
        AccountField::Groups => resolve_gids(value, groups).map(|gids| {
            let mut out = String::new();
            for gid in gids {
                if !out.is_empty() {
                    out.push(',');
                }
                out.push_str(&decimal(gid));
            }
            out
        }),
        _ => None,
    }
}

/// The lock word choice `index` names, or `None` for an index outside the
/// list this surface built.
///
/// Fails closed on a routing defect, so a mis-addressed choice stages
/// nothing rather than a state the reader did not pick.
pub(crate) fn lock_choice(index: usize) -> Option<String> {
    lock_choices().get(index).cloned()
}

/// One `usermod` switch: its name, and the value it takes where it takes
/// one.
struct Switch {
    name: &'static str,
    value: Option<String>,
}

/// The switch `field` is spelled by when it is set to `value`, or `None`
/// for a value no tool can carry.
///
/// The long spellings, so the command line a reader could be shown reads
/// as what it does. A group field is resolved to the ids the tool takes
/// before it gets here, because a name is this surface's spelling and an
/// id is the tool's.
fn switch_of(field: AccountField, value: &str) -> Option<Switch> {
    let named = |name: &'static str| {
        Some(Switch {
            name,
            value: Some(String::from(value)),
        })
    };
    match field {
        AccountField::DisplayName => named("--comment"),
        AccountField::Home => named("--home"),
        AccountField::Shell => named("--shell"),
        AccountField::PrimaryGroup => named("--gid"),
        AccountField::Groups => named("--groups"),
        AccountField::Grants => named("--grants"),
        AccountField::Lock => match value {
            "active" => Some(Switch {
                name: "--unlock",
                value: None,
            }),
            "locked" => Some(Switch {
                name: "--lock",
                value: None,
            }),
            _ => None,
        },
        AccountField::Password => None,
    }
}

/// The `passwd` run that replaces `username`'s password with a record
/// derived from `secret`.
///
/// The record is built here and handed straight to the argv: the password
/// itself is read once, out of the masked entry that is its only home, and
/// no plaintext ever leaves this process. A draw that produced no salt
/// refuses the run rather than reaching for a predictable one.
fn password_run(
    username: &str,
    secret: &str,
    salt: Option<Salt>,
) -> Result<AccountRun, Unappliable> {
    let salt = salt.ok_or(Unappliable::NoRandomness)?;
    let record = tairix_users::PasswordRecord::new(
        secret.as_bytes(),
        salt,
        tairix_users::DEFAULT_ITERATIONS,
    )
    .map_err(|_| Unappliable::Unspellable)?;
    Ok(AccountRun {
        program: PASSWD_RUN_PATH,
        argv: alloc::vec![
            String::from("--record"),
            record.encode(),
            String::from("--"),
            String::from(username),
        ],
    })
}

/// The gids `text` names, or `None` where an element names neither a group
/// this machine holds nor a decimal id, or where there are more of them
/// than one account may carry.
///
/// Names *or* ids, because a name is what a reader reads and an id is what
/// survives a directory this window could not take. An empty string is the
/// empty set, which is how a membership is cleared.
fn resolve_gids(text: &str, groups: Option<&[(u32, String)]>) -> Option<Vec<u32>> {
    let mut gids = Vec::new();
    for word in text.split(',').filter(|word| !word.is_empty()) {
        let gid = resolve_gid(word, groups)?;
        if !gids.contains(&gid) {
            gids.push(gid);
        }
    }
    (gids.len() <= MAX_SUPPLEMENTARY_GIDS).then_some(gids)
}

/// The gid `word` names: a group of the directory, else a decimal id.
fn resolve_gid(word: &str, groups: Option<&[(u32, String)]>) -> Option<u32> {
    if let Some(listed) = groups {
        if let Some((gid, _)) = listed.iter().find(|(_, name)| name == word) {
            return Some(*gid);
        }
    }
    parse_id(word)
}

/// `gids` in the comma-separated spelling a row shows and takes back: each
/// group's name where the directory holds one, else its decimal id.
fn render_gids(gids: &[u32], groups: Option<&[(u32, String)]>) -> String {
    let mut out = String::new();
    for gid in gids {
        if !out.is_empty() {
            out.push(',');
        }
        out.push_str(&group_word(*gid, groups));
    }
    out
}

/// `gids` as a reading: the same spelling, or the plain statement that
/// there are none.
fn membership_text(gids: &[u32], groups: Option<&[(u32, String)]>) -> String {
    if gids.is_empty() {
        return String::from("none");
    }
    render_gids(gids, groups)
}

/// The word `gid` is rendered by: its group's name where the directory
/// holds one, else its decimal id.
///
/// A directory this window could not read renders every gid numerically,
/// which is the honest answer rather than a fabricated name.
fn group_word(gid: u32, groups: Option<&[(u32, String)]>) -> String {
    groups
        .and_then(|listed| listed.iter().find(|(held, _)| *held == gid))
        .map_or_else(|| decimal(gid), |(_, name)| name.clone())
}

/// A canonically spelled decimal id: no sign, no leading zeros, so each
/// value has exactly one spelling a row can round-trip.
fn parse_id(text: &str) -> Option<u32> {
    if text.is_empty() || (text.len() > 1 && text.starts_with('0')) {
        return None;
    }
    text.bytes()
        .all(|byte| byte.is_ascii_digit())
        .then(|| text.parse().ok())
        .flatten()
}

/// One unsigned id as text.
fn decimal(id: u32) -> String {
    alloc::format!("{id}")
}

/// One inline account text field, decoded lossily for display.
///
/// Lossily because the frame carries whatever the database holds and a
/// display must render *something*: a byte the account name grammar
/// forbids is a defect in the database, not a reason to blank the row.
fn field_text(bytes: &[u8]) -> String {
    tairix_procinfo::field_lossy(bytes)
}

/// One inline home or shell path, with the database's own absent-path
/// marker rendered as the reader's word for it — the same word every other
/// plate shows an absent value by.
fn path_text(bytes: &[u8]) -> String {
    let text = field_text(bytes);
    if text.is_empty() || text == NO_PATH_MARKER {
        return String::from(UNSET);
    }
    text
}

/// One label-and-reading row with a static label.
fn reading(label: &'static str, value: String) -> FieldRow {
    FieldRow::new(label, FieldControl::Reading(value))
}

/// One label-and-reading row whose label is discovered rather than
/// declared.
fn reading_owned(label: String, value: String) -> FieldRow {
    FieldRow::new(label, FieldControl::Reading(value))
}

/// One row stating that there is no measurement, and why.
fn unmeasured(label: &'static str, said: &str) -> FieldRow {
    FieldRow::new(label, FieldControl::Unmeasured(String::from(said)))
}

/// A plate holding nothing but the statement of why it holds nothing.
fn unmeasured_group(caption: &'static str, label: &'static str, said: &str) -> FieldGroup {
    FieldGroup::new(caption, alloc::vec![unmeasured(label, said)])
}
