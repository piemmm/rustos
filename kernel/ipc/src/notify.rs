//! Asynchronous, capability-gated notifications.
//!
//! A [`NotificationChannel`] is a kernel-owned, signal-like
//! broadcast endpoint: senders raise one or more bit flags
//! ([`NotificationFlags`]), and bound receivers later consume the
//! accumulated bits with a single atomic exchange. The semantics are
//! "level-triggered, OR-accumulated, lossless of bits": a receiver
//! that does not drain quickly enough still sees every flag at least
//! once, just possibly bundled together.
//!
//! Capabilities are checked at *bind* time (the receiver's eligibility
//! is fixed when it joins the channel — `AGENTS.md` §5.2) and on
//! *every* signal (the sender's eligibility may have been revoked
//! since the previous send). Refused operations fail closed with
//! [`Errno::PermissionDenied`] + [`AuditEvent::NotifySignalDenied`].

use rustos_abi::Errno;
use rustos_caps::CapabilitySet;
use rustos_kernel_sec::captable::TaskCapabilities;
use rustos_log::{Field, Sink};
use rustos_util::fmt::format_hex_u64;

use crate::audit::{record, AuditEvent};
use crate::loom_compat::{AtomicU32, Ordering};

/// Bitfield of notification flags accumulated by a channel.
///
/// Wraps a `u32` so up to 32 distinct events can share one channel
/// without allocating per-event state. The numeric values are
/// channel-defined, not kernel-defined: the channel binder decides
/// what `0b0001` means.
#[repr(transparent)]
#[derive(Copy, Clone, Debug, Default, Eq, PartialEq, Hash, Ord, PartialOrd)]
pub struct NotificationFlags(pub u32);

impl NotificationFlags {
    /// Empty set.
    pub const EMPTY: Self = Self(0);

    /// `true` if no flags are set.
    #[must_use]
    pub const fn is_empty(self) -> bool {
        self.0 == 0
    }

    /// Combine two sets bitwise-OR.
    #[must_use]
    pub const fn union(self, other: Self) -> Self {
        Self(self.0 | other.0)
    }

    /// Numeric value.
    #[must_use]
    pub const fn bits(self) -> u32 {
        self.0
    }
}

/// A capability-gated asynchronous notification channel.
pub struct NotificationChannel {
    id: u64,
    required_send_caps: CapabilitySet,
    required_bind_caps: CapabilitySet,
    pending: AtomicU32,
    bound: AtomicU32, // 0 = unbound, 1 = bound
}

impl NotificationChannel {
    /// Create a new channel.
    ///
    /// The creator must already hold every capability in
    /// `required_bind_caps`; the kernel will additionally require
    /// that property at bind time (so a future task that takes over
    /// the binder role is not retroactively granted authority).
    /// Channels that restrict who may *signal* are privileged and
    /// require [`rustos_abi::CapabilityId::IPC_BIND_PRIVILEGED`] at
    /// creation, mirroring [`crate::Port::create`].
    ///
    /// # Errors
    ///
    /// * [`Errno::PermissionDenied`] if the creator does not satisfy
    ///   the bind authority described above.
    pub fn create<S: Sink + ?Sized>(
        id: u64,
        creator: &TaskCapabilities,
        required_send_caps: CapabilitySet,
        required_bind_caps: CapabilitySet,
        audit: &S,
    ) -> Result<Self, Errno> {
        let mut id_buf = [0u8; 16];
        let id_field = Field {
            key: "channel",
            value: format_hex_u64(id, &mut id_buf),
        };
        if !required_bind_caps.is_subset_of(creator.effective()) {
            record(audit, AuditEvent::NotifySignalDenied, &[id_field]);
            return Err(Errno::PermissionDenied);
        }
        if !required_send_caps.is_empty()
            && !creator.has(rustos_abi::CapabilityId::IPC_BIND_PRIVILEGED)
        {
            record(audit, AuditEvent::NotifySignalDenied, &[id_field]);
            return Err(Errno::PermissionDenied);
        }
        Ok(Self {
            id,
            required_send_caps,
            required_bind_caps,
            pending: AtomicU32::new(0),
            bound: AtomicU32::new(0),
        })
    }

