//! The `CAP_USER_ADMIN` account-administration engine behind the
//! `users_admin` syscall (`plans/CAPABILITY_USE.md` CU4).
//!
//! The engine is the single kernel-side writer of the account state: it
//! owns the parsed user/group databases, applies one validated, typed
//! operation at a time, re-verifies the whole identity table with the
//! same checks the boot load runs, persists the edited `users-v1` /
//! `groups-v1` texts to the root volume, and only then swaps the live
//! [`LateUsersDb`] text and [`LateIdentity`] table. A change therefore
//! binds at the *next* spawn or login; running processes keep the
//! capability record they were derived with.
//!
//! Enforcement the engine adds on top of the dispatch-level
//! `CAP_USER_ADMIN` gate:
//!
//! * **Delegation narrows** — a grant edit may add only capabilities the
//!   *caller's own* effective set holds; an administrator cannot mint an
//!   account more powerful than themselves.
//! * **User management cannot be bricked** — the last active account
//!   holding `CAP_USER_ADMIN` can be neither deleted, locked, nor
//!   stripped of that grant.
//! * **The on-disk format bounds hold** — every field passes the
//!   `lib/users` validating constructors, and the serialised texts are
//!   refused if they exceed the format maxima the next boot's parser
//!   enforces.
//!
//! Every operation outcome is audited with a stable event id and the
//! caller's kernel-attested uid; no password material ever appears in a
//! record or a response.

use alloc::vec::Vec;

use rustos_abi::users_admin::{
    gid_list_into, grant_list_into, AccountStateCode, GroupEntry, ListResponseBuilder, UserEntry,
    UsersAdminRequest,
};
use rustos_abi::{CapabilityId, CapabilityQuery, Errno};
use rustos_caps::CapabilitySet;
use rustos_log::{Field, FieldValue, Level, Sink};
use rustos_users::{
    AccountState, Gid, GroupRecord, GroupsDb, Identity, ParseError, PasswordRecord, StoredPassword,
    Uid, UserRecord, UsersDb, MAX_DB_LEN, MAX_GROUPS_DB_LEN, NO_PATH_MARKER,
};
use rustos_util::fmt::format_usize;

use crate::audit::{emit, AuditEvent};
use crate::fs::LateIdentity;
use crate::groups::build_identity_table;
use crate::sleeplock::SleepLock;
use crate::users::{HeldUsersDbSource, LateUsersDb};

/// The facility seam the `users_admin` syscall handler dispatches into
/// (mirrors [`crate::users::UsersDbSource`]).
///
/// `Sync` because the single installed engine is shared by the per-CPU
/// syscall handlers.
pub trait UsersAdmin: Sync {
    /// Apply one decoded request under the caller's kernel-attested
    /// identity, writing any list response into `out` and returning the
    /// response byte length (`0` for a mutating operation).
    ///
    /// # Errors
    ///
    /// The stable [`Errno`] for the refusal; every failure leaves the
    /// account state untouched (an operation applies whole or not at
    /// all).
    fn handle(
        &self,
        caller_uid: u32,
        caller_caps: &dyn CapabilityQuery,
        request: &UsersAdminRequest<'_>,
        out: &mut [u8],
    ) -> Result<u64, Errno>;
}

/// The fail-closed default: every operation reports
/// [`Errno::NotImplemented`] until the boot path installs the real
/// engine (mirrors [`crate::users::NullUsersDbSource`]).
#[derive(Debug, Default, Copy, Clone)]
pub struct NullUsersAdmin;

impl UsersAdmin for NullUsersAdmin {
    fn handle(
        &self,
        _caller_uid: u32,
        _caller_caps: &dyn CapabilityQuery,
        _request: &UsersAdminRequest<'_>,
        _out: &mut [u8],
    ) -> Result<u64, Errno> {
        Err(Errno::NotImplemented)
    }
}

/// The shared [`NullUsersAdmin`] instance the syscall handler defaults
/// to until a boot path installs the real engine.
pub static NULL_USERS_ADMIN: NullUsersAdmin = NullUsersAdmin;

/// An engine install was refused because one is already installed.
#[derive(Debug, Default, Copy, Clone, Eq, PartialEq)]
pub struct UsersAdminAlreadyInstalled;

