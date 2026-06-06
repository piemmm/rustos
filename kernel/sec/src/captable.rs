//! Per-task capability state.
//!
//! Every running task has a [`TaskCapabilities`] record. The effective
//! capability set is **the intersection** of two narrower-or-equal sets
//! (`AGENTS.md` §5.2):
//!
//! * the user grant attached to the task's owning [`UserId`], and
//! * the capability set the binary's signed manifest *requested*.
//!
//! Both halves enter through the verifier in `manifest.rs` and the
//! verifier in `identity.rs`; this module never widens what those modules
//! sanctioned. Delegation and revocation are forwarded to
//! `lib/caps` — the single source of truth for the subset-only delegation
//! invariant — and every transition emits exactly one audit event.
//!
//! # No ambient authority
//!
//! Nothing in this module branches on `uid == 0`. The numeric uid is
//! attached purely so the audit trail can attribute a record to a
//! principal; it confers no extra capability.

extern crate alloc;

use alloc::collections::BTreeMap;

use rustos_abi::Errno;
use rustos_caps::{CapabilitySet, CapabilityToken, RevocationEpoch};
use rustos_crypto::Ed25519PublicKey;
use rustos_log::{Field, Sink};

use crate::audit::{record, AuditEvent};
use crate::identity::{format_hex_u64, format_i32, UserId};

/// Numeric task identifier carried by audit records.
///
/// Distinct from `pid_t`: `TaskId` is the kernel's internal handle for a
/// schedulable entity. `kernel/sched` produces these; we accept them
/// verbatim for audit attribution.
#[repr(transparent)]
#[derive(Copy, Clone, Debug, Default, Eq, PartialEq, Hash, Ord, PartialOrd)]
pub struct TaskId(pub u64);

/// Per-task capability state.
///
/// The fields are private so callers cannot bypass the intersection
/// invariant by writing the effective set directly. Construct via
/// [`Self::derive`] and mutate only through [`Self::delegate`] and
/// [`Self::revoke`].
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct TaskCapabilities {
    task: TaskId,
    owner: UserId,
    /// Maximum the owner's user grant ever allows on this task. Acts as
    /// the *upper bound* for every subsequent operation; nothing in this
    /// module can grow `effective` past this set.
    user_grant: CapabilitySet,
    /// What the binary's manifest asked for (already verified by
    /// [`crate::verify_manifest`]).
    manifest_request: CapabilitySet,
    /// Currently effective set. Always a subset of `user_grant ∩ manifest_request`.
    effective: CapabilitySet,
}

impl TaskCapabilities {
    /// Derive a task's effective capabilities from its user grant and the
    /// verified manifest request.
    ///
    /// The effective set is the intersection of the two inputs (`AGENTS.md`
    /// §5.2). Emits exactly one
    /// [`AuditEvent::TaskCapabilitiesDerived`].
    pub fn derive<S: Sink + ?Sized>(
        task: TaskId,
        owner: UserId,
        user_grant: CapabilitySet,
        manifest_request: CapabilitySet,
        audit: &S,
    ) -> Self {
        let effective = user_grant.intersection(&manifest_request);
        let mut task_buf = [0u8; 16];
        let task_field = format_hex_u64(task.0, &mut task_buf);
        let mut uid_buf = [0u8; 12];
        let uid_field = format_i32(i32::try_from(owner.0).unwrap_or(i32::MAX), &mut uid_buf);
        let mut len_buf = [0u8; 12];
        let len_field = format_i32(
            i32::try_from(effective.len()).unwrap_or(i32::MAX),
            &mut len_buf,
        );
        record(
            audit,
            AuditEvent::TaskCapabilitiesDerived,
            &[
                Field {
                    key: "task",
                    value: task_field,
                },
                Field {
                    key: "uid",
                    value: uid_field,
                },
                Field {
                    key: "caps",
                    value: len_field,
                },
            ],
        );
        Self {
            task,
            owner,
            user_grant,
            manifest_request,
            effective,
        }
    }

