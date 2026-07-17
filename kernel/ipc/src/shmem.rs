//! Capability-gated shared-memory objects.
//!
//! A [`SharedMemory`] is a kernel-tracked allocation that can be
//! mapped into one or more recipient tasks. Pages are owned by the
//! kernel (delegated to `kernel/mem`); the recipient holds a
//! capability-checked [`ShmemMapping`] guard that grants access only
//! while the underlying object is alive.
//!
//! Two security properties are enforced unconditionally:
//!
//! * **Capability gate on every `map`.** A recipient must hold every
//!   capability in `required_caps`; the kernel enforces, the receiver
//!   does not re-check. Refused requests fail
//!   with [`Errno::PermissionDenied`] and emit
//!   [`AuditEvent::ShmemMapDenied`].
//! * **Revocation invalidates every live mapping.** [`SharedMemory::revoke`]
//!   transitions the object to the revoked state atomically; every
//!   pre-existing [`ShmemMapping::as_bytes`] call thereafter returns
//!   `None`. This is the racing-with-mapper invariant the tests
//!   exercise.
//!
//! The backing storage uses [`tairix_kernel_mem::SensitiveBuffer`] —
//! the audited, zero-on-free allocator from `kernel/mem` — so that
//! any credential or capability-token bytes ever carried through
//! shared memory are wiped at revocation.

extern crate alloc;

use alloc::sync::Arc;

use tairix_abi::Errno;
use tairix_caps::CapabilitySet;
use tairix_kernel_mem::sensitive::alloc_sensitive;
use tairix_kernel_mem::SensitiveBuffer;
use tairix_kernel_sec::captable::TaskCapabilities;
use tairix_log::{Field, Sink};
use tairix_sync::RwLock;
use tairix_util::fmt::{format_hex_u64, format_usize};

use crate::audit::{record, AuditEvent};

/// Identifier for a shared-memory object.
#[repr(transparent)]
#[derive(Copy, Clone, Debug, Eq, PartialEq, Hash, Ord, PartialOrd)]
pub struct ShmemId(pub u64);

/// Maximum size, in bytes, of a single shared-memory object.
///
/// Sized generously enough for any current kernel data-plane use but
/// bounded so a misbehaving caller cannot exhaust kernel memory by
/// requesting an arbitrarily large region.
pub const SHMEM_MAX_BYTES: usize = 16 * 1024 * 1024;

/// Inner state of a shared-memory object, held behind an `Arc` so
/// every [`ShmemMapping`] keeps the storage alive for its lifetime.
struct Inner {
    id: ShmemId,
    required_caps: CapabilitySet,
    // Buffer + revoked flag are protected by a single `RwLock`:
    // mappers and readers share readers; revocation takes the writer.
    state: RwLock<State>,
}

struct State {
    buffer: Option<SensitiveBuffer>,
    revoked: bool,
}

/// A capability-gated shared-memory object.
///
/// Construct with [`SharedMemory::create`]; map into recipients with
/// [`SharedMemory::map`]; tear down with [`SharedMemory::revoke`].
pub struct SharedMemory {
    inner: Arc<Inner>,
}

/// Owner handle on a [`SharedMemory`] kept by the creator.
///
/// Functionally an alias for the shared-memory object itself; named
/// distinctly so the Stage 2.7 dispatcher can keep the creator's
/// handle separate from the per-recipient [`ShmemMapping`] handles
/// in its tables.
pub type ShmemHandle = SharedMemory;

impl SharedMemory {
    /// Allocate a `len`-byte shared-memory object owned by the creator.
    ///
    /// The creator must already hold every capability in
    /// `required_caps`: a binder may not grant authority it does not
    /// itself hold (no ambient authority).
    ///
    /// # Errors
    ///
    /// * [`Errno::LengthOutOfRange`] if `len == 0` or `len > SHMEM_MAX_BYTES`.
    /// * [`Errno::PermissionDenied`] if the creator lacks the
    ///   required capabilities.
    pub fn create<S: Sink + ?Sized>(
        id: ShmemId,
        creator: &TaskCapabilities,
        required_caps: CapabilitySet,
        len: usize,
        audit: &S,
    ) -> Result<Self, Errno> {
        let mut id_buf = [0u8; 16];
        let mut len_buf = [0u8; 12];
        let id_field = Field {
            key: "shmem",
            value: tairix_log::FieldValue::Str(format_hex_u64(id.0, &mut id_buf)),
        };
        let len_field = Field {
            key: "len",
            value: tairix_log::FieldValue::Str(format_usize(len, &mut len_buf)),
        };

        if len == 0 || len > SHMEM_MAX_BYTES {
            record(audit, AuditEvent::ShmemMapDenied, &[id_field, len_field]);
            return Err(Errno::LengthOutOfRange);
        }
        if !required_caps.is_subset_of(creator.effective()) {
            record(audit, AuditEvent::ShmemMapDenied, &[id_field]);
            return Err(Errno::PermissionDenied);
        }
        let buffer = alloc_sensitive(len).map_err(|_| Errno::LengthOutOfRange)?;
        record(audit, AuditEvent::ShmemCreated, &[id_field, len_field]);
        Ok(Self {
            inner: Arc::new(Inner {
                id,
                required_caps,
                state: RwLock::new(State {
                    buffer: Some(buffer),
                    revoked: false,
                }),
            }),
        })
    }