/// A set-once [`UsersAdmin`] cell the boot path installs the real
/// engine into after the unlock has loaded the account databases
/// (mirrors [`crate::users::LateUsersDb`]).
///
/// The engine can only be built once the root volume is unlocked and
/// both databases are verified, which is long past the point the
/// dispatch hook is wired. The hook therefore holds a
/// `&'static LateUsersAdmin` from boot; until the trusted unlock step
/// installs the engine every `users_admin` call fails closed with
/// [`Errno::NotImplemented`], and the install is set-once so no later
/// code path can swap the live engine.
pub struct LateUsersAdmin {
    engine: rustos_sync::OnceCell<UserAdminEngine>,
}

impl LateUsersAdmin {
    /// Construct an empty cell. `const` so a boot path can place it in
    /// a `static`.
    #[must_use]
    pub const fn new() -> Self {
        Self {
            engine: rustos_sync::OnceCell::new(),
        }
    }

    /// Publish the engine exactly once.
    ///
    /// # Errors
    ///
    /// [`UsersAdminAlreadyInstalled`] if an engine is already installed.
    pub fn install(&self, engine: UserAdminEngine) -> Result<(), UsersAdminAlreadyInstalled> {
        self.engine
            .set(engine)
            .map_err(|_| UsersAdminAlreadyInstalled)
    }

    /// Whether an engine has been installed.
    #[must_use]
    pub fn is_installed(&self) -> bool {
        self.engine.is_initialised()
    }
}

impl Default for LateUsersAdmin {
    fn default() -> Self {
        Self::new()
    }
}

impl UsersAdmin for LateUsersAdmin {
    fn handle(
        &self,
        caller_uid: u32,
        caller_caps: &dyn CapabilityQuery,
        request: &UsersAdminRequest<'_>,
        out: &mut [u8],
    ) -> Result<u64, Errno> {
        match self.engine.get() {
            Ok(Some(engine)) => engine.handle(caller_uid, caller_caps, request, out),
            _ => Err(Errno::NotImplemented),
        }
    }
}

/// The storage the engine commits through: the root-volume writes only
/// the disk-owning boot path can perform.
///
/// Implemented by `rustos-kernel` over the mounted root volume's
/// concrete driver; kernel/core stays driver-agnostic. Both methods run
/// under the engine's operation lock, so implementations need no
/// ordering of their own.
pub trait UserAdminBacking: Sync {
    /// Persist both database texts to `/System/Security/Users` and
    /// `/System/Security/Groups`, durably (written and flushed before
    /// returning).
    ///
    /// # Errors
    ///
    /// The stable [`Errno`] for the write failure; the caller treats any
    /// error as "nothing changed" and keeps the previous state live.
    fn persist(&self, users_text: &str, groups_text: &str) -> Result<(), Errno>;

    /// Create the directory `home` owned by `(uid, gid)` with an
    /// owner-only mode, so a freshly created account can use its home on
    /// first login. An already-existing directory is left untouched and
    /// reported as success (provisioning is idempotent).
    ///
    /// # Errors
    ///
    /// The stable [`Errno`] for the failure.
    fn provision_home(&self, home: &str, uid: u32, gid: u32) -> Result<(), Errno>;
}

/// Upper bound, in bytes, on any `users_admin` list response the kernel
/// will build (validation bound — a defence, not a capacity).
///
/// A [`UsersAdminOp::ListUsers`](rustos_abi::users_admin::UsersAdminOp)
/// response re-encodes the account records without their password
/// fields, so it is comfortably bounded by twice the on-disk database
/// maximum; the groups response is far smaller. The handler clamps its
/// response allocation here so a hostile capacity cannot drive an
/// unbounded kernel allocation.
pub const USERS_ADMIN_MAX_RESPONSE: usize = 2 * MAX_DB_LEN;

/// The parsed account state the engine owns and edits.
struct AdminState {
    users: UsersDb,
    groups: GroupsDb,
}

/// The production [`UsersAdmin`]: owns the parsed databases and commits
/// each operation through validate → verify → persist → swap.
///
/// Constructed by the boot path *after* the unlock has installed the
/// boot databases into the live cells, over the same parsed state, so
/// the engine and the cells can never start out disagreeing.
pub struct UserAdminEngine {
    /// The account state, serialised under a sleep lock: an operation
    /// parks on disk I/O while persisting, and operations are strictly
    /// one at a time (no lost updates, no partial interleavings).
    state: SleepLock<AdminState>,
    /// The live users-database text the `users_db_read` syscall serves;
    /// swapped on every successful mutation.
    users_cell: &'static LateUsersDb,
    /// The live identity table spawn/fs resolution reads; swapped on
    /// every successful mutation.
    identity_cell: &'static LateIdentity,
    /// The root-volume writes the engine commits through.
    backing: &'static dyn UserAdminBacking,
    /// The audit sink every operation outcome is recorded to.
    audit: &'static (dyn Sink + Sync),
}