    /// Currently effective capability set.
    #[must_use]
    pub fn effective(&self) -> &CapabilitySet {
        &self.effective
    }

    /// User grant the task is bounded by.
    #[must_use]
    pub fn user_grant(&self) -> &CapabilitySet {
        &self.user_grant
    }

    /// Original manifest request.
    #[must_use]
    pub fn manifest_request(&self) -> &CapabilitySet {
        &self.manifest_request
    }

    /// Owning user identifier.
    #[must_use]
    pub fn owner(&self) -> UserId {
        self.owner
    }

    /// Task identifier carried in audit records.
    #[must_use]
    pub fn task(&self) -> TaskId {
        self.task
    }

    /// `true` if the task's effective set holds `cap`.
    ///
    /// This is the per-syscall predicate every privileged operation must
    /// consult; it never emits audit traffic itself so callers can cheaply
    /// probe membership without filling the log. The *decision* an
    /// IPC/syscall site takes after consulting this predicate is the
    /// thing recorded — that lives in the dispatch layer (Stage 2.5).
    #[must_use]
    pub fn has(&self, cap: rustos_abi::CapabilityId) -> bool {
        self.effective.contains(cap)
    }

    /// Install a delegated subset on the task.
    ///
    /// Returns [`Errno::DelegationWiden`] (and emits
    /// [`AuditEvent::TaskCapabilitiesDelegateWiden`]) if `requested`
    /// would widen the current effective set. On success the effective
    /// set is **replaced** with the delegated subset and one
    /// [`AuditEvent::TaskCapabilitiesDelegated`] is emitted. The
    /// upstream `user_grant` and `manifest_request` are not touched, so
    /// a later [`Self::derive`]-equivalent refresh is still possible by
    /// re-intersecting them.
    pub fn delegate<S: Sink + ?Sized>(
        &mut self,
        requested: &CapabilitySet,
        audit: &S,
    ) -> Result<(), Errno> {
        match self.effective.delegate(requested) {
            Ok(narrowed) => {
                self.effective = narrowed;
                let mut buf = [0u8; 16];
                record(
                    audit,
                    AuditEvent::TaskCapabilitiesDelegated,
                    &[Field {
                        key: "task",
                        value: format_hex_u64(self.task.0, &mut buf),
                    }],
                );
                Ok(())
            }
            Err(err) => {
                let mut buf = [0u8; 16];
                record(
                    audit,
                    AuditEvent::TaskCapabilitiesDelegateWiden,
                    &[Field {
                        key: "task",
                        value: format_hex_u64(self.task.0, &mut buf),
                    }],
                );
                Err(err)
            }
        }
    }

    /// Apply a signed [`CapabilityToken`] to this task.
    ///
    /// The token is verified against `authority`, the current effective
    /// set (which acts as the parent), and **this task's id as the
    /// subject** — a token minted for another task is refused here, so a
    /// stolen or misdirected token cannot be replayed onto an unrelated
    /// principal (`AGENTS.md` §5.4). On success the task's effective set
    /// is replaced with the token's payload (always a subset of the
    /// current set by [`CapabilityToken::verify`]'s own invariant).
    /// Failure modes are mapped to the same audit event as a
    /// direct [`Self::delegate`]: a forged or stale token is *security*
    /// information, not crypto trivia, and the audit trail records the
    /// security decision rather than which validation step failed
    /// (matching the rationale in `lib/caps/token.rs`).
    ///
    /// # Errors
    ///
    /// Forwards [`CapabilityToken::verify`]'s error verbatim and emits
    /// [`AuditEvent::TaskCapabilitiesDelegateWiden`].
    pub fn apply_token<S: Sink + ?Sized>(
        &mut self,
        token: &CapabilityToken,
        authority: &Ed25519PublicKey,
        epoch: RevocationEpoch,
        audit: &S,
    ) -> Result<(), Errno> {
        match token.verify(authority, &self.effective, epoch, self.task.0) {
            Ok(()) => {
                self.effective = token.caps;
                let mut buf = [0u8; 16];
                record(
                    audit,
                    AuditEvent::TaskCapabilitiesDelegated,
                    &[Field {
                        key: "task",
                        value: format_hex_u64(self.task.0, &mut buf),
                    }],
                );
                Ok(())
            }
            Err(err) => {
                let mut buf = [0u8; 16];
                record(
                    audit,
                    AuditEvent::TaskCapabilitiesDelegateWiden,
                    &[Field {
                        key: "task",
                        value: format_hex_u64(self.task.0, &mut buf),
                    }],
                );
                Err(err)
            }
        }
    }