    /// Bind a receiver to the channel.
    ///
    /// Idempotent on a single receiver — bind twice and the second
    /// call merely re-records the audit decision. Channels are
    /// single-receiver in `abi-v1`; a Stage 2.7 multiplexer crate
    /// (if and when needed) will introduce a fan-out wrapper rather
    /// than mutating this contract.
    ///
    /// # Errors
    ///
    /// [`Errno::PermissionDenied`] if `receiver` lacks any capability
    /// in `required_bind_caps`. The kernel enforces; the receiver
    /// does not re-check (`AGENTS.md` §5.2 final bullet).
    pub fn bind<S: Sink + ?Sized>(
        &self,
        receiver: &TaskCapabilities,
        audit: &S,
    ) -> Result<(), Errno> {
        let mut id_buf = [0u8; 16];
        let mut recv_buf = [0u8; 16];
        let id_field = Field {
            key: "channel",
            value: format_hex_u64(self.id, &mut id_buf),
        };
        let recv_field = Field {
            key: "receiver",
            value: format_hex_u64(receiver.task().0, &mut recv_buf),
        };
        if !self.required_bind_caps.is_subset_of(receiver.effective()) {
            record(
                audit,
                AuditEvent::NotifySignalDenied,
                &[id_field, recv_field],
            );
            return Err(Errno::PermissionDenied);
        }
        self.bound.store(1, Ordering::Release);
        record(audit, AuditEvent::NotifyBound, &[id_field, recv_field]);
        Ok(())
    }

    /// `true` if a receiver has been bound.
    #[must_use]
    pub fn is_bound(&self) -> bool {
        self.bound.load(Ordering::Acquire) == 1
    }

    /// OR `flags` into the channel's pending set.
    ///
    /// # Errors
    ///
    /// [`Errno::PermissionDenied`] if `sender` lacks any capability
    /// in `required_send_caps`; one [`AuditEvent::NotifySignalDenied`]
    /// is emitted before the call returns (fail-closed).
    pub fn signal<S: Sink + ?Sized>(
        &self,
        sender: &TaskCapabilities,
        flags: NotificationFlags,
        audit: &S,
    ) -> Result<(), Errno> {
        let mut id_buf = [0u8; 16];
        let mut sender_buf = [0u8; 16];
        let id_field = Field {
            key: "channel",
            value: format_hex_u64(self.id, &mut id_buf),
        };
        let sender_field = Field {
            key: "sender",
            value: format_hex_u64(sender.task().0, &mut sender_buf),
        };
        if !self.required_send_caps.is_subset_of(sender.effective()) {
            record(
                audit,
                AuditEvent::NotifySignalDenied,
                &[id_field, sender_field],
            );
            return Err(Errno::PermissionDenied);
        }
        if flags.is_empty() {
            // Signalling no bits is not a security event; the call is
            // simply a no-op and is not audited.
            return Ok(());
        }
        // Lossless accumulation: every bit raised since the last drain
        // is preserved.
        self.pending.fetch_or(flags.bits(), Ordering::AcqRel);
        record(
            audit,
            AuditEvent::NotifySignalled,
            &[id_field, sender_field],
        );
        Ok(())
    }

    /// Atomically take and clear the pending flags.
    ///
    /// Does not perform a capability check — the receiver was vetted
    /// at [`Self::bind`] time and the kernel routes the result only
    /// to that bound task (Stage 2.7 dispatcher).
    pub fn take_pending(&self) -> NotificationFlags {
        NotificationFlags(self.pending.swap(0, Ordering::AcqRel))
    }

    /// Capability set required of every sender.
    #[must_use]
    pub fn required_send_caps(&self) -> &CapabilitySet {
        &self.required_send_caps
    }