impl UserAdminEngine {
    /// Build the engine over the boot-loaded databases and the live
    /// cells they were installed into.
    #[must_use]
    pub fn new(
        users: UsersDb,
        groups: GroupsDb,
        users_cell: &'static LateUsersDb,
        identity_cell: &'static LateIdentity,
        backing: &'static dyn UserAdminBacking,
        audit: &'static (dyn Sink + Sync),
    ) -> Self {
        Self {
            state: SleepLock::new(AdminState { users, groups }),
            users_cell,
            identity_cell,
            backing,
            audit,
        }
    }

    /// Validate `candidate` as the complete next account state, persist
    /// it, and swap the live cells — the single commit path every
    /// mutating operation funnels through (whole or not at all).
    fn commit(&self, state: &mut AdminState, candidate: AdminState) -> Result<(), Errno> {
        // The last active administrator must survive every edit: an
        // account state in which no active account holds CAP_USER_ADMIN
        // could never be administered again (fail closed).
        let administrable = candidate.users.records().iter().any(|record| {
            record.state() == AccountState::Active
                && record.capabilities().contains(CapabilityId::USER_ADMIN)
        });
        if !administrable {
            return Err(Errno::PermissionDenied);
        }

        // Re-verify the whole identity table with the same checks the
        // boot load runs (group referential integrity, uid/gid
        // uniqueness, supplementary bounds).
        let table = build_identity_table(&candidate.users, &candidate.groups, self.audit)?;

        // The serialised texts must stay parseable by the next boot's
        // reader, whose format maxima are validation bounds.
        let users_text = candidate.users.serialise();
        let groups_text = candidate.groups.serialise();
        if users_text.len() > MAX_DB_LEN || groups_text.len() > MAX_GROUPS_DB_LEN {
            return Err(Errno::NoSpace);
        }

        // Persist first: the on-disk databases are what the next boot
        // reads, so the disk commits before the live view moves.
        self.backing.persist(&users_text, &groups_text)?;

        // Swap the live views. Both cells were installed at boot (the
        // engine is only constructed afterwards), so neither replace can
        // refuse; a refusal would mean the engine was mis-wired and the
        // operation reports it rather than half-applying.
        let mut users_bytes = users_text.into_bytes();
        let replaced = self
            .users_cell
            .replace(HeldUsersDbSource::new(core::mem::take(&mut users_bytes)));
        replaced?;
        self.identity_cell.replace(table)?;

        *state = candidate;
        Ok(())
    }

    /// Record one operation outcome with the caller's attested uid.
    fn audit_outcome(&self, caller_uid: u32, op: &'static str, target: &str, err: Option<Errno>) {
        let mut uid_buf = [0u8; 12];
        let uid = format_usize(caller_uid as usize, &mut uid_buf);
        let mut errno_buf = [0u8; 12];
        let (event, level, errno) = match err {
            None => (AuditEvent::UserAdminApplied, Level::Info, ""),
            Some(err) => (
                AuditEvent::UserAdminRejected,
                Level::Warn,
                format_usize(err as usize, &mut errno_buf),
            ),
        };
        emit(
            self.audit,
            level,
            event,
            &[
                Field {
                    key: "op",
                    value: FieldValue::Str(op),
                },
                Field {
                    key: "target",
                    value: FieldValue::Str(target),
                },
                Field {
                    key: "caller_uid",
                    value: FieldValue::Str(uid),
                },
                Field {
                    key: "errno",
                    value: FieldValue::Str(errno),
                },
            ],
        );
    }

    /// Enforce that `new_grants` adds nothing over `old_grants` the
    /// caller does not hold: delegation narrows, so an administrator can
    /// grant at most their own effective authority.
    fn check_never_widen(
        caller_caps: &dyn CapabilityQuery,
        old_grants: &CapabilitySet,
        new_grants: &CapabilitySet,
    ) -> Result<(), Errno> {
        for cap in new_grants {
            if !old_grants.contains(cap) && !caller_caps.holds(cap) {
                return Err(Errno::PermissionDenied);
            }
        }
        Ok(())
    }