    /// Revoke a single capability from the task.
    ///
    /// Idempotent; if the capability was not held, the call is still
    /// audited (the *attempt* is the security event) but `false` is
    /// returned. Emits one [`AuditEvent::TaskCapabilitiesRevoked`].
    pub fn revoke<S: Sink + ?Sized>(&mut self, cap: rustos_abi::CapabilityId, audit: &S) -> bool {
        let was_present = self.effective.revoke(cap);
        let mut task_buf = [0u8; 16];
        let mut cap_buf = [0u8; 12];
        record(
            audit,
            AuditEvent::TaskCapabilitiesRevoked,
            &[
                Field {
                    key: "task",
                    value: format_hex_u64(self.task.0, &mut task_buf),
                },
                Field {
                    key: "cap",
                    value: format_i32(i32::from(cap.as_u16()), &mut cap_buf),
                },
            ],
        );
        was_present
    }
}

/// Per-task capability registry — the `TaskId → TaskCapabilities` lookup
/// the syscall dispatcher consults to recover a caller's effective
/// capability set after the per-CPU current-task slot
/// (`Scheduler::current_task`, Stage 2.7 follow-up (f1)) has named the
/// caller.
///
/// The registry owns the per-task records: callers pass a freshly
/// derived [`TaskCapabilities`] in via [`Self::insert`] (at task
/// creation, after `TaskCapabilities::derive` has audited the
/// intersection) and pull it back out via [`Self::remove`] when the
/// task exits. Lookups go through [`Self::caps_for`].
///
/// # Synchronisation
///
/// `CapTable` carries no interior mutability. The owning scope —
/// `KernelState` in `kernel/core::init` — is responsible for whatever
/// lock policy is appropriate (a reader-preferring `RwLock` mirrors
/// what `Scheduler::tasks` already uses for the same shape of access
/// pattern: many concurrent syscall-context readers, occasional
/// task-creation writers). Pushing the lock outside the type keeps
/// the borrow `caps_for(&self, _) -> Option<&TaskCapabilities>`
/// natural and lets `KernelState` compose this registry with the
/// scheduler under a single lock-ordering policy
/// (`AGENTS.md` §2.1 / §2.4 — no hidden global state, no interface
/// creep).
///
/// # No ambient authority
///
/// Inserts never widen capabilities. The caller-supplied
/// [`TaskCapabilities`] has already passed through the
/// intersection-on-derive invariant in [`TaskCapabilities::derive`];
/// the registry simply stores it. There is no "make this task root"
/// shortcut and no implicit grant on lookup.
#[derive(Debug, Default)]
pub struct CapTable {
    entries: BTreeMap<TaskId, TaskCapabilities>,
}

impl CapTable {
    /// Construct an empty registry.
    #[must_use]
    pub const fn new() -> Self {
        Self {
            entries: BTreeMap::new(),
        }
    }

    /// Register a task's capabilities. The [`TaskId`] is taken from the
    /// record (`caps.task()`); callers do not pass it separately so the
    /// id and the body cannot diverge.
    ///
    /// Returns the previously-registered record, if any. A non-`None`
    /// return is an unusual condition — task ids are not recycled
    /// within a single scheduler instance (see
    /// `kernel/sched::scheduler` invariants) — but is surfaced rather
    /// than silently dropped so callers can audit / refuse it.
    pub fn insert(&mut self, caps: TaskCapabilities) -> Option<TaskCapabilities> {
        self.entries.insert(caps.task(), caps)
    }

