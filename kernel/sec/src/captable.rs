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
    /// The token is verified against `authority` and the current
    /// effective set (which acts as the parent); on success the task's
    /// effective set is replaced with the token's payload (always a
    /// subset of the current set by [`CapabilityToken::verify`]'s own
    /// invariant). Failure modes are mapped to the same audit event as a
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
        match token.verify(authority, &self.effective, epoch) {
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
}