    /// The index of `username`'s record, or [`Errno::NotFound`].
    fn find_user(users: &UsersDb, username: &str) -> Result<usize, Errno> {
        users
            .records()
            .iter()
            .position(|record| record.username() == username)
            .ok_or(Errno::NotFound)
    }

    /// Apply one mutating rebuild of the user list: replace the records
    /// wholesale and re-run the whole-database invariants.
    fn users_with(state: &AdminState, records: Vec<UserRecord>) -> Result<AdminState, Errno> {
        Ok(AdminState {
            users: UsersDb::new(records).map_err(parse_errno)?,
            groups: state.groups.clone(),
        })
    }

    fn create_user(
        &self,
        caller_caps: &dyn CapabilityQuery,
        req: &rustos_abi::users_admin::CreateUser<'_>,
        state: &mut AdminState,
    ) -> Result<(), Errno> {
        let grants = capability_set(req.grants.iter());
        Self::check_never_widen(caller_caps, &CapabilitySet::empty(), &grants)?;
        let password = PasswordRecord::decode(req.password_record).map_err(parse_errno)?;
        let supplementary: Vec<Gid> = req.supplementary_gids.iter().map(Gid).collect();
        let record = UserRecord::new(
            Identity {
                username: req.username,
                uid: Uid(req.uid),
                primary_gid: Gid(req.primary_gid),
                supplementary_gids: &supplementary,
                display_name: req.display_name,
                home: Some(req.home),
                shell: Some(req.shell),
                capabilities: grants,
                state: AccountState::Active,
            },
            StoredPassword::Password(password),
        )
        .map_err(parse_errno)?;

        let mut records = state.users.records().to_vec();
        records.push(record);
        let candidate = Self::users_with(state, records)?;

        // Provision the home before the commit: creation is idempotent
        // and an orphaned directory from a later refusal is harmless,
        // whereas a committed account with no usable home is a defect.
        self.backing
            .provision_home(req.home, req.uid, req.primary_gid)?;
        self.commit(state, candidate)
    }

    fn modify_user(
        &self,
        req: &rustos_abi::users_admin::ModifyUser<'_>,
        state: &mut AdminState,
    ) -> Result<(), Errno> {
        let index = Self::find_user(&state.users, req.username)?;
        let old = &state.users.records()[index];
        let supplementary: Vec<Gid> = req.supplementary_gids.iter().map(Gid).collect();
        let rebuilt = UserRecord::new(
            Identity {
                username: old.username(),
                uid: old.uid(),
                primary_gid: Gid(req.primary_gid),
                supplementary_gids: &supplementary,
                display_name: req.display_name,
                home: Some(req.home),
                shell: Some(req.shell),
                capabilities: old.capabilities(),
                state: old.state(),
            },
            old.password().clone(),
        )
        .map_err(parse_errno)?;

        let home_changed = old.home() != Some(req.home);
        let (uid, gid) = (old.uid().0, Gid(req.primary_gid).0);
        let mut records = state.users.records().to_vec();
        records[index] = rebuilt;
        let candidate = Self::users_with(state, records)?;
        if home_changed {
            self.backing.provision_home(req.home, uid, gid)?;
        }
        self.commit(state, candidate)
    }

    fn delete_user(&self, username: &str, state: &mut AdminState) -> Result<(), Errno> {
        let index = Self::find_user(&state.users, username)?;
        let mut records = state.users.records().to_vec();
        records.remove(index);
        let candidate = Self::users_with(state, records)?;
        self.commit(state, candidate)
    }

    fn set_account_state(
        &self,
        username: &str,
        locked: bool,
        state: &mut AdminState,
    ) -> Result<(), Errno> {
        let target = if locked {
            AccountState::Locked
        } else {
            AccountState::Active
        };
        Self::rebuild_user(state, username, |old, supplementary| {
            UserRecord::new(
                Identity {
                    username: old.username(),
                    uid: old.uid(),
                    primary_gid: old.primary_gid(),
                    supplementary_gids: supplementary,
                    display_name: old.display_name(),
                    home: old.home(),
                    shell: old.shell(),
                    capabilities: old.capabilities(),
                    state: target,
                },
                old.password().clone(),
            )
        })
        .and_then(|candidate| self.commit(state, candidate))
    }