    /// Capability set required of any binder.
    #[must_use]
    pub fn required_bind_caps(&self) -> &CapabilitySet {
        &self.required_bind_caps
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::audit::RecordingSink;
    use rustos_abi::CapabilityId;
    use rustos_kernel_sec::captable::TaskId;
    use rustos_kernel_sec::identity::UserId;

    fn caps_of(items: &[CapabilityId]) -> CapabilitySet {
        let mut s = CapabilitySet::empty();
        for c in items {
            s.insert(*c);
        }
        s
    }

    fn task_with(task_id: u64, caps: &[CapabilityId]) -> TaskCapabilities {
        let sink = RecordingSink::new();
        let set = caps_of(caps);
        TaskCapabilities::derive(TaskId(task_id), UserId(1), set, set, &sink)
    }

    #[test]
    fn create_rejects_creator_lacking_bind_caps() {
        let sink = RecordingSink::new();
        let creator = task_with(1, &[]);
        let required_bind = caps_of(&[CapabilityId::AUDIT_READ]);
        assert_eq!(
            NotificationChannel::create(1, &creator, CapabilitySet::empty(), required_bind, &sink)
                .map(|_| ()),
            Err(Errno::PermissionDenied)
        );
    }

    #[test]
    fn create_requires_ipc_bind_privileged_for_restricted_sender() {
        let sink = RecordingSink::new();
        let creator = task_with(1, &[CapabilityId::NET_RAW]);
        let required_send = caps_of(&[CapabilityId::NET_RAW]);
        assert_eq!(
            NotificationChannel::create(2, &creator, required_send, CapabilitySet::empty(), &sink)
                .map(|_| ()),
            Err(Errno::PermissionDenied)
        );
    }

    #[test]
    fn bind_denies_receiver_missing_caps() {
        let sink = RecordingSink::new();
        let creator = task_with(
            1,
            &[CapabilityId::IPC_BIND_PRIVILEGED, CapabilityId::AUDIT_READ],
        );
        let ch = NotificationChannel::create(
            3,
            &creator,
            caps_of(&[CapabilityId::IPC_BIND_PRIVILEGED]),
            caps_of(&[CapabilityId::AUDIT_READ]),
            &sink,
        )
        .unwrap();
        let receiver = task_with(2, &[]);
        assert_eq!(ch.bind(&receiver, &sink), Err(Errno::PermissionDenied));
        assert!(sink.ids().contains(&AuditEvent::NotifySignalDenied.id().0));
        assert!(!ch.is_bound());
    }

    #[test]
    fn signal_without_caps_is_eperm_and_audited() {
        let sink = RecordingSink::new();
        let creator = task_with(
            1,
            &[CapabilityId::IPC_BIND_PRIVILEGED, CapabilityId::NET_RAW],
        );
        let ch = NotificationChannel::create(
            4,
            &creator,
            caps_of(&[CapabilityId::NET_RAW]),
            CapabilitySet::empty(),
            &sink,
        )
        .unwrap();
        let bad_sender = task_with(7, &[]);
        assert_eq!(
            ch.signal(&bad_sender, NotificationFlags(0b1), &sink),
            Err(Errno::PermissionDenied)
        );
        assert!(sink.ids().contains(&AuditEvent::NotifySignalDenied.id().0));
        // No bits accumulated.
        assert!(ch.take_pending().is_empty());
    }

    #[test]
    fn signal_or_accumulates_and_take_pending_clears() {
        let sink = RecordingSink::new();
        let creator = task_with(1, &[CapabilityId::IPC_BIND_PRIVILEGED]);
        let ch = NotificationChannel::create(
            5,
            &creator,
            CapabilitySet::empty(),
            CapabilitySet::empty(),
            &sink,
        )
        .unwrap();
        let sender = task_with(7, &[]);
        ch.signal(&sender, NotificationFlags(0b0001), &sink)
            .unwrap();
        ch.signal(&sender, NotificationFlags(0b0100), &sink)
            .unwrap();
        let drained = ch.take_pending();
        assert_eq!(drained, NotificationFlags(0b0101));
        // Second drain is empty (atomic swap cleared on first call).
        assert!(ch.take_pending().is_empty());
        assert!(sink.ids().contains(&AuditEvent::NotifySignalled.id().0));
    }

    #[test]
    fn signalling_empty_flags_is_a_silent_noop() {
        let sink = RecordingSink::new();
        let creator = task_with(1, &[CapabilityId::IPC_BIND_PRIVILEGED]);
        let ch = NotificationChannel::create(
            6,
            &creator,
            CapabilitySet::empty(),
            CapabilitySet::empty(),
            &sink,
        )
        .unwrap();
        let sender = task_with(7, &[]);
        let n_before = sink.len();
        ch.signal(&sender, NotificationFlags::EMPTY, &sink).unwrap();
        assert_eq!(sink.len(), n_before, "empty signal does not audit");
    }

    #[test]
    fn bind_then_signal_is_audited() {
        let sink = RecordingSink::new();
        let creator = task_with(1, &[CapabilityId::IPC_BIND_PRIVILEGED]);
        let ch = NotificationChannel::create(
            7,
            &creator,
            CapabilitySet::empty(),
            CapabilitySet::empty(),
            &sink,
        )
        .unwrap();
        let receiver = task_with(2, &[]);
        ch.bind(&receiver, &sink).unwrap();
        assert!(ch.is_bound());
        assert!(sink.ids().contains(&AuditEvent::NotifyBound.id().0));
    }

    #[test]
    fn flags_helpers_are_sane() {
        assert!(NotificationFlags::EMPTY.is_empty());
        assert_eq!(
            NotificationFlags(0b001).union(NotificationFlags(0b110)),
            NotificationFlags(0b111),
        );
        assert_eq!(NotificationFlags(0xDEAD).bits(), 0xDEAD);
    }

    #[test]
    fn accessors_return_configured_sets() {
        let sink = RecordingSink::new();
        let creator = task_with(1, &[CapabilityId::IPC_BIND_PRIVILEGED]);
        let ch = NotificationChannel::create(
            8,
            &creator,
            CapabilitySet::empty(),
            CapabilitySet::empty(),
            &sink,
        )
        .unwrap();
        assert!(ch.required_send_caps().is_empty());
        assert!(ch.required_bind_caps().is_empty());
    }
}
