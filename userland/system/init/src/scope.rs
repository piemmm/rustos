//! The authority scope a service-manager instance wields
//! (`plans/NEW-SERVICEMANAGER.md` §3.2).
//!
//! TAIRiX runs **one policy engine at two authority scopes**, never two
//! codebases: the single system service manager (PID 1's role) and one per-user
//! manager instance per logged-in user. The scope an [`Init`] was created with
//! is the security boundary between those two roles — it is fixed for the
//! instance's whole life and decides which services that instance is allowed to
//! manage.
//!
//! # The boundary this enforces
//!
//! A per-user manager is spawned by the system manager at session start and
//! delegated only that user's authority. It must never be able to bring a
//! system-authority service to life, nor reach another user's services: a
//! user's on-demand request can never make a system-authority service appear,
//! and one user's manager can never touch another user's services.
//!
//! Because a service is always launched **as a service account** (a uid) and
//! the kernel derives the capability grant from that account's ceiling — the
//! kernel is the single capability authority — a manager confined to one user
//! can only be permitted to manage services that run **as that user**. Naming
//! any other account (a system service account, or another user's uid) is the
//! escalation this scope refuses, fail closed.
//!
//! This is deliberately an *identity* boundary, not a capability computation.
//! The engine never decodes a manifest or derives a grant on the launch path;
//! it checks only the one fact it already holds — the service account the
//! [`ServiceSpec`] names — so there is no second capability-derivation path to
//! drift from the kernel's authoritative one.
//!
//! [`Init`]: crate::Init
//! [`ServiceSpec`]: crate::ServiceSpec

/// Which authority a service-manager instance holds.
///
/// Chosen once when the instance is created and never changed. The system
/// manager and every per-user manager are the *same* [`Init`](crate::Init)
/// engine differing only in this value, so the system-versus-user boundary is
/// realised as data, not as a forked codebase.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum AuthorityScope {
    /// The single system service manager — PID 1's role.
    ///
    /// It holds system authority and may manage services running under any
    /// account (the system service accounts of `plans/USERS.md`). It is also
    /// the last-resort reaper for every orphaned process on the machine.
    System,
    /// A per-user manager instance, confined to exactly one user.
    ///
    /// It may manage **only** services that run as `uid`. Naming any other
    /// account is refused (fail closed), so a per-user manager can neither
    /// raise a service to system authority nor reach into another user's
    /// services.
    User {
        /// The uid this manager is confined to. Every service it manages must
        /// run as this account.
        uid: u32,
    },
}

impl AuthorityScope {
    /// Whether this is the system-manager scope.
    #[must_use]
    pub const fn is_system(self) -> bool {
        matches!(self, Self::System)
    }

    /// The uid a per-user manager is confined to, or `None` for the system
    /// manager.
    #[must_use]
    pub const fn uid(self) -> Option<u32> {
        match self {
            Self::System => None,
            Self::User { uid } => Some(uid),
        }
    }

    /// Whether a manager in this scope may manage a service that runs as the
    /// service account `account`.
    ///
    /// The system manager may manage any account; a per-user manager may
    /// manage **only** its own `uid`. This single predicate is the whole
    /// authority boundary between the two roles: a `false` here is the
    /// fail-closed refusal that stops a user's manager from bringing a
    /// system-authority service — or another user's service — to life.
    #[must_use]
    pub const fn permits_account(self, account: u32) -> bool {
        match self {
            Self::System => true,
            Self::User { uid } => account == uid,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::AuthorityScope;

    #[test]
    fn system_scope_permits_every_account() {
        let scope = AuthorityScope::System;
        assert!(scope.is_system());
        assert_eq!(scope.uid(), None);
        // Any account — a system service account, any user — is in scope.
        assert!(scope.permits_account(0));
        assert!(scope.permits_account(15));
        assert!(scope.permits_account(1000));
        assert!(scope.permits_account(u32::MAX));
    }

    #[test]
    fn user_scope_permits_only_its_own_uid() {
        let scope = AuthorityScope::User { uid: 1000 };
        assert!(!scope.is_system());
        assert_eq!(scope.uid(), Some(1000));
        // Its own account is in scope.
        assert!(scope.permits_account(1000));
        // A system service account is refused — no privilege escalation.
        assert!(!scope.permits_account(15));
        assert!(!scope.permits_account(0));
        // Another user's account is refused — no cross-user reach.
        assert!(!scope.permits_account(1001));
    }
}