    fn set_grants(
        &self,
        caller_caps: &dyn CapabilityQuery,
        username: &str,
        grants: CapabilitySet,
        state: &mut AdminState,
    ) -> Result<(), Errno> {
        {
            let index = Self::find_user(&state.users, username)?;
            let old = &state.users.records()[index];
            Self::check_never_widen(caller_caps, &old.capabilities(), &grants)?;
        }
        Self::rebuild_user(state, username, |old, supplementary| {
            UserRecord::new(
                Identity {
                    username: old.username(),
                    uid: old.uid(),
                    primary_gid: old.primary_gid(),
                    supplementary_gids: supplementary,
                    display_name: old.display_name(),
                    home: old.home(),
                    shell: old.shell(),
                    capabilities: grants,
                    state: old.state(),
                },
                old.password().clone(),
            )
        })
        .and_then(|candidate| self.commit(state, candidate))
    }

    fn set_password(
        &self,
        username: &str,
        password_record: &str,
        state: &mut AdminState,
    ) -> Result<(), Errno> {
        let password =
            StoredPassword::Password(PasswordRecord::decode(password_record).map_err(parse_errno)?);
        Self::rebuild_user(state, username, |old, supplementary| {
            UserRecord::new(
                Identity {
                    username: old.username(),
                    uid: old.uid(),
                    primary_gid: old.primary_gid(),
                    supplementary_gids: supplementary,
                    display_name: old.display_name(),
                    home: old.home(),
                    shell: old.shell(),
                    capabilities: old.capabilities(),
                    state: old.state(),
                },
                password.clone(),
            )
        })
        .and_then(|candidate| self.commit(state, candidate))
    }

    /// Rebuild `username`'s record through `build` (which receives the
    /// old record and its supplementary-gid slice) into a candidate
    /// state.
    fn rebuild_user(
        state: &AdminState,
        username: &str,
        build: impl Fn(&UserRecord, &[Gid]) -> Result<UserRecord, ParseError>,
    ) -> Result<AdminState, Errno> {
        let index = Self::find_user(&state.users, username)?;
        let old = &state.users.records()[index];
        let supplementary: Vec<Gid> = old.supplementary_gids().to_vec();
        let rebuilt = build(old, &supplementary).map_err(parse_errno)?;
        let mut records = state.users.records().to_vec();
        records[index] = rebuilt;
        Self::users_with(state, records)
    }

    fn create_group(&self, name: &str, gid: u32, state: &mut AdminState) -> Result<(), Errno> {
        let record = GroupRecord::new(name, Gid(gid)).map_err(parse_errno)?;
        let mut records = state.groups.records().to_vec();
        records.push(record);
        let candidate = AdminState {
            users: state.users.clone(),
            groups: GroupsDb::new(records).map_err(parse_errno)?,
        };
        self.commit(state, candidate)
    }

    fn delete_group(&self, name: &str, state: &mut AdminState) -> Result<(), Errno> {
        let index = state
            .groups
            .records()
            .iter()
            .position(|record| record.name() == name)
            .ok_or(Errno::NotFound)?;
        let mut records = state.groups.records().to_vec();
        records.remove(index);
        let candidate = AdminState {
            users: state.users.clone(),
            groups: GroupsDb::new(records).map_err(parse_errno)?,
        };
        // A group still referenced by any account fails the identity
        // verification inside `commit` (referential integrity), so the
        // deletion is refused rather than stranding a user.
        self.commit(state, candidate)
    }

    fn list_users(state: &AdminState, out: &mut [u8]) -> Result<u64, Errno> {
        let mut builder = ListResponseBuilder::new(out)?;
        for record in state.users.records() {
            let ids: Vec<CapabilityId> = record.capabilities().iter().collect();
            let mut grant_backing = alloc::vec![0u8; ids.len() * 2];
            let grants = grant_list_into(&ids, &mut grant_backing)?;
            let gids: Vec<u32> = record
                .supplementary_gids()
                .iter()
                .map(|gid| gid.0)
                .collect();
            let mut gid_backing = alloc::vec![0u8; gids.len() * 4];
            let supplementary_gids = gid_list_into(&gids, &mut gid_backing)?;
            builder.push_user(&UserEntry {
                username: record.username(),
                uid: record.uid().0,
                primary_gid: record.primary_gid().0,
                supplementary_gids,
                display_name: record.display_name(),
                home: record.home().unwrap_or(NO_PATH_MARKER),
                shell: record.shell().unwrap_or(NO_PATH_MARKER),
                grants,
                state: match record.state() {
                    AccountState::Active => AccountStateCode::Active,
                    AccountState::Locked => AccountStateCode::Locked,
                    AccountState::NoLogin => AccountStateCode::NoLogin,
                },
            })?;
        }
        Ok(builder.finish() as u64)
    }

