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

/// `timeout_ns` value meaning "no deadline": park until a member is ready.
///
/// Named because every reactor that folds a set of armed deadlines needs a
/// spelling for "nothing is armed", and two callers writing the bare
/// saturating value could drift from what the syscall actually treats as
/// unbounded.
pub const WAITSET_TIMEOUT_NONE: u64 = u64::MAX;

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
    /// A system notice topic (its `id` is the
    /// [`NoticeTopic`](crate::notice::NoticeTopic) wire value). Ready when
    /// the topic's generation differs from the one this member last
    /// observed.
    ///
    /// Every topic is a machine-wide state no principal owns — the
    /// desktop's description, the mount table's composition, the
    /// memory-pressure band — so adding the member needs no capability;
    /// *publishing* is what carries per-topic authority. An `id` outside
    /// the topic set names a source that does not exist and is refused
    /// like any other unresolvable member.
    ///
    /// Readiness is **edge-triggered on the topic's generation**: reporting
    /// the member ready advances its observed generation to the current one,
    /// so the next wait blocks until the topic moves *again*. A member added
    /// while a topic already holds an unusual value therefore stays quiet —
    /// the subscriber reads the value once at start-up and is then told only
    /// about moves. The memory-pressure topic's generation is the band depth
    /// itself, so a band that deepens and relaxes again before the waiter
    /// runs correctly does *not* fire: the waiter's view is already right
    /// and there is nothing to do.
    ///
    /// This is what lets a process *converge* on machine-wide state rather
    /// than poll for it: a desktop application re-themes the moment the
    /// session switches appearance, a file manager re-reads its places when
    /// a volume is attached, and a process holding rasterised glyphs gives
    /// them back as memory tightens — each woken by the edge, none burning a
    /// core to learn nothing on almost every sample (`plans/NOTICE.md`).
    SystemNotice = 9,
    /// Room in an asynchronous IPC message port's mailbox (its `id` is the
    /// port's endpoint id) — the send-side twin of [`Port`](Self::Port).
    /// Adding the member is authorised by the caller's *send* authority to
    /// the port — the same capability subset
    /// [`crate::SyscallNumber::IPC_SEND`] checks, never the port *owner*
    /// check the [`Port`](Self::Port) kind applies (the caller here is the
    /// sender, not the binder); an unknown port and one the caller may not
    /// send to both refuse with the same oracle-free `NotFound` the other
    /// kinds use.
    ///
    /// Ready when a send would **not** be refused for want of room: the
    /// mailbox is below capacity, the port is gone (the sender must learn
    /// that rather than park on a destination that can never drain), or the
    /// caller no longer holds the send authority (its send now fails on the
    /// capability, so parking for room would be waiting on the wrong thing —
    /// and a member that is unconditionally ready tells an unauthorised
    /// caller nothing about the mailbox). Readiness is a non-consuming,
    /// level-triggered peek: the woken sender's own
    /// [`IPC_SEND`](crate::SyscallNumber::IPC_SEND) consumes the room, so a
    /// mailbox with room to spare reports again on the next wait.
    ///
    /// Level-triggered, not an edge on the occupancy falling, because the
    /// member is armed *after* a send was refused: an edge seeded at that
    /// moment would already have passed if the receiver drained in between,
    /// and the sender would park forever on a mailbox that is empty. It
    /// exists so a sender holding an event the receiver must not lose — a
    /// window resize, a file-picker conclusion — can park until the
    /// destination drains instead of dropping the event or polling for room
    /// (`plans/APPWIN.md`; the desktop's app-ward hold-back).
    PortRoom = 10,
    /// Room in a writable stream of the caller's **own** open table (its
    /// `id` is that descriptor number) — the send-side twin of
    /// [`Stream`](Self::Stream). Adding the member is owner- and
    /// descriptor-checked exactly as `Stream` is: the descriptor must be a
    /// pipe end opened for writing, a pty master, or a pty slave. A pipe
    /// read end, a path- or resource-backed descriptor, an unopened
    /// number, and another task's descriptor all refuse with the same
    /// oracle-free `NotFound` the other kinds use.
    ///
    /// Ready when a write would **not** be refused for want of room: the
    /// ring is below capacity, **or** the stream is broken (every reader
    /// gone), the latter so a writer parked on a departed reader wakes and
    /// its own write fails `BrokenPipe` rather than waiting forever on a
    /// stream that can never drain. Readiness is a non-consuming,
    /// level-triggered peek: the woken owner's own write takes the room,
    /// so a stream with room to spare reports again on the next wait.
    ///
    /// Without it a parent that multiplexes a long-lived worker over a pipe
    /// pair has no wake to retry a refused write on — only the reply pipe's
    /// readability ever wakes it, so a worker that consumes a burst and
    /// emits nothing leaves the parent's queued bytes stranded. Polling for
    /// room is forbidden, so the room edge is a source like any other
    /// (`plans/SSH.md` §1.1 — the monitor↔worker flow control).
    StreamRoom = 11,
    /// The caller's **own** peer-exit feed (its `id` is always `0`: a thread
    /// has exactly one feed, and observes only its own). Ready when a
    /// process the thread watches through
    /// [`crate::SyscallNumber::PEER_WATCH`] has exited and the exit has not
    /// yet been taken with [`crate::PeerWatchOp::Take`]. Readiness is a
    /// non-consuming peek, so an untaken exit reports again on the next wait.
    ///
    /// It is how a service learns that a client it holds state for — a
    /// socket, a session, a counted connection — has gone, without being its
    /// parent and without polling for it: one member covers every watch.
    PeerExit = 12,
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
            9 => Ok(Self::SystemNotice),
            10 => Ok(Self::PortRoom),
            11 => Ok(Self::StreamRoom),
            12 => Ok(Self::PeerExit),
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
            WaitSourceKind::SystemNotice,
            WaitSourceKind::PortRoom,
            WaitSourceKind::StreamRoom,
            WaitSourceKind::PeerExit,
        ] {
            assert_eq!(WaitSourceKind::from_u32(kind.as_u32()), Ok(kind));
        }
        assert_eq!(WaitSourceKind::from_u32(13), Err(Errno::OutOfRange));
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
        assert_eq!(WaitSourceKind::SystemNotice.as_u32(), 9);
        assert_eq!(WaitSourceKind::PortRoom.as_u32(), 10);
        assert_eq!(WaitSourceKind::StreamRoom.as_u32(), 11);
        assert_eq!(WAITSET_CHILD_ANY, u64::MAX);
        assert_eq!(WAITSET_TIMEOUT_NONE, u64::MAX);
    }
}
