//! Per-task capability state.
//!
//! Every running task has a [`TaskCapabilities`] record. The effective
//! capability set is **the intersection** of two narrower-or-equal sets:
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
use alloc::vec::Vec;

use rustos_abi::{CapabilitySummary, Errno, Origin, ProcId, TrustDomain};
use rustos_caps::{CapabilitySet, CapabilityToken, RevocationEpoch};
use rustos_crypto::Ed25519PublicKey;
use rustos_log::{Field, Sink};

use crate::audit::{record, AuditEvent};
use crate::identity::{format_hex_u64, format_i32, GroupId, UserId};

/// Numeric task identifier carried by audit records.
///
/// Distinct from `pid_t`: `TaskId` is the kernel's internal handle for a
/// schedulable entity. `kernel/sched` produces these; we accept them
/// verbatim for audit attribution.
#[repr(transparent)]
#[derive(Copy, Clone, Debug, Default, Eq, PartialEq, Hash, Ord, PartialOrd)]
pub struct TaskId(pub u64);

/// Maximum length, in bytes, of a kernel-attested process name.
///
/// Reuses the one process-name bound `rustos_abi` already defines for the
/// System Information process record, so the attested audit name and any
/// reported process name can never disagree on length — a single definition.
pub const PROC_NAME_MAX: usize = rustos_abi::sysinfo::PROCESS_NAME_MAX;

/// A kernel-attested, bounded process name.
///
/// Set kernel-side at process admission from trusted state — the resolved
/// executable path, or a fixed name for the kernel's own principals — and
/// never from caller-supplied bytes, so an audit consumer may trust it to
/// name the acting process. Stored inline (no allocation) so it can be read
/// on the audit path, and it holds only a valid-UTF-8 prefix so
/// [`as_str`](Self::as_str) is total.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ProcName {
    buf: [u8; PROC_NAME_MAX],
    len: usize,
}

impl ProcName {
    /// The empty name — the default for a record whose admit path attested
    /// none (kernel threads, in-kernel binder / device-host records).
    pub const EMPTY: Self = Self {
        buf: [0u8; PROC_NAME_MAX],
        len: 0,
    };

    /// Build a name from `bytes`, keeping the largest valid-UTF-8 prefix that
    /// fits in [`PROC_NAME_MAX`].
    ///
    /// A process name is display/attribution metadata, not a security
    /// decision, so an over-long or non-UTF-8-boundary input is bounded to
    /// its largest valid prefix rather than rejected — never storing invalid
    /// UTF-8 (so [`as_str`](Self::as_str) is total) and never storing a
    /// caller-trusted value (the sole callers pass kernel-resolved bytes).
    #[must_use]
    pub fn from_bytes_truncating(bytes: &[u8]) -> Self {
        let capped = if bytes.len() > PROC_NAME_MAX {
            &bytes[..PROC_NAME_MAX]
        } else {
            bytes
        };
        let valid = match core::str::from_utf8(capped) {
            Ok(s) => s.len(),
            Err(e) => e.valid_up_to(),
        };
        let mut buf = [0u8; PROC_NAME_MAX];
        buf[..valid].copy_from_slice(&capped[..valid]);
        Self { buf, len: valid }
    }

    /// Borrow the name as a string; always valid UTF-8 by construction.
    #[must_use]
    pub fn as_str(&self) -> &str {
        core::str::from_utf8(&self.buf[..self.len]).unwrap_or("")
    }

    /// `true` if no name was attested.
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.len == 0
    }
}