    /// Borrow the registry entry for `task` immutably.
    ///
    /// Used by the syscall dispatcher's `cap_query` / `cap_revoke`
    /// paths: the caller's effective set is read but not mutated.
    #[must_use]
    pub fn caps_for(&self, task: TaskId) -> Option<&TaskCapabilities> {
        self.entries.get(&task)
    }

    /// Borrow the registry entry for `task` mutably. Used by the
    /// syscall dispatcher's `cap_delegate` / `cap_revoke` paths,
    /// which call `TaskCapabilities::{delegate,revoke,apply_token}`
    /// directly on the borrowed record.
    pub fn caps_for_mut(&mut self, task: TaskId) -> Option<&mut TaskCapabilities> {
        self.entries.get_mut(&task)
    }

    /// Remove the registry entry for `task`, returning it.
    ///
    /// Called by the syscall dispatcher's `exit` handler after
    /// `Scheduler::exit` has flipped the task's state; the returned
    /// record can be inspected by tests, then dropped. Returning the
    /// record (instead of swallowing it) lets the caller zero out any
    /// capability material in line with the kernel allocator's
    /// "zero-on-free for credential-holding memory" requirement
    /// (`AGENTS.md` §4).
    pub fn remove(&mut self, task: TaskId) -> Option<TaskCapabilities> {
        self.entries.remove(&task)
    }

    /// Number of tasks currently registered. Primarily for tests.
    #[must_use]
    pub fn len(&self) -> usize {
        self.entries.len()
    }

    /// `true` if no task is currently registered.
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.entries.is_empty()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::audit::RecordingSink;
    use ed25519_dalek::{Signer, SigningKey};
    use rustos_abi::{CapabilityId, ABI_VERSION_CURRENT};
    use rustos_crypto::Ed25519Signature;

    fn caps_of(items: &[CapabilityId]) -> CapabilitySet {
        let mut s = CapabilitySet::empty();
        for c in items {
            s.insert(*c);
        }
        s
    }

    #[test]
    fn derive_is_intersection() {
        let user_grant = caps_of(&[
            CapabilityId::FS_MOUNT,
            CapabilityId::NET_RAW,
            CapabilityId::AUDIT_READ,
        ]);
        let manifest_request = caps_of(&[CapabilityId::FS_MOUNT, CapabilityId::DRV_LOAD]);
        let sink = RecordingSink::new();
        let t =
            TaskCapabilities::derive(TaskId(1), UserId(1000), user_grant, manifest_request, &sink);
        // Intersection: only FS_MOUNT is in both.
        assert!(t.has(CapabilityId::FS_MOUNT));
        assert!(!t.has(CapabilityId::NET_RAW));
        assert!(!t.has(CapabilityId::DRV_LOAD));
        assert_eq!(sink.ids(), [AuditEvent::TaskCapabilitiesDerived.id().0]);
    }

    #[test]
    fn delegate_subset_succeeds_and_replaces_effective() {
        let user_grant = caps_of(&[CapabilityId::FS_MOUNT, CapabilityId::NET_RAW]);
        let manifest_request = user_grant; // identical → effective == both.
        let sink = RecordingSink::new();
        let mut t =
            TaskCapabilities::derive(TaskId(2), UserId(1), user_grant, manifest_request, &sink);
        let narrower = caps_of(&[CapabilityId::FS_MOUNT]);
        assert_eq!(t.delegate(&narrower, &sink), Ok(()));
        assert!(t.has(CapabilityId::FS_MOUNT));
        assert!(!t.has(CapabilityId::NET_RAW));
        assert_eq!(
            sink.ids(),
            [
                AuditEvent::TaskCapabilitiesDerived.id().0,
                AuditEvent::TaskCapabilitiesDelegated.id().0,
            ]
        );
    }

