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
    /// An asynchronous IPC message port the caller bound via
    /// [`crate::SyscallNumber::PORT_BIND`] (its `id` is the port's
    /// endpoint id). Adding the member is owner-checked against the
    /// port's owning task: only the binder may observe its own mailbox.
    /// Ready when at least one delivered message is waiting to be
    /// drained by [`crate::SyscallNumber::IPC_RECV`]; readiness is a
    /// non-consuming peek, so the woken owner's drain — not the wait —
    /// consumes the message (`plans/APPWIN.md` AW3: an app parks here
    /// for its window events, never a poll loop).
    Port = 4,
    /// A readable pipe stream of the caller's **own** open table (its
    /// `id` is the descriptor number a [`crate::SyscallNumber::PIPE_CREATE`]
    /// read end landed at). Adding the member is owner- and
    /// descriptor-checked against the calling task's open table: the
    /// descriptor must be a pipe end opened for reading — a write end, a
    /// path- or resource-backed descriptor, an unopened number, or
    /// another task's descriptor all refuse with the same oracle-free
    /// `NotFound` the other kinds use. Ready when a read would not park:
    /// buffered bytes are waiting, **or** every write end is closed (the
    /// woken owner's read observes end-of-stream rather than waiting
    /// forever on a dead writer). Readiness is a non-consuming peek — the
    /// woken owner's read, not the wait, drains the bytes — so a still-
    /// readable stream re-reports on the next wait (`plans/APPWIN.md`
    /// AW4: the windowed terminal parks here for its shell's output,
    /// never a poll loop).
    Stream = 5,
    /// The caller's **own** signal intake (its `id` is always `0`: a
    /// process has exactly one intake and can only ever observe its own).
    /// Adding the member requires the caller to have opted in through
    /// [`crate::SyscallNumber::SIGNAL_INTAKE`]
    /// ([`crate::SignalIntakeOp::Enable`]); without the opt-in there is no
    /// intake to observe and the add fails closed with the same
    /// oracle-free `NotFound` the other kinds use. Ready when an observed
    /// termination-request signal (`Interrupt`/`Terminate`) is pending
    /// undrained; readiness is a non-consuming peek — the woken owner
    /// drains through [`crate::SignalIntakeOp::Take`], never the wait
    /// (`plans/STRESSTEST.md` ST3: the stress controller parks here to
    /// catch `^C` and tear its workers down).
    Signal = 6,
    /// A path-backed open descriptor of the caller's **own** open table —
    /// a regular file or a directory (its `id` is that descriptor number).
    /// Adding the member is owner- and descriptor-checked against the
    /// calling task's open table: the descriptor must be a path-backed
    /// handle the caller has open — a resource- or pipe-backed descriptor,
    /// an unopened number, or another task's descriptor all refuse with the
    /// same oracle-free `NotFound` the other kinds use. Ready when the
    /// node the descriptor names has *changed* since the member was added
    /// or last reported ready: a write or truncate to a file, or a create,
    /// remove, or rename under a directory. Readiness is **edge-triggered**
    /// on the node's change generation — reporting the member ready advances
    /// the member's observed generation to the current one, so the next
    /// wait blocks until the node changes *again* (a followed file that
    /// grows twice fires twice, and one that never changes never fires).
    /// The kernel keys the notification on the node's stable
    /// [`crate::FileId`], so a write wakes only the descriptors watching
    /// *that* node, never every file watcher on every write (`tail -f`
    /// parks here for its file's growth and its directory's rotation,
    /// never a poll loop).
    File = 7,
    /// The reply to a request the caller posted with
    /// [`crate::SyscallNumber::CALL_POST`] on a call endpoint (its `id` is
    /// that endpoint id). Adding the member is authorised by the caller's
    /// *send* authority to the endpoint — the same grant
    /// [`crate::SyscallNumber::IPC_CALL`] / [`crate::SyscallNumber::CALL_POST`]
    /// check — never the endpoint *owner* check the [`Endpoint`](Self::Endpoint)
    /// kind applies (the caller here is the client, not the server); an
    /// endpoint the caller may not post to refuses with the same oracle-free
    /// `NotFound` the other kinds use. Ready when a reply the caller posted
    /// has arrived and is unclaimed, **or** its per-request deadline has
    /// elapsed (so a wedged callee wakes the waiter exactly like a real
    /// completion). Readiness is a non-consuming peek — the woken owner drains
    /// with [`crate::SyscallNumber::CALL_REAP`], never the wait — so a caller
    /// driving many devices multiplexes all their completions on one wait-set
    /// instead of parking on each in turn (`plans/FIX-IO.md` IO1/IO2: the
    /// volume manager services many block devices without a blocking thread
    /// per device, never a poll loop).
    CallReply = 8,
    /// The system memory-pressure band (its `id` is always `0`: the
    /// machine has exactly one band). Ready when the band the kernel
    /// publishes differs from the one this member last observed.
    ///
    /// The band is a five-level, hysteresis-damped, machine-wide
    /// indicator — `normal`, `mild`, `moderate`, `severe`, `critical` —
    /// and carries no per-process, per-user, or byte-level figure, so
    /// adding the member needs no capability: any process may learn that
    /// the machine is short of memory, exactly as it may read the load
    /// average. The privileged, audited
    /// [`SysinfoQueryId::MEMORY_PRESSURE`](crate::SysinfoQueryId::MEMORY_PRESSURE)
    /// query — watermarks, free and total bytes, per-band transition
    /// counts — is unchanged and still gated.
    ///
    /// Readiness is **edge-triggered on the band itself**, not on a
    /// change counter: reporting the member ready advances its observed
    /// band to the published one, so a band that deepens and relaxes
    /// again before the waiter runs correctly does *not* fire — the
    /// waiter's view is already right and there is nothing to do. A
    /// member added while the band is `normal` therefore stays quiet
    /// until the machine actually tightens.
    ///
    /// This exists so a process can *cooperate* with reclaim instead of
    /// being reclaimed against: a desktop session holding megabytes of
    /// rasterised glyphs and icons parks here and gives them back the
    /// moment the band deepens, in the same order and at the same bands
    /// as the kernel's own caches (`plans/SMARTRAM.md` SMART5). Polling
    /// for that would burn a core to learn nothing on almost every
    /// sample; the band changes rarely, so an edge is exactly the right
    /// shape.
    MemoryPressure = 9,
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
            4 => Ok(Self::Port),
            5 => Ok(Self::Stream),
            6 => Ok(Self::Signal),
            7 => Ok(Self::File),
            8 => Ok(Self::CallReply),
            9 => Ok(Self::MemoryPressure),
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
            WaitSourceKind::Port,
            WaitSourceKind::Stream,
            WaitSourceKind::Signal,
            WaitSourceKind::File,
            WaitSourceKind::CallReply,
            WaitSourceKind::MemoryPressure,
        ] {
            assert_eq!(WaitSourceKind::from_u32(kind.as_u32()), Ok(kind));
        }
        assert_eq!(WaitSourceKind::from_u32(10), Err(Errno::OutOfRange));
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
        assert_eq!(WaitSourceKind::Port.as_u32(), 4);
        assert_eq!(WaitSourceKind::Stream.as_u32(), 5);
        assert_eq!(WaitSourceKind::Signal.as_u32(), 6);
        assert_eq!(WaitSourceKind::File.as_u32(), 7);
        assert_eq!(WaitSourceKind::CallReply.as_u32(), 8);
        assert_eq!(WaitSourceKind::MemoryPressure.as_u32(), 9);
        assert_eq!(WAITSET_CHILD_ANY, u64::MAX);
    }
}