    /// Identifier this object was created with.
    #[must_use]
    pub fn id(&self) -> ShmemId {
        self.inner.id
    }

    /// Capability set required of every mapper.
    #[must_use]
    pub fn required_caps(&self) -> &CapabilitySet {
        &self.inner.required_caps
    }

    /// Length of the underlying buffer in bytes, or 0 if revoked.
    #[must_use]
    pub fn len(&self) -> usize {
        let s = self.inner.state.read();
        s.buffer.as_ref().map_or(0, SensitiveBuffer::len)
    }

    /// `true` if the object has been revoked.
    #[must_use]
    pub fn is_revoked(&self) -> bool {
        self.inner.state.read().revoked
    }

    /// `true` if the underlying buffer is logically empty (revoked or
    /// zero-length, though the latter is rejected by `create`).
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.len() == 0
    }

    /// Establish a mapping for `recipient`.
    ///
    /// The kernel enforces `required_caps` on every call; the
    /// receiver does not re-check (final bullet).
    /// Refused requests fail closed with
    /// [`Errno::PermissionDenied`] (missing capability) or
    /// [`Errno::NotFound`] (object already revoked).
    pub fn map<S: Sink + ?Sized>(
        &self,
        recipient: &TaskCapabilities,
        audit: &S,
    ) -> Result<ShmemMapping, Errno> {
        let mut id_buf = [0u8; 16];
        let mut recv_buf = [0u8; 16];
        let id_field = Field {
            key: "shmem",
            value: tairix_log::FieldValue::Str(format_hex_u64(self.inner.id.0, &mut id_buf)),
        };
        let recv_field = Field {
            key: "recipient",
            value: tairix_log::FieldValue::Str(format_hex_u64(recipient.task().0, &mut recv_buf)),
        };

        if !self.inner.required_caps.is_subset_of(recipient.effective()) {
            record(audit, AuditEvent::ShmemMapDenied, &[id_field, recv_field]);
            return Err(Errno::PermissionDenied);
        }
        if self.inner.state.read().revoked {
            record(audit, AuditEvent::ShmemMapDenied, &[id_field, recv_field]);
            return Err(Errno::NotFound);
        }
        record(audit, AuditEvent::ShmemMapped, &[id_field, recv_field]);
        Ok(ShmemMapping {
            inner: Arc::clone(&self.inner),
        })
    }

    /// Atomically revoke the object.
    ///
    /// Every subsequent call to [`ShmemMapping::as_bytes`] or
    /// [`ShmemMapping::with_bytes_mut`] returns `None`. The backing
    /// buffer is wiped (zero-on-free, via
    /// [`SensitiveBuffer`]'s `Drop`) before this call returns.
    /// Idempotent.
    pub fn revoke<S: Sink + ?Sized>(&self, audit: &S) {
        let mut id_buf = [0u8; 16];
        let id_field = Field {
            key: "shmem",
            value: tairix_log::FieldValue::Str(format_hex_u64(self.inner.id.0, &mut id_buf)),
        };
        {
            let mut s = self.inner.state.write();
            // Dropping the buffer zeroes its bytes (SensitiveBuffer::drop).
            s.buffer = None;
            s.revoked = true;
        }
        record(audit, AuditEvent::ShmemRevoked, &[id_field]);
    }
}

/// A capability-checked mapping into a shared-memory object.
///
/// Returned by [`SharedMemory::map`]. While the object is alive the
/// mapping grants read/write access through [`Self::as_bytes`] and
/// [`Self::with_bytes_mut`]; once the object is revoked, every
/// accessor returns `None` — the test
/// `revocation_races_with_mapper` model-checks this.
pub struct ShmemMapping {
    inner: Arc<Inner>,
}

impl ShmemMapping {
    /// Identifier of the underlying [`SharedMemory`].
    #[must_use]
    pub fn id(&self) -> ShmemId {
        self.inner.id
    }

    /// Run `f` against the mapped bytes, or return `None` if the
    /// object has been revoked.
    ///
    /// The closure is invoked under the inner read lock so revocation
    /// cannot tear down the buffer mid-access; revocation taking the
    /// writer lock blocks until every concurrent reader has released
    /// it. This is the synchronisation that makes the racing-mapper
    /// test deterministic.
    pub fn with_bytes<R>(&self, f: impl FnOnce(&[u8]) -> R) -> Option<R> {
        let s = self.inner.state.read();
        s.buffer.as_ref().map(|b| f(b.as_bytes()))
    }

    /// Run `f` against the mapped bytes mutably; same semantics as
    /// [`Self::with_bytes`] but takes the writer lock so it is
    /// mutually exclusive with revocation *and* with concurrent
    /// readers.
    pub fn with_bytes_mut<R>(&self, f: impl FnOnce(&mut [u8]) -> R) -> Option<R> {
        let mut s = self.inner.state.write();
        s.buffer.as_mut().map(|b| f(b.as_bytes_mut()))
    }