    #[test]
    fn delegate_widening_is_refused_with_audit() {
        let user_grant = caps_of(&[CapabilityId::FS_MOUNT]);
        let manifest_request = user_grant;
        let sink = RecordingSink::new();
        let mut t =
            TaskCapabilities::derive(TaskId(3), UserId(1), user_grant, manifest_request, &sink);
        let wider = caps_of(&[CapabilityId::FS_MOUNT, CapabilityId::DRV_KERNEL]);
        assert_eq!(t.delegate(&wider, &sink), Err(Errno::DelegationWiden));
        // The effective set is unchanged.
        assert!(t.has(CapabilityId::FS_MOUNT));
        assert!(!t.has(CapabilityId::DRV_KERNEL));
        assert_eq!(
            sink.ids(),
            [
                AuditEvent::TaskCapabilitiesDerived.id().0,
                AuditEvent::TaskCapabilitiesDelegateWiden.id().0,
            ]
        );
    }

    #[test]
    fn revoke_removes_capability_and_returns_previous_state() {
        let user_grant = caps_of(&[CapabilityId::FS_MOUNT, CapabilityId::NET_RAW]);
        let sink = RecordingSink::new();
        let mut t = TaskCapabilities::derive(TaskId(4), UserId(1), user_grant, user_grant, &sink);
        assert!(t.revoke(CapabilityId::FS_MOUNT, &sink));
        assert!(!t.has(CapabilityId::FS_MOUNT));
        // Revoking again is idempotent (returns false) but still
        // produces an audit record per `AGENTS.md` §5.4.4.
        assert!(!t.revoke(CapabilityId::FS_MOUNT, &sink));
        assert_eq!(
            sink.ids(),
            [
                AuditEvent::TaskCapabilitiesDerived.id().0,
                AuditEvent::TaskCapabilitiesRevoked.id().0,
                AuditEvent::TaskCapabilitiesRevoked.id().0,
            ]
        );
    }

    #[test]
    fn token_application_accepts_signed_subset() {
        let signing = SigningKey::from_bytes(&[0x11; 32]);
        let authority = Ed25519PublicKey::from_bytes(signing.verifying_key().as_bytes()).unwrap();

        let user_grant = caps_of(&[CapabilityId::FS_MOUNT, CapabilityId::AUDIT_READ]);
        let sink = RecordingSink::new();
        let mut t = TaskCapabilities::derive(TaskId(5), UserId(1), user_grant, user_grant, &sink);

        let epoch = RevocationEpoch(3);
        let narrowed = caps_of(&[CapabilityId::FS_MOUNT]);
        let body =
            CapabilityToken::signing_input(ABI_VERSION_CURRENT, t.task().0, epoch, &narrowed);
        let sig = signing.sign(&body);
        let token = CapabilityToken {
            abi_version: ABI_VERSION_CURRENT,
            subject: t.task().0,
            epoch,
            caps: narrowed,
            signature: Ed25519Signature::from_bytes(sig.to_bytes()),
        };
        assert_eq!(t.apply_token(&token, &authority, epoch, &sink), Ok(()));
        assert!(t.has(CapabilityId::FS_MOUNT));
        assert!(!t.has(CapabilityId::AUDIT_READ));
    }

    #[test]
    fn token_for_another_task_is_refused() {
        // A correctly-signed, current-epoch, subset token issued to a
        // *different* task must not apply here: binding to the subject
        // forecloses replaying a stolen token onto another principal
        // (`AGENTS.md` §5.4). The effective set must be left untouched.
        let signing = SigningKey::from_bytes(&[0x33; 32]);
        let authority = Ed25519PublicKey::from_bytes(signing.verifying_key().as_bytes()).unwrap();

        let user_grant = caps_of(&[CapabilityId::FS_MOUNT, CapabilityId::AUDIT_READ]);
        let sink = RecordingSink::new();
        let mut t = TaskCapabilities::derive(TaskId(9), UserId(1), user_grant, user_grant, &sink);

        let epoch = RevocationEpoch(3);
        let narrowed = caps_of(&[CapabilityId::FS_MOUNT]);
        // Sign the token for some other task, not `t`.
        let other_subject = t.task().0 ^ 0x1;
        let body =
            CapabilityToken::signing_input(ABI_VERSION_CURRENT, other_subject, epoch, &narrowed);
        let sig = signing.sign(&body);
        let token = CapabilityToken {
            abi_version: ABI_VERSION_CURRENT,
            subject: other_subject,
            epoch,
            caps: narrowed,
            signature: Ed25519Signature::from_bytes(sig.to_bytes()),
        };
        assert_eq!(
            t.apply_token(&token, &authority, epoch, &sink),
            Err(Errno::NotFound),
        );
        // The task keeps its full grant; the foreign token changed nothing.
        assert!(t.has(CapabilityId::FS_MOUNT));
        assert!(t.has(CapabilityId::AUDIT_READ));
        assert!(sink
            .ids()
            .contains(&AuditEvent::TaskCapabilitiesDelegateWiden.id().0));
    }