    fn list_groups(state: &AdminState, out: &mut [u8]) -> Result<u64, Errno> {
        let mut builder = ListResponseBuilder::new(out)?;
        for record in state.groups.records() {
            builder.push_group(&GroupEntry {
                name: record.name(),
                gid: record.gid().0,
            })?;
        }
        Ok(builder.finish() as u64)
    }
}

impl UsersAdmin for UserAdminEngine {
    fn handle(
        &self,
        caller_uid: u32,
        caller_caps: &dyn CapabilityQuery,
        request: &UsersAdminRequest<'_>,
        out: &mut [u8],
    ) -> Result<u64, Errno> {
        let mut state = self.state.lock();
        let (op, target): (&'static str, &str) = match request {
            UsersAdminRequest::CreateUser(req) => ("create_user", req.username),
            UsersAdminRequest::ModifyUser(req) => ("modify_user", req.username),
            UsersAdminRequest::DeleteUser { username } => ("delete_user", username),
            UsersAdminRequest::SetAccountState { username, locked } => {
                (if *locked { "lock_user" } else { "unlock_user" }, *username)
            }
            UsersAdminRequest::SetGrants { username, .. } => ("set_grants", username),
            UsersAdminRequest::SetPassword { username, .. } => ("set_password", username),
            UsersAdminRequest::CreateGroup { name, .. } => ("create_group", name),
            UsersAdminRequest::DeleteGroup { name } => ("delete_group", name),
            UsersAdminRequest::ListUsers => ("list_users", ""),
            UsersAdminRequest::ListGroups => ("list_groups", ""),
        };
        let result = match request {
            UsersAdminRequest::CreateUser(req) => self
                .create_user(caller_caps, req, &mut state)
                .map(|()| 0u64),
            UsersAdminRequest::ModifyUser(req) => self.modify_user(req, &mut state).map(|()| 0u64),
            UsersAdminRequest::DeleteUser { username } => {
                self.delete_user(username, &mut state).map(|()| 0u64)
            }
            UsersAdminRequest::SetAccountState { username, locked } => self
                .set_account_state(username, *locked, &mut state)
                .map(|()| 0u64),
            UsersAdminRequest::SetGrants { username, grants } => self
                .set_grants(
                    caller_caps,
                    username,
                    capability_set(grants.iter()),
                    &mut state,
                )
                .map(|()| 0u64),
            UsersAdminRequest::SetPassword {
                username,
                password_record,
            } => self
                .set_password(username, password_record, &mut state)
                .map(|()| 0u64),
            UsersAdminRequest::CreateGroup { name, gid } => {
                self.create_group(name, *gid, &mut state).map(|()| 0u64)
            }
            UsersAdminRequest::DeleteGroup { name } => {
                self.delete_group(name, &mut state).map(|()| 0u64)
            }
            UsersAdminRequest::ListUsers => Self::list_users(&state, out),
            UsersAdminRequest::ListGroups => Self::list_groups(&state, out),
        };
        self.audit_outcome(caller_uid, op, target, result.err());
        result
    }
}

/// Collect capability ids into a [`CapabilitySet`].
fn capability_set(ids: impl Iterator<Item = CapabilityId>) -> CapabilitySet {
    let mut set = CapabilitySet::empty();
    for id in ids {
        set.insert(id);
    }
    set
}

/// Map a `lib/users` validation refusal onto the stable [`Errno`] the
/// syscall reports: duplicates collide with existing state, record
/// budgets are exhaustion, and every other refusal is a malformed field.
fn parse_errno(err: ParseError) -> Errno {
    match err {
        ParseError::DuplicateUsername
        | ParseError::DuplicateUserId
        | ParseError::DuplicateGroupName
        | ParseError::DuplicateGroupId => Errno::AlreadyExists,
        ParseError::TooManyUsers | ParseError::TooManyGroups => Errno::NoSpace,
        ParseError::TooLong | ParseError::LineTooLong => Errno::LengthOutOfRange,
        _ => Errno::OutOfRange,
    }
}

#[cfg(test)]
#[path = "useradmin_tests.rs"]
mod tests;