impl Default for ProcName {
    fn default() -> Self {
        Self::EMPTY
    }
}

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
    /// Primary group of the task's kernel-attested credential.
    ///
    /// Snapshotted at process creation from the authoritative identity table
    /// (a switch to a target user) or inherited from the spawning parent's own
    /// record; never derived from caller-supplied bytes. Defaults to
    /// [`GroupId::default`] (gid 0, the system group) for a record built
    /// before a credential is attached (kernel principals, a plain
    /// [`Self::derive`]). It is identity used by the filesystem permission
    /// model and reported in the attested [`Origin`]; it confers no capability
    /// (capabilities flow only through `effective`).
    primary_gid: GroupId,
    /// Supplementary groups of the task's kernel-attested credential.
    ///
    /// Bounded by the identity table's verifier when the credential is
    /// resolved, so a task can never carry an unbounded set. Snapshotted or
    /// inherited exactly like [`primary_gid`](Self::primary_gid); empty for a
    /// record with no attached credential.
    supplementary_gids: Vec<GroupId>,
    /// Maximum the owner's user grant ever allows on this task. Acts as
    /// the *upper bound* for every subsequent operation; nothing in this
    /// module can grow `effective` past this set.
    user_grant: CapabilitySet,
    /// What the binary's manifest asked for (already verified by
    /// [`crate::verify_manifest`]).
    manifest_request: CapabilitySet,
    /// Currently effective set. Always a subset of `user_grant ∩ manifest_request`.
    effective: CapabilitySet,
    /// Kernel-generated process-instance identity, distinct from the
    /// reusable scheduler [`TaskId`]. Defaults to [`ProcId::KERNEL`] —
    /// the sentinel for a schedulable entity that is not a distinct user
    /// process instance (kernel threads, IPC-binder/device-host records).
    /// The two process-admit paths replace it with a minted value through
    /// [`Self::with_proc_id`]; it is set kernel-side and never derived from
    /// any caller-supplied bytes, so a task can neither forge nor influence
    /// it.
    proc_id: ProcId,
    /// Process-instance identity of the task's **parent** — the process
    /// that spawned it — distinct from the parent's reusable numeric id, so
    /// parentage survives PID reuse exactly as [`proc_id`](Self::proc_id)
    /// does for the task itself. Defaults to [`ProcId::KERNEL`], the
    /// sentinel for a kernel-parented task with no distinct user-process
    /// parent (PID 1, the storage bootstrap-floor drivers, kernel threads).
    /// The spawn admit path replaces it with the spawning parent's attested
    /// identity through [`Self::with_parent_proc_id`]; like `proc_id` it is
    /// set kernel-side from the parent's own kernel-held record and never
    /// from caller-supplied bytes, so a task cannot forge or influence its
    /// recorded parentage.
    parent_proc_id: ProcId,
    /// Kernel-attested process name — the resolved executable's basename for
    /// a spawned process, a fixed name for the kernel's own principals (PID 1,
    /// the storage bootstrap-floor drivers). Defaults to [`ProcName::EMPTY`].
    /// The process-admit paths set it through [`Self::with_name`] from
    /// kernel-resolved state, never from caller-supplied bytes, so an audit
    /// consumer may trust it to name the acting process.
    name: ProcName,
    /// Kernel-attested monotonic timestamp of the task's admission — the
    /// value the Arch HAL monotonic counter (`ticks_now`) read at the instant
    /// the process was admitted. Defaults to `0`, the sentinel for a task
    /// admitted before user-process start tracking runs (PID 1, the storage
    /// bootstrap-floor drivers, kernel threads — the boot principals), which
    /// began at boot. The production process-admit path sets it through
    /// [`Self::with_start_time`] from the kernel's own monotonic clock, never
    /// from caller-supplied bytes, so an audit or origin consumer may trust it
    /// to order and age a process instance, and to distinguish two lifetimes
    /// that reused a numeric id even within one monotonic epoch. It confers no
    /// capability.
    start_time: u64,
}