    #[test]
    fn token_with_revoked_epoch_is_refused() {
        let signing = SigningKey::from_bytes(&[0x22; 32]);
        let authority = Ed25519PublicKey::from_bytes(signing.verifying_key().as_bytes()).unwrap();

        let user_grant = caps_of(&[CapabilityId::FS_MOUNT]);
        let sink = RecordingSink::new();
        let mut t = TaskCapabilities::derive(TaskId(6), UserId(1), user_grant, user_grant, &sink);

        // Sign for epoch 1 but verify under epoch 2 — mass revocation.
        let issued_at = RevocationEpoch(1);
        let current = RevocationEpoch(2);
        let body =
            CapabilityToken::signing_input(ABI_VERSION_CURRENT, t.task().0, issued_at, &user_grant);
        let sig = signing.sign(&body);
        let token = CapabilityToken {
            abi_version: ABI_VERSION_CURRENT,
            subject: t.task().0,
            epoch: issued_at,
            caps: user_grant,
            signature: Ed25519Signature::from_bytes(sig.to_bytes()),
        };
        assert_eq!(
            t.apply_token(&token, &authority, current, &sink),
            Err(Errno::NotFound),
        );
        // Audit records the refusal under the delegation-widen event id
        // (single failure path; see docstring for rationale).
        assert!(sink
            .ids()
            .contains(&AuditEvent::TaskCapabilitiesDelegateWiden.id().0));
    }

    #[test]
    fn uid_zero_gets_no_extra_powers() {
        // A uid==0 task with an empty user grant ends up with an empty
        // effective set, even when the manifest requests the universe.
        let manifest_request = caps_of(&[
            CapabilityId::FS_MOUNT,
            CapabilityId::DRV_KERNEL,
            CapabilityId::USER_ADMIN,
        ]);
        let sink = RecordingSink::new();
        let t = TaskCapabilities::derive(
            TaskId(7),
            UserId(0),
            CapabilitySet::empty(), // ambient powers? no.
            manifest_request,
            &sink,
        );
        assert!(t.effective().is_empty());
    }

    // ---------------------------------------------------------------
    // Stage 2.7 follow-up (f2): per-task CapTable registry.
    // ---------------------------------------------------------------

    fn make_caps(task: u64, caps: &[rustos_abi::CapabilityId]) -> TaskCapabilities {
        let grant = caps_of(caps);
        let sink = RecordingSink::new();
        TaskCapabilities::derive(TaskId(task), UserId(1000), grant, grant, &sink)
    }

    #[test]
    fn captable_is_empty_when_constructed() {
        let table = CapTable::new();
        assert!(table.is_empty());
        assert_eq!(table.len(), 0);
        assert!(table.caps_for(TaskId(1)).is_none());
    }

    #[test]
    fn captable_insert_then_lookup_returns_record() {
        let mut table = CapTable::new();
        let caps = make_caps(7, &[rustos_abi::CapabilityId::FS_MOUNT]);
        assert!(table.insert(caps).is_none());
        assert_eq!(table.len(), 1);
        let got = table.caps_for(TaskId(7)).expect("registered");
        assert!(got.has(rustos_abi::CapabilityId::FS_MOUNT));
        assert_eq!(got.task(), TaskId(7));
    }

