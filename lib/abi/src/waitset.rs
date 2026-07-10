//! The wait-set control vocabulary (`plans/USB.md` — the asynchronous
//! host-controller event loop).
//!
//! A **wait-set** is a kernel object that multiplexes the readiness of
//! several heterogeneous event sources so one process can service them all
//! without a busy poll loop (the charter forbids spinning a core). It is the
//! scalable analogue of `epoll`/`kqueue`: membership is registered once and
//! persists across waits, so the set grows on demand rather than capping the
//! number of sources at a fixed ceiling, and the wait syscall passes only the
//! set handle — never a per-wait array.
//!
//! The three syscalls that drive a wait-set are
//! [`crate::SyscallNumber::WAITSET_CREATE`] (mint the object),
//! [`crate::SyscallNumber::WAITSET_CTL`] (add/remove a member), and
//! [`crate::SyscallNumber::WAITSET_WAIT`] (block until a member is ready).
//! This module defines the two small scalar enumerations those syscalls carry
//! as arguments; the rest of the contract is the syscall arguments themselves,
//! so there is no packed wire format here (the values cross the syscall
//! boundary as plain registers, not as a serialised struct).

use crate::Errno;

/// The operation [`crate::SyscallNumber::WAITSET_CTL`] performs on a wait-set's
/// membership.
#[derive(Copy, Clone, Debug, Eq, PartialEq)]
#[repr(u32)]
pub enum WaitSetOp {
    /// Add a new member, after resolving and owner-checking the named
    /// resource against the calling task.
    Add = 0,
    /// Remove an existing member by its `(kind, id)`.
    Del = 1,
}

impl WaitSetOp {
    /// The wire value for this operation.
    #[must_use]
    pub const fn as_u32(self) -> u32 {
        self as u32
    }

    /// Recover an operation from its wire value.
    ///
    /// # Errors
    ///
    /// [`Errno::OutOfRange`] if `value` is not a known operation (fail closed
    /// on a malformed argument).
    pub const fn from_u32(value: u32) -> Result<Self, Errno> {
        match value {
            0 => Ok(Self::Add),
            1 => Ok(Self::Del),
            _ => Err(Errno::OutOfRange),
        }
    }
}

/// Sentinel `id` for a [`WaitSourceKind::Child`] member observing **any**
/// child of the calling task — the wait-set analogue of
/// [`crate::WAIT_PID_ANY`]. Any other `id` names one specific child by its
/// PID.
pub const WAITSET_CHILD_ANY: u64 = u64::MAX;

/// The kind of event source a wait-set member observes.
///
/// Every kind names a resource the calling task already holds; the kernel
/// owner-checks the resource named by [`crate::SyscallNumber::WAITSET_CTL`]'s
/// `id` against the kind when the member is added, so a wait-set can never
/// observe authority the caller lacks.
#[derive(Copy, Clone, Debug, Eq, PartialEq)]
#[repr(u32)]
pub enum WaitSourceKind {
    /// An IPC call endpoint the caller serves (its `id` is the
    /// endpoint id). Ready when a request is waiting to be received on it.
    Endpoint = 0,
    /// A hardware interrupt line the caller bound (its `id` is the
    /// [`crate::IrqHandle`] raw value). Ready when the line has fired.
    Irq = 1,
    /// A child process of the caller (its `id` is the child's PID, or
    /// [`WAITSET_CHILD_ANY`] for whichever child exits next). Ready when a
    /// matching child has exited and is waiting to be reaped; readiness is a
    /// peek — the caller still reaps through the `wait` syscall (its
    /// non-blocking `WaitFlags::NONBLOCK` form, so the reap itself never
    /// parks the serve loop that observed the readiness). A process can only
    /// ever observe its **own** children: a specific `id` that names no
    /// child of the caller is refused when the member is added.
    Child = 2,
    /// A seat's desktop input channels (its `id` is the seat id). Adding the
    /// member is owner-checked against the seat's **live lease**: only the
    /// task that acquired the seat (`display_acquire`) may observe its input.
    /// Ready when the seat's keyboard **or** pointer channel holds a record
    /// for the member's task, and *also* when that task no longer holds the
    /// live lease (the lease was revoked, released, or the seat was
    /// hot-removed) — the wake-on-loss makes losing the seat observable: the
    /// woken owner's next drain fails closed with the typed refusal and the
    /// session tears down instead of parking forever
    /// (`plans/DISPLAY.md` D7a).
    SeatInput = 3,
}

impl WaitSourceKind {
    /// The wire value for this kind.
    #[must_use]
    pub const fn as_u32(self) -> u32 {
        self as u32
    }

    /// Recover a kind from its wire value.
    ///
    /// # Errors
    ///
    /// [`Errno::OutOfRange`] if `value` is not a known kind (fail closed on a
    /// malformed argument).
    pub const fn from_u32(value: u32) -> Result<Self, Errno> {
        match value {
            0 => Ok(Self::Endpoint),
            1 => Ok(Self::Irq),
            2 => Ok(Self::Child),
            3 => Ok(Self::SeatInput),
            _ => Err(Errno::OutOfRange),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn op_round_trips_and_rejects_unknown() {
        for op in [WaitSetOp::Add, WaitSetOp::Del] {
            assert_eq!(WaitSetOp::from_u32(op.as_u32()), Ok(op));
        }
        assert_eq!(WaitSetOp::from_u32(2), Err(Errno::OutOfRange));
        assert_eq!(WaitSetOp::from_u32(u32::MAX), Err(Errno::OutOfRange));
    }

    #[test]
    fn kind_round_trips_and_rejects_unknown() {
        for kind in [
            WaitSourceKind::Endpoint,
            WaitSourceKind::Irq,
            WaitSourceKind::Child,
            WaitSourceKind::SeatInput,
        ] {
            assert_eq!(WaitSourceKind::from_u32(kind.as_u32()), Ok(kind));
        }
        assert_eq!(WaitSourceKind::from_u32(4), Err(Errno::OutOfRange));
        assert_eq!(WaitSourceKind::from_u32(u32::MAX), Err(Errno::OutOfRange));
    }

    #[test]
    fn wire_values_are_frozen() {
        assert_eq!(WaitSetOp::Add.as_u32(), 0);
        assert_eq!(WaitSetOp::Del.as_u32(), 1);
        assert_eq!(WaitSourceKind::Endpoint.as_u32(), 0);
        assert_eq!(WaitSourceKind::Irq.as_u32(), 1);
        assert_eq!(WaitSourceKind::Child.as_u32(), 2);
        assert_eq!(WaitSourceKind::SeatInput.as_u32(), 3);
        assert_eq!(WAITSET_CHILD_ANY, u64::MAX);
    }
}