impl TaskCapabilities {
    /// Derive a task's effective capabilities from its user grant and the
    /// verified manifest request.
    ///
    /// The effective set is the intersection of the two inputs. Emits exactly one
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
                    value: rustos_log::FieldValue::Str(task_field),
                },
                Field {
                    key: "uid",
                    value: rustos_log::FieldValue::Str(uid_field),
                },
                Field {
                    key: "caps",
                    value: rustos_log::FieldValue::Str(len_field),
                },
            ],
        );
        Self {
            task,
            owner,
            primary_gid: GroupId::default(),
            supplementary_gids: Vec::new(),
            user_grant,
            manifest_request,
            effective,
            proc_id: ProcId::KERNEL,
            parent_proc_id: ProcId::KERNEL,
            name: ProcName::EMPTY,
            start_time: 0,
        }
    }

    /// Attach the task's kernel-attested group credential (primary group and
    /// supplementary groups) to this record.
    ///
    /// Consumed and returned so the process-admit path can set the credential
    /// inline before inserting the record into the [`CapTable`], mirroring
    /// [`Self::with_proc_id`]. Only the kernel's process-admit sites call it,
    /// passing groups resolved from the authoritative identity table (a
    /// spawn-as-user switch) or copied from the spawning parent's own attested
    /// record (inherit) — never a caller-supplied value. The groups are
    /// identity for the filesystem permission model; they confer no
    /// capability. A record with no attached credential keeps gid 0 and an
    /// empty supplementary set.
    #[must_use]
    pub fn with_credential(
        mut self,
        primary_gid: GroupId,
        supplementary_gids: Vec<GroupId>,
    ) -> Self {
        self.primary_gid = primary_gid;
        self.supplementary_gids = supplementary_gids;
        self
    }

    /// The primary group of the task's attested credential.
    #[must_use]
    pub fn primary_gid(&self) -> GroupId {
        self.primary_gid
    }

    /// The supplementary groups of the task's attested credential.
    #[must_use]
    pub fn supplementary_gids(&self) -> &[GroupId] {
        &self.supplementary_gids
    }

    /// Attach a minted process-instance identity to this record.
    ///
    /// Consumed and returned so the process-admit path can set the identity
    /// inline before inserting the record into the [`CapTable`]. Only the
    /// kernel's two process-admit sites call this; every other producer
    /// leaves the [`ProcId::KERNEL`] sentinel.
    #[must_use]
    pub fn with_proc_id(mut self, proc_id: ProcId) -> Self {
        self.proc_id = proc_id;
        self
    }

    /// The task's kernel-generated process-instance identity.
    ///
    /// Returns [`ProcId::KERNEL`] for a record that is not a distinct user
    /// process instance. The value is attested by the kernel — never
    /// caller-supplied — so an audit or origin consumer may trust it.
    #[must_use]
    pub fn proc_id(&self) -> ProcId {
        self.proc_id
    }

    /// Attach the spawning parent's process-instance identity to this record.
    ///
    /// Consumed and returned so the process-admit path can set the parentage
    /// inline before inserting the record into the [`CapTable`], mirroring
    /// [`Self::with_proc_id`]. Only the kernel's process-admit sites call it,
    /// passing the parent's *attested* [`proc_id`](Self::proc_id) read from
    /// the parent's own kernel-held record — never a caller-supplied value.
    /// A kernel-parented task (PID 1, the storage bootstrap-floor drivers)
    /// leaves the [`ProcId::KERNEL`] sentinel.
    #[must_use]
    pub fn with_parent_proc_id(mut self, parent_proc_id: ProcId) -> Self {
        self.parent_proc_id = parent_proc_id;
        self
    }

    /// The process-instance identity of the task's parent.
    ///
    /// Returns [`ProcId::KERNEL`] for a kernel-parented task (no distinct
    /// user-process parent). The value is attested by the kernel — read from
    /// the parent's own kernel-held record, never caller-supplied — so an
    /// audit or origin consumer may trust it to attribute a task to the exact
    /// parent instance that spawned it, even across PID reuse.
    #[must_use]
    pub fn parent_proc_id(&self) -> ProcId {
        self.parent_proc_id
    }

    /// Attach the kernel-attested process name to this record.
    ///
    /// Consumed and returned so the process-admit path can set the name
    /// inline before inserting the record into the [`CapTable`], mirroring
    /// [`Self::with_proc_id`]. Only the kernel's process-admit sites call it,
    /// passing a name derived from kernel-resolved state (the executable's
    /// basename, or a fixed name for a kernel principal) — never a
    /// caller-supplied value. A record with no attested name keeps
    /// [`ProcName::EMPTY`].
    #[must_use]
    pub fn with_name(mut self, name: ProcName) -> Self {
        self.name = name;
        self
    }

    /// Attach the kernel-attested monotonic admission timestamp to this
    /// record.
    ///
    /// Consumed and returned so the process-admit path can set it inline
    /// before inserting the record into the [`CapTable`], mirroring
    /// [`Self::with_proc_id`]. Only the kernel's process-admit path calls it,
    /// passing the value the Arch HAL monotonic counter read at admission —
    /// never a caller-supplied value. A record with no attested start time
    /// keeps the `0` boot/kernel-principal sentinel.
    #[must_use]
    pub fn with_start_time(mut self, start_time: u64) -> Self {
        self.start_time = start_time;
        self
    }

    /// The task's kernel-attested monotonic admission timestamp.
    ///
    /// Returns `0` for a boot or kernel principal admitted before
    /// user-process start tracking runs. The value is attested by the kernel
    /// — read from the monotonic clock at admission, never caller-supplied —
    /// so an audit or origin consumer may trust it to order and age the
    /// process instance.
    #[must_use]
    pub fn start_time(&self) -> u64 {
        self.start_time
    }

    /// The task's kernel-attested process name (empty if none was attested).
    ///
    /// Read from this record's own kernel-held state — never caller-supplied
    /// — so an audit consumer may trust it to name the acting process.
    #[must_use]
    pub fn name(&self) -> &str {
        self.name.as_str()
    }

    /// Produce the kernel-attested [`Origin`] of this task.
    ///
    /// Every field is read from this record's own kernel-held state — never
    /// from anything a caller supplied — so the result is authoritative: a
    /// task can neither forge another principal's origin nor inflate its own.
    /// The trust domain is [`TrustDomain::Kernel`] for the
    /// [`ProcId::KERNEL`] sentinel (kernel threads and in-kernel binder /
    /// device-host records) and [`TrustDomain::User`] for a minted process
    /// instance. The capability summary is the effective set's wire image —
    /// the non-secret membership bitmap, carrying no capability *tokens*.
    #[must_use]
    pub fn attest_origin(&self) -> Origin {
        let trust_domain = if self.proc_id.is_kernel() {
            TrustDomain::Kernel
        } else {
            TrustDomain::User
        };
        let capabilities = CapabilitySummary::from_raw(self.effective.to_le_bytes());
        Origin::new(
            trust_domain,
            self.owner.0,
            self.primary_gid.0,
            self.task.0,
            self.proc_id,
            capabilities,
        )
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
                        value: rustos_log::FieldValue::Str(format_hex_u64(self.task.0, &mut buf)),
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
                        value: rustos_log::FieldValue::Str(format_hex_u64(self.task.0, &mut buf)),
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
    /// principal. On success the task's effective set
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
                        value: rustos_log::FieldValue::Str(format_hex_u64(self.task.0, &mut buf)),
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
                        value: rustos_log::FieldValue::Str(format_hex_u64(self.task.0, &mut buf)),
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
                    value: rustos_log::FieldValue::Str(format_hex_u64(self.task.0, &mut task_buf)),
                },
                Field {
                    key: "cap",
                    value: rustos_log::FieldValue::Str(format_i32(
                        i32::from(cap.as_u16()),
                        &mut cap_buf,
                    )),
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
/// (no hidden global state, no interface
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
    /// "zero-on-free for credential-holding memory" requirement.
    pub fn remove(&mut self, task: TaskId) -> Option<TaskCapabilities> {
        self.entries.remove(&task)
    }

    /// Iterate every registered task's attested capability record, in
    /// ascending [`TaskId`] order.
    ///
    /// The order is the `BTreeMap` key order, so it is stable across calls
    /// as long as the registry is unchanged — letting the System Information
    /// introspection source page a consistent process list. Every field of
    /// each [`TaskCapabilities`] is kernel-attested (minted at admit), so a
    /// consumer building a process view reads authoritative identity, never
    /// a caller claim.
    pub fn iter(&self) -> impl Iterator<Item = &TaskCapabilities> {
        self.entries.values()
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
    fn proc_id_defaults_to_kernel_sentinel_and_with_proc_id_attaches() {
        use rustos_abi::ProcId;
        let grant = caps_of(&[CapabilityId::FS_MOUNT]);
        let sink = RecordingSink::new();
        let base = TaskCapabilities::derive(TaskId(7), UserId(1000), grant, grant, &sink);
        // A freshly-derived record carries the kernel sentinel: it is not a
        // distinct user process instance until a minted id is attached.
        assert_eq!(base.proc_id(), ProcId::KERNEL);
        assert!(base.proc_id().is_kernel());

        let minted = ProcId::from_raw([0xAB; 16]);
        let admitted = base.with_proc_id(minted);
        assert_eq!(admitted.proc_id(), minted);
        assert!(!admitted.proc_id().is_kernel());
        // Attaching the identity changes nothing about the capability set.
        assert!(admitted.has(CapabilityId::FS_MOUNT));
    }

    #[test]
    fn parent_proc_id_defaults_to_kernel_sentinel_and_with_parent_attaches() {
        use rustos_abi::ProcId;
        let grant = caps_of(&[CapabilityId::FS_MOUNT]);
        let sink = RecordingSink::new();
        let base = TaskCapabilities::derive(TaskId(9), UserId(1000), grant, grant, &sink);
        // A freshly-derived record is kernel-parented until the admit path
        // attaches the spawning parent's attested identity.
        assert_eq!(base.parent_proc_id(), ProcId::KERNEL);
        assert!(base.parent_proc_id().is_kernel());

        let parent = ProcId::from_raw([0xC3; 16]);
        let child = ProcId::from_raw([0xAB; 16]);
        let admitted = base.with_proc_id(child).with_parent_proc_id(parent);
        // The task's own identity and its parentage are independent fields:
        // attaching one never disturbs the other or the capability set.
        assert_eq!(admitted.proc_id(), child);
        assert_eq!(admitted.parent_proc_id(), parent);
        assert!(!admitted.parent_proc_id().is_kernel());
        assert!(admitted.has(CapabilityId::FS_MOUNT));
    }

    #[test]
    fn proc_name_defaults_empty_and_with_name_attaches() {
        let grant = caps_of(&[CapabilityId::FS_MOUNT]);
        let sink = RecordingSink::new();
        let base = TaskCapabilities::derive(TaskId(11), UserId(1000), grant, grant, &sink);
        // A freshly-derived record has no attested name.
        assert_eq!(base.name(), "");

        let named = base.with_name(ProcName::from_bytes_truncating(b"sysinfod"));
        assert_eq!(named.name(), "sysinfod");
        // Attaching the name changes nothing about the capability set.
        assert!(named.has(CapabilityId::FS_MOUNT));
    }

    #[test]
    fn start_time_defaults_to_boot_sentinel_and_with_start_time_attaches() {
        let grant = caps_of(&[CapabilityId::FS_MOUNT]);
        let sink = RecordingSink::new();
        let base = TaskCapabilities::derive(TaskId(15), UserId(1000), grant, grant, &sink);
        // A freshly-derived record carries the `0` boot/kernel-principal
        // sentinel until an admission timestamp is attested.
        assert_eq!(base.start_time(), 0);

        let started = base.with_start_time(0x1234_5678_9abc_def0);
        assert_eq!(started.start_time(), 0x1234_5678_9abc_def0);
        // Attaching the start time is identity only: it changes nothing about
        // the capability set, and it is independent of the other attested
        // fields (proc_id / name stay at their defaults).
        assert!(started.has(CapabilityId::FS_MOUNT));
        assert!(started.proc_id().is_kernel());
        assert_eq!(started.name(), "");
    }

    #[test]
    fn proc_name_bounds_to_largest_valid_utf8_prefix() {
        // The empty name is empty and its default matches.
        assert_eq!(ProcName::EMPTY.as_str(), "");
        assert!(ProcName::EMPTY.is_empty());
        assert_eq!(ProcName::default(), ProcName::EMPTY);

        // An over-long input is bounded to PROC_NAME_MAX bytes.
        let long = [b'a'; PROC_NAME_MAX + 8];
        let name = ProcName::from_bytes_truncating(&long);
        assert_eq!(name.as_str().len(), PROC_NAME_MAX);
        assert!(name.as_str().bytes().all(|b| b == b'a'));

        // A cut that would land inside a multi-byte code point keeps only
        // the largest valid prefix, so `as_str` never observes invalid UTF-8.
        // "é" (0xC3 0xA9) repeated fills 2 bytes each; PROC_NAME_MAX (32) is
        // even, so the boundary lands cleanly, but a trailing lone lead byte
        // must be dropped:
        let mut bytes = alloc::vec![0u8; PROC_NAME_MAX];
        for chunk in bytes.chunks_mut(2) {
            chunk[0] = 0xC3;
            if chunk.len() > 1 {
                chunk[1] = 0xA9;
            }
        }
        // Append a stray lead byte past the cap; it is dropped with the cap.
        bytes.push(0xC3);
        let accented = ProcName::from_bytes_truncating(&bytes);
        // Valid UTF-8 by construction, and every code point is "é".
        assert!(accented.as_str().chars().all(|c| c == 'é'));
        assert_eq!(accented.as_str().len(), PROC_NAME_MAX);

        // A trailing lone lead byte within the cap is dropped, not stored.
        let truncated = ProcName::from_bytes_truncating(&[b'h', b'i', 0xC3]);
        assert_eq!(truncated.as_str(), "hi");
    }

    #[test]
    fn credential_defaults_empty_and_with_credential_attaches() {
        let grant = caps_of(&[CapabilityId::FS_MOUNT]);
        let sink = RecordingSink::new();
        let base = TaskCapabilities::derive(TaskId(13), UserId(1000), grant, grant, &sink);
        // A freshly-derived record carries the system group and no
        // supplementary groups until a credential is attached.
        assert_eq!(base.primary_gid(), GroupId::default());
        assert!(base.supplementary_gids().is_empty());

        let cred = base
            .clone()
            .with_credential(GroupId(50), alloc::vec![GroupId(60), GroupId(70)]);
        assert_eq!(cred.primary_gid(), GroupId(50));
        assert_eq!(cred.supplementary_gids(), &[GroupId(60), GroupId(70)]);
        // Attaching the credential changes nothing about the capability set.
        assert!(cred.has(CapabilityId::FS_MOUNT));
    }

    #[test]
    fn attest_origin_carries_the_attested_primary_gid() {
        use rustos_abi::ProcId;
        let grant = caps_of(&[CapabilityId::FS_MOUNT]);
        let sink = RecordingSink::new();
        let proc = TaskCapabilities::derive(TaskId(44), UserId(1000), grant, grant, &sink)
            .with_proc_id(ProcId::from_raw([0x5A; 16]))
            .with_credential(GroupId(77), alloc::vec![]);
        assert_eq!(proc.attest_origin().gid(), 77);
    }

    #[test]
    fn attest_origin_is_built_from_kernel_state() {
        use rustos_abi::{ProcId, TrustDomain};
        let grant = caps_of(&[CapabilityId::FS_MOUNT, CapabilityId::SYSINFO_GLOBAL]);
        let sink = RecordingSink::new();
        // A kernel-domain record (no minted proc_id) attests as Kernel.
        let kernel_task = TaskCapabilities::derive(TaskId(3), UserId(0), grant, grant, &sink);
        let kernel_origin = kernel_task.attest_origin();
        assert_eq!(kernel_origin.trust_domain(), TrustDomain::Kernel);
        assert!(kernel_origin.proc_id().is_kernel());

        // A minted process instance attests as User, carrying its own uid,
        // pid, proc_id, and a capability summary mirroring its effective set.
        let minted = ProcId::from_raw([0x5A; 16]);
        let proc = TaskCapabilities::derive(TaskId(42), UserId(1000), grant, grant, &sink)
            .with_proc_id(minted);
        let origin = proc.attest_origin();
        assert_eq!(origin.trust_domain(), TrustDomain::User);
        assert_eq!(origin.uid(), 1000);
        assert_eq!(origin.pid(), 42);
        assert_eq!(origin.proc_id(), minted);
        assert!(origin.capabilities().holds_cap(CapabilityId::FS_MOUNT));
        assert!(origin
            .capabilities()
            .holds_cap(CapabilityId::SYSINFO_GLOBAL));
        assert!(!origin.capabilities().holds_cap(CapabilityId::NET_RAW));
        // The summary is exactly the effective set's wire image.
        assert_eq!(
            origin.capabilities().as_bytes(),
            &proc.effective().to_le_bytes()
        );
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
        // produces an audit record.
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
        // forecloses replaying a stolen token onto another principal. The effective set must be left untouched.
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