    #[test]
    fn captable_lookup_miss_returns_none() {
        let mut table = CapTable::new();
        let caps = make_caps(1, &[rustos_abi::CapabilityId::FS_MOUNT]);
        table.insert(caps);
        assert!(table.caps_for(TaskId(2)).is_none());
    }

    #[test]
    fn captable_insert_returns_previous_record_on_duplicate_id() {
        // Task ids are not recycled in `kernel/sched`, so a duplicate
        // insert is a real anomaly. Surface it via the return value so
        // a caller can audit / refuse rather than silently lose state.
        let mut table = CapTable::new();
        table.insert(make_caps(3, &[rustos_abi::CapabilityId::FS_MOUNT]));
        let displaced = table.insert(make_caps(3, &[rustos_abi::CapabilityId::NET_RAW]));
        let prior = displaced.expect("first record returned");
        assert!(prior.has(rustos_abi::CapabilityId::FS_MOUNT));
        // The registry now reflects the second insert only.
        assert_eq!(table.len(), 1);
        let current = table.caps_for(TaskId(3)).expect("present");
        assert!(current.has(rustos_abi::CapabilityId::NET_RAW));
        assert!(!current.has(rustos_abi::CapabilityId::FS_MOUNT));
    }

    #[test]
    fn captable_remove_returns_and_evicts_record() {
        let mut table = CapTable::new();
        table.insert(make_caps(9, &[rustos_abi::CapabilityId::FS_MOUNT]));
        let evicted = table.remove(TaskId(9)).expect("present before remove");
        assert!(evicted.has(rustos_abi::CapabilityId::FS_MOUNT));
        assert!(table.is_empty());
        assert!(table.caps_for(TaskId(9)).is_none());
        // Idempotent: a second remove returns None and leaves the
        // registry empty.
        assert!(table.remove(TaskId(9)).is_none());
        assert!(table.is_empty());
    }

    #[test]
    fn captable_caps_for_mut_supports_revoke_in_place() {
        // The dispatcher's `cap_revoke` handler reaches `TaskCapabilities`
        // through `caps_for_mut`; this test exercises that path so the
        // mutable lookup is covered by the same security-relevant
        // assertions as `caps_for`.
        let mut table = CapTable::new();
        table.insert(make_caps(
            11,
            &[
                rustos_abi::CapabilityId::FS_MOUNT,
                rustos_abi::CapabilityId::NET_RAW,
            ],
        ));
        let sink = RecordingSink::new();
        let entry = table.caps_for_mut(TaskId(11)).expect("present");
        assert!(entry.revoke(rustos_abi::CapabilityId::FS_MOUNT, &sink));
        let after = table.caps_for(TaskId(11)).expect("still present");
        assert!(!after.has(rustos_abi::CapabilityId::FS_MOUNT));
        assert!(after.has(rustos_abi::CapabilityId::NET_RAW));
    }

    #[test]
    fn captable_stores_multiple_tasks_independently() {
        let mut table = CapTable::new();
        table.insert(make_caps(1, &[rustos_abi::CapabilityId::FS_MOUNT]));
        table.insert(make_caps(2, &[rustos_abi::CapabilityId::NET_RAW]));
        table.insert(make_caps(3, &[rustos_abi::CapabilityId::DRV_LOAD]));
        assert_eq!(table.len(), 3);
        assert!(table
            .caps_for(TaskId(1))
            .expect("1")
            .has(rustos_abi::CapabilityId::FS_MOUNT));
        assert!(table
            .caps_for(TaskId(2))
            .expect("2")
            .has(rustos_abi::CapabilityId::NET_RAW));
        assert!(table
            .caps_for(TaskId(3))
            .expect("3")
            .has(rustos_abi::CapabilityId::DRV_LOAD));
        // Removing one leaves the others intact (no aliasing).
        table.remove(TaskId(2));
        assert_eq!(table.len(), 2);
        assert!(table.caps_for(TaskId(2)).is_none());
        assert!(table.caps_for(TaskId(1)).is_some());
        assert!(table.caps_for(TaskId(3)).is_some());
    }
}