    /// Snapshot the mapped bytes (or return `None` after revocation).
    ///
    /// Returns an owned copy because the inner buffer is protected by
    /// a lock and cannot be borrowed out across the guard boundary.
    #[must_use]
    pub fn as_bytes(&self) -> Option<alloc::vec::Vec<u8>> {
        self.with_bytes(<[u8]>::to_vec)
    }

    /// `true` if the underlying object has been revoked.
    #[must_use]
    pub fn is_revoked(&self) -> bool {
        self.inner.state.read().revoked
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::audit::RecordingSink;
    use tairix_abi::CapabilityId;
    use tairix_kernel_sec::captable::TaskId;
    use tairix_kernel_sec::identity::UserId;

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
    fn create_rejects_zero_and_oversize() {
        let sink = RecordingSink::new();
        let creator = task_with(1, &[]);
        assert_eq!(
            SharedMemory::create(ShmemId(1), &creator, CapabilitySet::empty(), 0, &sink)
                .map(|_| ()),
            Err(Errno::LengthOutOfRange)
        );
        assert_eq!(
            SharedMemory::create(
                ShmemId(2),
                &creator,
                CapabilitySet::empty(),
                SHMEM_MAX_BYTES + 1,
                &sink
            )
            .map(|_| ()),
            Err(Errno::LengthOutOfRange)
        );
    }

    #[test]
    fn create_rejects_unheld_caps() {
        let sink = RecordingSink::new();
        let creator = task_with(1, &[]);
        let required = caps_of(&[CapabilityId::AUDIT_READ]);
        assert_eq!(
            SharedMemory::create(ShmemId(3), &creator, required, 64, &sink).map(|_| ()),
            Err(Errno::PermissionDenied)
        );
    }

    #[test]
    fn create_succeeds_and_records_event() {
        let sink = RecordingSink::new();
        let creator = task_with(1, &[]);
        let s =
            SharedMemory::create(ShmemId(4), &creator, CapabilitySet::empty(), 64, &sink).unwrap();
        assert_eq!(s.len(), 64);
        assert!(!s.is_revoked());
        assert!(sink.ids().contains(&AuditEvent::ShmemCreated.id().0));
    }

    #[test]
    fn map_denies_recipient_missing_caps() {
        let sink = RecordingSink::new();
        let creator = task_with(1, &[CapabilityId::AUDIT_READ]);
        let s = SharedMemory::create(
            ShmemId(5),
            &creator,
            caps_of(&[CapabilityId::AUDIT_READ]),
            64,
            &sink,
        )
        .unwrap();
        let recipient = task_with(2, &[]);
        assert_eq!(
            s.map(&recipient, &sink).map(|_| ()),
            Err(Errno::PermissionDenied)
        );
        assert!(sink.ids().contains(&AuditEvent::ShmemMapDenied.id().0));
    }

    #[test]
    fn map_then_read_returns_buffer() {
        let sink = RecordingSink::new();
        let creator = task_with(1, &[]);
        let s =
            SharedMemory::create(ShmemId(6), &creator, CapabilitySet::empty(), 8, &sink).unwrap();
        let recipient = task_with(2, &[]);
        let m = s.map(&recipient, &sink).unwrap();
        m.with_bytes_mut(|b| b.copy_from_slice(b"01234567"))
            .unwrap();
        assert_eq!(m.as_bytes(), Some(b"01234567".to_vec()));
    }

    #[test]
    fn revoke_invalidates_existing_mapping() {
        let sink = RecordingSink::new();
        let creator = task_with(1, &[]);
        let s =
            SharedMemory::create(ShmemId(7), &creator, CapabilitySet::empty(), 4, &sink).unwrap();
        let recipient = task_with(2, &[]);
        let m = s.map(&recipient, &sink).unwrap();
        assert!(m.as_bytes().is_some());
        s.revoke(&sink);
        assert!(m.is_revoked());
        assert!(m.as_bytes().is_none());
        assert!(sink.ids().contains(&AuditEvent::ShmemRevoked.id().0));
    }

    #[test]
    fn map_on_revoked_object_is_not_found() {
        let sink = RecordingSink::new();
        let creator = task_with(1, &[]);
        let s =
            SharedMemory::create(ShmemId(8), &creator, CapabilitySet::empty(), 4, &sink).unwrap();
        s.revoke(&sink);
        let recipient = task_with(2, &[]);
        assert_eq!(s.map(&recipient, &sink).map(|_| ()), Err(Errno::NotFound));
    }

    #[test]
    fn revoke_is_idempotent() {
        let sink = RecordingSink::new();
        let creator = task_with(1, &[]);
        let s =
            SharedMemory::create(ShmemId(9), &creator, CapabilitySet::empty(), 4, &sink).unwrap();
        s.revoke(&sink);
        s.revoke(&sink);
        assert!(s.is_revoked());
    }
}
