//! Syscall identifiers shared between the kernel and user space.
//!
//! This module defines the numeric identifier each syscall carries on the
//! ABI. The kernel's per-architecture dispatch table — landing in
//! `kernel/syscall/src/table.rs` during Stage 2 — is generated from this
//! definition; `cargo xtask abi-check` enforces that the two never drift.
//!
//! ## Stage 1 boundary
//!
//! Only the *numbering* of syscalls is fixed here. The kernel-side dispatch
//! table is intentionally not yet introduced (see `PLAN.md` Stage 2). To
//! avoid prematurely triggering the cross-check, the syscall ABI lives in a
//! `syscall.rs` (singular) module rather than the `syscalls.rs` file that
//! `cargo xtask abi-check` watches for. The file `lib/abi/src/syscalls.rs`
//! will be introduced together with `kernel/syscall/src/table.rs` so that
//! the diff tool always sees both halves.

use crate::Errno;

/// Length in bytes of the cryptographic hash a manifest uses to pin the
/// syscall table it was built against.
///
/// A manifest carrying a hash whose value disagrees with the kernel's
/// compiled-in hash is refused at load time; this is the mechanism by which
/// `abi-v1` binaries are detected on an `abi-v2` kernel and vice-versa.
pub const SYSCALL_TABLE_HASH_LEN: usize = 32;

/// Stable syscall identifier.
///
/// Wraps a `u16` so it cannot be confused with raw integer arguments at call
/// sites. Identifiers are dense; gaps are not permitted because the kernel
/// dispatch table indexes directly with the value.
#[repr(transparent)]
#[derive(Copy, Clone, Debug, Eq, PartialEq, Hash, Ord, PartialOrd)]
pub struct SyscallNumber(u16);

impl SyscallNumber {
    /// Yield the calling thread.
    pub const YIELD: Self = Self(0);
    /// Terminate the calling process with the supplied exit code.
    pub const EXIT: Self = Self(1);
    /// Send a message to an IPC message port (fire-and-forget).
    ///
    /// Arguments: `endpoint: u64` (a port bound via [`Self::PORT_BIND`]),
    /// `payload: *const u8`, `len: usize`. The kernel copies the payload,
    /// re-checks the port's required send capabilities against the caller's
    /// effective set on every send, records the sender's kernel-attested
    /// [`crate::Origin`] beside the bytes, and wakes the port's owner if it
    /// is parked on a wait-set observing the port. The send never blocks: a
    /// full mailbox is the retryable [`crate::Errno::WouldBlock`], never a
    /// wait — so a server can deliver to an unresponsive client without ever
    /// being parked by it.
    pub const IPC_SEND: Self = Self(2);
    /// Receive a message from an IPC message port the caller owns.
    ///
    /// Arguments: `endpoint: u64`, `buf: *mut u8`, `len: usize`,
    /// `sender_out: *mut u8` (exactly [`crate::ORIGIN_WIRE_LEN`] bytes).
    /// Only the port's owning task may receive (checked against the
    /// kernel-trusted caller, alongside the port's required receive
    /// capabilities). On success the payload is copied into `buf`, the
    /// sending task's kernel-attested [`crate::Origin`] — snapshotted at
    /// send time, never claimable by the sender — is written through
    /// `sender_out`, and the payload length is returned, so a receiver can
    /// fail closed on a message from an unexpected principal. An empty
    /// mailbox is the retryable [`crate::Errno::WouldBlock`]; the caller
    /// parks on a wait-set member of kind
    /// [`crate::WaitSourceKind::Port`], never a poll loop.
    pub const IPC_RECV: Self = Self(3);
    /// Query whether the caller holds a given capability.
    pub const CAP_QUERY: Self = Self(4);
    /// Delegate a (necessarily narrower) capability set to another task.
    pub const CAP_DELEGATE: Self = Self(5);
    /// Revoke a previously delegated capability set.
    pub const CAP_REVOKE: Self = Self(6);
    /// Read the monotonic clock.
    pub const CLOCK_GET: Self = Self(7);
    /// Bind to a hardware interrupt line.
    ///
    /// Argument: `line: u32` — architecture-defined IRQ identifier
    /// (GSI on x86_64, GIC `IntId` on `AArch64`, PLIC source on
    /// RISC-V; the per-architecture binding is documented in
    /// `docs/src/security/irq.md`). Returns an opaque
    /// [`crate::IrqHandle`] (kernel-issued, unforgeable) bound to the
    /// calling task. Requires [`crate::CapabilityId::IRQ_BIND`].
    pub const IRQ_BIND: Self = Self(8);
    /// Wait for a wake-up on a previously bound interrupt handle.
    ///
    /// Arguments: `handle: IrqHandle`, `timeout_ns: u64`. The kernel
    /// blocks the caller on the handle's wait queue. Returns
    /// `Ok(())` when the interrupt fires (the kernel masks the line
    /// at the controller before resuming the waiter so the same edge
    /// does not stampede the driver), or [`crate::Errno::TimedOut`]
    /// if `timeout_ns` elapses first. Requires
    /// [`crate::CapabilityId::IRQ_BIND`] — the handle is also
    /// re-checked against the calling task's binding to defend
    /// against handle forgery.
    pub const IRQ_WAIT: Self = Self(9);
    /// Fill a user buffer with cryptographically secure random bytes.
    ///
    /// Arguments: `buf: *mut u8` (user pointer), `len: usize`,
    /// `flags: u32` ([`crate::RandomFlags`]). Returns the number of
    /// bytes written. The kernel draws from its CSPRNG-backed output
    /// reserve; the call is unprivileged (drawing
    /// randomness needs no capability), but a `len` above
    /// [`crate::RANDOM_REQUEST_MAX_BYTES`] is refused. With
    /// [`crate::RandomFlags::NON_BLOCKING`] set and the kernel RNG not
    /// yet seeded it returns [`crate::Errno::EntropyNotReady`] rather
    /// than blocking.
    pub const RANDOM_GET: Self = Self(10);
    /// Write a byte buffer to one of the calling process's inherited
    /// standard streams.
    ///
    /// Arguments: `fd: u32` (the standard descriptor — [`crate::STDOUT`],
    /// [`crate::STDERR`], or [`crate::STDINFO`]), `buf: *const u8` (user
    /// pointer), `len: usize`. Returns the number of bytes written. The
    /// kernel resolves `fd` against the caller's per-process descriptor
    /// table ([`crate::DescriptorTable`]) — the inherited descriptor, not
    /// an ambient device, is the authority — then copies the buffer
    /// through the validated `copy_from_user` boundary
    /// and emits it to that descriptor's kernel stream backing. In the
    /// bootstrap session every backing is the discovered console (the
    /// detected framebuffer when present, else the first discovered UART,
    /// `plans/PI.md` P6), so use of a console-backed stream additionally
    /// requires [`crate::CapabilityId::CONSOLE_WRITE`]. An `fd` that is
    /// not a writable inherited stream fails closed; a build with no
    /// backing wired fails closed with [`crate::Errno::NotImplemented`].
    pub const STREAM_WRITE: Self = Self(11);
    /// Spawn a new process from an embedded program named by an absolute
    /// path (`plans/SPAWN.md` SP3, SP10).
    ///
    /// Arguments: `path: *const u8` (user pointer to the program's
    /// absolute path), `path_len: usize`, `attach: u64` (the address of an
    /// encoded [`crate::SpawnAttach`] block, or `0` for full inherit), and
    /// `attach_len: usize` (its exact byte length,
    /// [`crate::SPAWN_ATTACH_LEN`], zero when absent). The attach block
    /// selects the child's target user ([`crate::SPAWN_UID_INHERIT`] or a
    /// concrete uid, kernel-gated on
    /// [`crate::CapabilityId::SPAWN_AS_USER`]), the console its base
    /// descriptor table comes from ([`CONSOLE_INHERIT`](crate::CONSOLE_INHERIT)
    /// = the **caller's own** table; any other value names a console index
    /// reported by [`SyscallNumber::CONSOLE_COUNT`], failing closed with
    /// [`crate::Errno::NotFound`] when not installed), and one
    /// [`crate::FdWire`] per standard descriptor — so a shell wires a
    /// child's fd 0/1/2/3 onto pre-opened files, resources, or pipe ends
    /// of the caller's **own** open table, each owner-checked before any
    /// state is touched. The kernel
    /// copies the path in through the validated `copy_from_user`
    /// boundary, looks it up in the kernel's
    /// embedded-program registry, builds a fresh **hardware-isolated**
    /// address space for it, registers it as a runnable process,
    /// and returns the new process's PID; the caller keeps running (a
    /// true concurrent spawn, not an `exec`-style hand-off). Gated by
    /// [`crate::CapabilityId::PROC_SPAWN`] — spawning materialises a new
    /// principal and hands it the CPU, so it is privileged rather than
    /// ambient. The spawned program receives only the
    /// intersection of its own signed manifest request and its user's
    /// grants; spawn authority does not widen the child's
    /// authority. A build with no spawn service wired, or a path naming
    /// no registered program, fails closed.
    pub const SPAWN: Self = Self(12);
    /// Read a byte buffer from the calling process's inherited standard
    /// input.
    ///
    /// Arguments: `fd: u32` (the standard descriptor — normally
    /// [`crate::STDIN`]), `buf: *mut u8` (user pointer), `len: usize`.
    /// Returns the number of bytes read. The kernel resolves `fd` against
    /// the caller's per-process descriptor table ([`crate::DescriptorTable`])
    /// and reads from that descriptor's kernel stream backing — in the
    /// bootstrap session the first discovered keyboard/UART input source
    /// (`plans/PI.md` P6) — into a bounded kernel staging buffer, copying it
    /// out through the validated `copy_to_user` boundary. The input counterpart of [`SyscallNumber::STREAM_WRITE`]: a
    /// short read (fewer bytes than `len`, possibly zero when no input is
    /// pending) is valid, so the caller loops. Use of a console-backed
    /// stream additionally requires [`crate::CapabilityId::CONSOLE_READ`].
    /// Only the console's controlling (foreground) owner drains its input
    /// queue (`plans/DISPLAY.md` D5): while an owner is recorded through
    /// [`Self::CONSOLE_FOREGROUND`], any other task's read of that console
    /// is refused with [`crate::Errno::NotForeground`] before any input is
    /// consumed — a background reader fails closed, never stopped by an
    /// asynchronous signal; an unowned console reads openly. An `fd` that
    /// is not a readable inherited stream fails closed; a build with no
    /// backing wired fails closed with [`crate::Errno::NotImplemented`].
    pub const STREAM_READ: Self = Self(13);
    /// Map a fresh anonymous `RW` region into the calling process's own
    /// address space (`plans/SPAWN.md` SP5).
    ///
    /// Arguments: `len: usize` (bytes, rounded up to whole pages),
    /// `flags: u32` ([`crate::MapFlags`]), `addr_hint: u64` (a page-aligned
    /// placement hint; `0` means "kernel chooses"). Returns the base address
    /// of the new region. The region is zeroed before it is visible, is
    /// always `RW` and never executable (W^X), and is
    /// mapped only into the **caller's own** hardware-isolated address space
    /// (no global user heap, no cross-process mapping). The
    /// call is unprivileged — growing one's own address space needs no
    /// capability (precedent) — but the kernel validates
    /// every argument and fails closed. A frame- or
    /// page-table-allocation failure returns [`crate::Errno::OutOfMemory`]
    /// rather than panicking; a build with no memory
    /// service wired fails closed with [`crate::Errno::NotImplemented`].
    pub const MEM_MAP: Self = Self(14);
    /// Release a region previously returned by [`SyscallNumber::MEM_MAP`]
    /// from the calling process's own address space (`plans/SPAWN.md` SP5).
    ///
    /// Arguments: `base: u64` (the region's base, as returned by `mem_map`)
    /// and `len: usize` (its length in bytes). The frames reclaimed are
    /// zeroed on free (secret hygiene). Unmapping a region
    /// that was never mapped, or a partial/over-long range, fails closed; a build with no memory service wired fails closed
    /// with [`crate::Errno::NotImplemented`].
    pub const MEM_UNMAP: Self = Self(15);
    /// Wait for a child process to exit, reaping it and reporting its
    /// exit code (`plans/SPAWN.md` SP6).
    ///
    /// Arguments: `pid: i32` (the child to wait for, or [`WAIT_PID_ANY`] to
    /// wait for any of the caller's children), `status: *mut
    /// tairix_wait_status_t` (a non-null user pointer the kernel writes the
    /// typed [`crate::WaitStatusRecord`] into — an exited record carrying
    /// the reaped child's exit code, or, when requested with
    /// [`WaitFlags::STOPPED`], a stopped record carrying the stopping
    /// signal), and `flags: u32` (a [`WaitFlags`] set). Returns the
    /// reported child's PID. A process may only wait
    /// on its **own** children — waiting reaps a child the caller spawned,
    /// so it grants no authority over anything else and needs no
    /// capability (precedent — "list my own processes");
    /// the kernel validates the parent/child relationship and fails closed. Waiting on a `pid` that is not a child of the
    /// caller fails closed with [`crate::Errno::NotFound`]; a build with no
    /// process-wait service wired fails closed with
    /// [`crate::Errno::NotImplemented`].
    ///
    /// With [`WaitFlags::NONBLOCK`] set the call polls instead of blocking:
    /// it reaps an already-exited child if one exists, otherwise — when a
    /// matching child is still running — it returns [`crate::Errno::WouldBlock`]
    /// (the `abi-v1` "nothing yet, retry" signal) without parking the caller,
    /// and `status` is left untouched. With the bit clear the call blocks
    /// until a child becomes reapable (never busy-polls). With
    /// [`WaitFlags::STOPPED`] set the call also reports a child freshly
    /// stopped by [`crate::Signal::Stop`] — returning its PID and writing a
    /// stopped record — **without reaping it**; each stop is reported once,
    /// re-armed by [`crate::Signal::Continue`]. A reserved flag bit
    /// fails closed with [`crate::Errno::OutOfRange`].
    pub const WAIT: Self = Self(16);
    /// Read the calling process's effective limit for one resource.
    ///
    /// Arguments: `kind: u32` (a [`crate::LimitKind`] discriminant) and
    /// `out: *mut tairix_resource_limit_t` (a non-null user pointer the kernel
    /// writes the encoded [`crate::ResourceLimit`] into). Returns an error
    /// code (`Ok(0)` on success). Observing one's *own* effective limit is
    /// the unprivileged baseline — it grants no authority and needs no
    /// capability (precedent) — but the kernel validates
    /// `kind` and the pointer and fails closed. An
    /// unassigned `kind` fails with [`crate::Errno::OutOfRange`]; a build
    /// with no resource-limit service wired fails closed with
    /// [`crate::Errno::NotImplemented`].
    pub const RLIMIT_GET: Self = Self(17);
    /// Set the calling process's limit for one resource.
    ///
    /// Arguments: `kind: u32` (a [`crate::LimitKind`] discriminant) and
    /// `in: *const tairix_resource_limit_t` (a non-null user pointer to the
    /// encoded [`crate::ResourceLimit`] to install). Returns an error code
    /// (`Ok(0)` on success). A process may freely *lower* a bound, but
    /// *raising* a hard bound — or setting any bound above the inherited
    /// ceiling — requires [`crate::CapabilityId::RLIMIT_RAISE`] and
    /// otherwise fails with [`crate::Errno::PermissionDenied`]. A malformed
    /// pair (`soft > hard`) or an unassigned `kind` fails closed with
    /// [`crate::Errno::OutOfRange`]; a build with no resource-limit service
    /// wired fails closed with [`crate::Errno::NotImplemented`].
    pub const RLIMIT_SET: Self = Self(18);
    /// Read the system user database (`/System/Security/Users`) the kernel
    /// loaded off the mounted root volume at boot (
    /// `plans/PI.md` P11).
    ///
    /// Arguments: `buf: *mut u8` (a non-null user pointer) and
    /// `len: usize` (the buffer's capacity). Returns the number of bytes
    /// copied — the database's exact `users-v1` text, which the caller
    /// parses with the same fail-closed `lib/users` parser the kernel
    /// used. Gated by [`crate::CapabilityId::USERS_READ`]: the text
    /// carries every account's salted password record, so only the
    /// authentication principal (login) may read it (no
    /// ambient authority). A buffer smaller than the database fails
    /// closed with [`crate::Errno::BufferTooSmall`] — the kernel never
    /// truncates a credential database; sizing the
    /// buffer at the format's own 64 KiB maximum always suffices. A build
    /// holding no database — no root volume mounted, or the record was
    /// refused at boot — fails closed with [`crate::Errno::NotFound`], so
    /// a system without accounts refuses every login rather than
    /// inventing one.
    pub const USERS_DB_READ: Self = Self(19);
    /// Report how many system text consoles are installed
    /// (`plans/PI.md` P11).
    ///
    /// No arguments. Returns the number of console stream backings the
    /// boot path installed — each one an independent text console (the
    /// video console when a display is active, else the discovered UART)
    /// a spawner may attach a child's standard streams to through
    /// [`SyscallNumber::SPAWN`]'s `console` argument. PID 1 `init` uses
    /// it to start one login session per installed console
    /// (`plans/PI.md` P11). A UART beside an active display is not
    /// installed as a console at all: it carries only the debug log, so
    /// no session can draw over the log stream. Gated by
    /// [`crate::CapabilityId::CONSOLE_WRITE`]: console topology belongs
    /// to the principals that drive consoles, not to every task.
    pub const CONSOLE_COUNT: Self = Self(20);
    /// Set the console read line discipline of one of the calling
    /// process's inherited input streams (`plans/PI.md` P11 — the
    /// [`crate::InputMode`]: cooked, secret, or raw).
    ///
    /// Arguments: `fd: u32` (the input descriptor — normally
    /// [`crate::STDIN`]) and `mode: u32` (an [`crate::InputMode`]
    /// discriminant; the reserved `0` and every unknown value fail closed
    /// with [`crate::Errno::OutOfRange`]). Returns an error code (`Ok(0)`
    /// on success). The mode is the line-discipline behaviour of the
    /// console *backing*: **cooked** (the default) echoes every byte a
    /// [`SyscallNumber::STREAM_READ`] of `fd` consumes back to that
    /// descriptor's console output so an interactive user sees what they
    /// type; **secret** suppresses echo and shows the activity indicator
    /// instead (login's password read — the secret is never rendered but
    /// the operator sees progress); **raw** suppresses echo and draws
    /// nothing (a full-screen curses program paints its own display). The
    /// kernel performs the echo/indicator itself (it owns the line
    /// discipline), so a reader needs no separate
    /// [`crate::CapabilityId::CONSOLE_WRITE`] for it. Gated by
    /// [`crate::CapabilityId::CONSOLE_READ`]: the input discipline belongs
    /// to the principal that reads the console, never to every task — and,
    /// like the input drain, to the console's controlling (foreground)
    /// owner while one is recorded ([`Self::CONSOLE_FOREGROUND`],
    /// `plans/DISPLAY.md` D5): any other task's mode change is refused
    /// with [`crate::Errno::NotForeground`], so a background task cannot
    /// flip the foreground program's discipline under it. An
    /// `fd` that is not a readable inherited stream fails closed with
    /// [`crate::Errno::NotFound`]; a build with no console wired fails
    /// closed with [`crate::Errno::NotImplemented`].
    pub const STREAM_INPUT_MODE: Self = Self(21);
    /// Inject one decoded keyboard *key edge* into the kernel input-focus
    /// arbiter (`plans/PI.md` P11 — input follows the
    /// surface owner).
    ///
    /// Arguments: `seat: u64` (the seat the edge belongs to — the seat
    /// whose keyboard produced it, the boot seat `0` for a directly
    /// attached keyboard; an unknown id fails closed with
    /// [`crate::Errno::NotFound`]), `buf: *const u8` (one
    /// [`crate::input::KeyInput`] record)
    /// and `len: usize` (its length, [`crate::input::KeyInput::WIRE_LEN`]).
    /// Returns the number of bytes consumed, or a negative error code. The
    /// keyboard-input driver that decoded a directly attached keyboard
    /// (USB-HID / PS-2) emits the *device-resolved key edge* — a pressed or
    /// released [`crate::input::KeyValue`] plus the held
    /// [`crate::input::Modifiers`] — and the kernel arbiter decides both
    /// the **encoding** and the **destination** by who currently holds
    /// that seat (`plans/PI.md` P11): with the text console foreground it
    /// encodes the press to its console (tty) bytes through the shared
    /// `lib/keymap` map and enqueues them on the focused console's input
    /// queue (drained by a [`SyscallNumber::STREAM_READ`]); with the
    /// desktop (window manager) foreground it routes the whole record to
    /// the kernel keyboard channel (drained by
    /// [`SyscallNumber::KEYBOARD_READ`]). The driver no longer chooses the
    /// encoding or the destination — that policy left the device. Gated by
    /// [`crate::CapabilityId::INPUT_INJECT`]: feeding the system's keyboard
    /// stream is privileged, never ambient. A malformed
    /// record is refused fail-closed.
    pub const KEY_INJECT: Self = Self(22);
    /// Acquire ownership of a seat — one display with its keyboard — and
    /// claim its input focus (`plans/PI.md` P11 — input follows the
    /// surface owner).
    ///
    /// Argument: `seat: u64` (the seat to acquire; the boot seat is `0`
    /// and every further seat is minted per discovered display node — an
    /// unknown id fails closed with [`crate::Errno::NotFound`]). Returns
    /// the minted lease's generation (`>= 1`) or a negative error code.
    /// The compositing window manager calls this when it takes over a
    /// screen: the kernel records the **kernel-attested caller** as that
    /// seat's owner, and key edges subsequently
    /// injected for the seat ([`SyscallNumber::KEY_INJECT`]) are delivered as
    /// [`crate::input::KeyInput`] records the owner drains with
    /// [`SyscallNumber::KEYBOARD_READ`] — the same keyboard stream now
    /// following the new surface owner automatically (the desktop analogue
    /// of "input follows the foreground tty"). A seat held by another
    /// task refuses the claim with [`crate::Errno::SeatBusy`] (ownership
    /// is never displaced), and a repeat acquire by the holder is refused
    /// with [`crate::Errno::AlreadyExists`]. Gated by
    /// [`crate::CapabilityId::DISPLAY`]: owning the seat is privileged,
    /// never ambient (`plans/DISPLAY.md`).
    pub const DISPLAY_ACQUIRE: Self = Self(23);
    /// Release a seat and return its keyboard input focus to the text
    /// console (`plans/PI.md` P11).
    ///
    /// Argument: `seat: u64` (the seat to release; an unknown id fails
    /// closed with [`crate::Errno::NotFound`]). Returns an error code
    /// (`Ok(0)` on success). The
    /// inverse of [`SyscallNumber::DISPLAY_ACQUIRE`]: the seat owner
    /// calls it when it relinquishes the screen, and the kernel returns
    /// the seat's keyboard to the text console so a login/shell
    /// once again receives it. The release is owner-checked: a caller
    /// that does not hold the seat is refused with
    /// [`crate::Errno::SeatNotOwner`] (or [`crate::Errno::SeatRevoked`]
    /// once, after an administrative eviction) — never a global "flip it
    /// back" switch. Gated by [`crate::CapabilityId::DISPLAY`]
    /// (`plans/DISPLAY.md`).
    pub const DISPLAY_RELEASE: Self = Self(24);
    /// Read one decoded keyboard event from a seat's keyboard channel
    /// (`plans/PI.md` P11 — keyboard input for the
    /// desktop).
    ///
    /// Arguments: `seat: u64` (the seat whose channel is drained; an
    /// unknown id fails closed with [`crate::Errno::NotFound`]),
    /// `buf: *mut u8` (a buffer of at least
    /// [`crate::input::KeyInput::WIRE_LEN`] bytes) and `len: usize` (its
    /// length). Returns the number of bytes written — one
    /// [`crate::input::KeyInput`] record — or `0` when the channel is
    /// momentarily drained; a buffer too small to hold a record fails
    /// closed with [`crate::Errno::BufferTooSmall`]. The
    /// task that owns the seat (the window manager / desktop
    /// session) drains the records the seat registry routed to it while it
    /// held the seat. Gated by [`crate::CapabilityId::INPUT_READ`] **and**
    /// owner-gated against the seat's live lease: a caller that does not
    /// hold the seat is refused with [`crate::Errno::SeatNotOwner`] (or
    /// [`crate::Errno::SeatRevoked`] after an administrative eviction), so
    /// the keyboard stream is delivered only to whoever currently owns the
    /// surface, and an unattached channel denies rather than leaking to a
    /// device (`plans/DISPLAY.md`).
    pub const KEYBOARD_READ: Self = Self(25);
    /// Map a granted device MMIO register window into the calling
    /// driver's own address space (`plans/PI.md`
    /// P10 chunk 5d-0 — the `DriverHost` MMIO/DMA surface reachable over
    /// IPC).
    ///
    /// Arguments: `handle: u64` — an unforgeable, kernel-issued
    /// device-resource grant handle the driver received for the matched
    /// hardware-tree node it binds (one handle per [`crate::hwtree::HwResource`]
    /// the node requested); `offset: usize` — the byte offset of the
    /// sub-region to map *within* that granted window; and `len: usize` —
    /// its length in bytes. The kernel resolves the handle **against the
    /// calling task** (handle forgery is rejected exactly as
    /// [`SyscallNumber::IRQ_WAIT`] re-checks its binding),
    /// confirms the grant names a memory window
    /// ([`crate::hwtree::HwResourceKind::Mmio`] / `BusWindow`), confirms
    /// `[offset, offset + len)` lies wholly inside that granted window, and
    /// maps **only** that sub-region — caching disabled — into the caller's
    /// own hardware-isolated address space, returning its base user virtual
    /// address. A driver can therefore never synthesise a pointer to
    /// arbitrary physical memory: it maps a region inside one the kernel
    /// chose to grant it, and nothing more (no ambient authority).
    /// Mapping a sub-region (not the whole grant) is what lets a driver
    /// granted a large outbound bus aperture map just the single BAR it
    /// enumerated rather than the entire window. Gated by
    /// [`crate::CapabilityId::MMIO_MAP`]; an unknown or non-owned handle, a
    /// grant of the wrong kind, a sub-region that overflows or escapes the
    /// granted window, or a build with no map facility wired fails closed.
    pub const MMIO_MAP: Self = Self(26);
    /// Allocate a DMA-coherent buffer for the calling driver, bounded by a
    /// granted device DMA constraint (`plans/PI.md`
    /// P10 chunk 5d-0 — the `DriverHost` MMIO/DMA surface reachable over
    /// IPC).
    ///
    /// Arguments: `handle: u64` — an unforgeable, kernel-issued
    /// device-resource grant handle the driver received for the matched
    /// hardware-tree node it binds (a [`crate::hwtree::HwResourceKind::Dma`]
    /// constraint); `len: usize` — the number of bytes to allocate; and
    /// `device_out: *mut u64` — a user pointer the kernel writes the
    /// buffer's **device-visible** base address to on success. The kernel
    /// resolves the handle **against the calling task** (rejecting forgery
    /// exactly as [`SyscallNumber::MMIO_MAP`] does),
    /// confirms it names a DMA constraint, carves a physically contiguous,
    /// zeroed, coherent (caching-disabled) region whose physical extent lies
    /// within the grant's addressing limit (a device never
    /// reaches memory the kernel did not grant it), maps it `RW`,
    /// non-executable, guard-bracketed into the caller's own address space,
    /// writes the device-visible base to `device_out`, and returns the base
    /// **user virtual address** the driver's CPU accesses go through. For a
    /// coherent bus (and the QEMU `virt` stand-in) the device-visible
    /// address is the CPU-physical base; a translating inbound viewport
    /// (`dma_translated`) maps it onto the far-side bus address. Gated by
    /// [`crate::CapabilityId::MEM_DMA`]; an unknown or non-owned handle, a
    /// grant of the wrong kind, a region exceeding the grant's limit, or a
    /// build with no DMA facility wired fails closed.
    pub const DMA_ALLOC: Self = Self(27);
    /// Enumerate the device-resource grants the kernel minted for the
    /// calling driver task, delivering the unforgeable handles the driver
    /// passes to [`SyscallNumber::MMIO_MAP`] / [`SyscallNumber::DMA_ALLOC`]
    /// (`plans/PI.md` P10 chunk 5d-2 — handing
    /// a spawned driver process the handles for its matched node).
    ///
    /// Arguments: `buf: *mut u8` — a buffer the kernel fills with the
    /// caller's grant set, serialised as consecutive
    /// [`crate::hwtree::GrantedResource`] records (each
    /// [`crate::hwtree::GrantedResource::WIRE_LEN`] bytes) — and `len: usize`
    /// — its capacity in bytes. The kernel reads the grant set of the
    /// **calling task** (the kernel-trusted caller id, never a caller-supplied
    /// value), copies every record out through the
    /// validated boundary, and returns the total number of bytes written. A
    /// task with no grants returns `0`. A buffer too small to hold the whole
    /// set fails closed with [`Errno::BufferTooSmall`] rather than delivering
    /// a partial grant list; the driver sizes its buffer
    /// for the matched node's resource count, bounded by
    /// [`crate::hwtree::HwNode`]'s fixed resource maximum.
    ///
    /// Needs **no capability**: a task reads only its *own* minted grants,
    /// which confers no authority over anything else (the
    /// own-process-observer baseline). The handles are useless without the
    /// `CAP_MMIO_MAP` / `CAP_MEM_DMA` the matched driver also holds, and the
    /// kernel re-checks ownership when they are presented.
    pub const RESOURCE_GRANTS: Self = Self(28);
    /// Copy the discovered hardware tree out to the
    /// calling task (the read-only System Information API hardware view).
    ///
    /// Arguments: `buf: *mut u8` (a non-null user pointer) and
    /// `len: usize` (the buffer's capacity). Returns the number of bytes
    /// copied: a fixed [`crate::hwtree::HwTreeHeader`] (the store's current
    /// generation and the node count) followed by that many
    /// [`crate::hwtree::HwNode`] records, each
    /// [`crate::hwtree::HwNode::WIRE_LEN`] bytes. The caller parses the
    /// header to learn the generation (the value it passes to
    /// [`SyscallNumber::HW_TREE_WAIT`]) and the node count.
    ///
    /// Gated by [`crate::CapabilityId::SYSINFO_HW`]: the hardware inventory
    /// is a privileged global view, never an
    /// ambient read. A buffer too small for the whole snapshot fails closed
    /// with [`crate::Errno::BufferTooSmall`] — the inventory is never
    /// truncated; the caller grows its buffer and retries
    /// (the node count is a discovered capacity, not a fixed ceiling ). A build with no hardware-tree source wired fails
    /// closed with
    /// [`crate::Errno::NotImplemented`]. There is no `/proc`/`/sys` device
    /// tree and no path that bypasses this capability check.
    pub const HW_TREE_READ: Self = Self(29);
    /// Block the calling task until the hardware tree changes past a
    /// previously observed generation (reactive
    /// re-match and hotplug).
    ///
    /// Arguments: `last_generation: u64` (the generation the caller last
    /// observed through [`SyscallNumber::HW_TREE_READ`]'s header) and
    /// `timeout_ns: u64` (`u64::MAX` for an effectively unbounded wait).
    /// Returns `Ok(0)` once the store's generation differs from
    /// `last_generation` — a node was seeded, appended, or removed — so the
    /// caller re-reads the tree and re-matches; returns
    /// [`crate::Errno::TimedOut`] if the deadline elapses first. The kernel
    /// blocks the caller cooperatively (re-checking the generation between
    /// scheduler dispatches, the same shape as
    /// [`SyscallNumber::IRQ_WAIT`] / [`SyscallNumber::WAIT`]), never
    /// busy-spinning.
    ///
    /// Gated by [`crate::CapabilityId::SYSINFO_HW`] — the same privilege as
    /// reading the tree. A build with no hardware-tree source wired fails
    /// closed with [`crate::Errno::NotImplemented`].
    pub const HW_TREE_WAIT: Self = Self(30);
    /// Make a **synchronous** capability-checked call to a kernel-owned IPC
    /// call endpoint: post a request, block until exactly one matching reply
    /// arrives, and copy it out (Design D D2b —
    /// `.junie/next-pi-prompt.md`).
    ///
    /// Unlike [`SyscallNumber::IPC_SEND`] (fire-and-forget over a
    /// [`crate::ipc`] port), this is request/reply: the kernel correlates the
    /// posted request with one reply through an opaque per-call ticket, so a
    /// system service can answer a specific caller. The first consumer is the
    /// reactive device manager (`userland/system/devmgr`) reading the
    /// read-only `/System` driver store over the disk-owning kernel service's
    /// file-read endpoint ([`crate::driver_store`]).
    ///
    /// Arguments: `endpoint: u64` — the kernel-owned call endpoint id (a
    /// well-known reserved id such as
    /// [`crate::driver_store::DRIVER_STORE_ENDPOINT`], or a delegated one);
    /// `request: *const u8` and `request_len: usize` — the request payload;
    /// `reply: *mut u8` and `reply_cap: usize` — the buffer the reply is
    /// copied into. Returns the number of reply bytes written (`>= 0`), or
    /// `-errno`.
    ///
    /// The kernel enforces the endpoint's required send capability against
    /// the **caller's** effective set before posting (no ambient authority), validates both buffers against the
    /// caller's address space, and blocks the caller cooperatively until the
    /// reply arrives (the same park shape as [`SyscallNumber::HW_TREE_WAIT`]
    /// / [`SyscallNumber::WAIT`]), never busy-spinning. A full
    /// outstanding-call queue is the retryable [`crate::Errno::WouldBlock`].
    /// A reply larger than `reply_cap` fails closed with
    /// [`crate::Errno::BufferTooSmall`]; an unknown
    /// endpoint, a missing capability, or the endpoint being destroyed
    /// mid-call each fail closed. A build with no call-endpoint registry
    /// wired fails closed with [`crate::Errno::NotImplemented`].
    pub const IPC_CALL: Self = Self(31);

    /// Create and register a kernel-owned synchronous **call endpoint** the
    /// calling task then *serves*, so a user-space system service can answer
    /// [`SyscallNumber::IPC_CALL`] requests (Design D D3 — the server half of
    /// the synchronous IPC primitive; `.junie/next-pi-prompt.md`).
    ///
    /// [`SyscallNumber::IPC_CALL`] is the caller half and was, until now,
    /// answerable only by a kernel-resident service (the disk-owning
    /// driver-store kthread). This trio — `call_create` / `call_recv` /
    /// `call_reply` — lets an ordinary user-space process be the *callee*, so
    /// a driver service (the autoloaded `vcmailbox` mailbox service, future
    /// `appmgr`/shell services) can serve the one synchronous primitive
    /// rather than a hand-rolled convention over two async ports.
    ///
    /// Arguments: `endpoint: u64` — the call-endpoint id to bind (a
    /// well-known reserved id the service publishes); `send_caps: *const u8`
    /// and `recv_caps: *const u8` — two `CapabilitySet` wire images
    /// (`CapabilitySet::WIRE_LEN` bytes each) naming the capability a
    /// *caller* must hold to post and the capability a
    /// *server* must hold to [`SyscallNumber::CALL_RECV`]/
    /// [`SyscallNumber::CALL_REPLY`]; `max_request`, `max_reply`, `capacity:
    /// usize` — the endpoint payload and outstanding-call bounds (a
    /// fail-closed memory bound). Returns `0`, or `-errno`.
    ///
    /// Binding a **restricted-sender** endpoint (non-empty `send_caps`)
    /// requires [`crate::CapabilityId::IPC_BIND_PRIVILEGED`], enforced before any state is touched; an id already bound fails
    /// closed with [`crate::Errno::AlreadyExists`] so the kernel never
    /// re-points a live endpoint. The endpoint is owned by
    /// the creating task and torn down (in-flight callers released
    /// fail-closed) when that task exits. A build with no
    /// call-endpoint registry wired fails closed with
    /// [`crate::Errno::NotImplemented`].
    pub const CALL_CREATE: Self = Self(32);

    /// Receive the next request posted to a call endpoint the calling task
    /// owns, blocking until one arrives (Design D D3 — the server-side
    /// receive half; `.junie/next-pi-prompt.md`).
    ///
    /// Arguments: `endpoint: u64` — the bound call-endpoint id; `buf: *mut u8`
    /// and `buf_cap: usize` — the buffer the request payload is copied into;
    /// `ticket_out: *mut u64` — receives the opaque per-call ticket the
    /// server must answer with via [`SyscallNumber::CALL_REPLY`]. Returns the
    /// number of request bytes written (`>= 0`), or `-errno`.
    ///
    /// The kernel enforces the endpoint's required **receive** capability
    /// against the caller's effective set before touching any state
    /// (no ambient authority), validates both
    /// pointers against the caller's address space, and blocks the caller
    /// cooperatively until a request is posted (the same park shape as
    /// [`SyscallNumber::IPC_CALL`]), never busy-spinning. A
    /// request larger than `buf_cap` fails closed with
    /// [`crate::Errno::BufferTooSmall`] and is left queued; an unknown endpoint, a missing capability, or the endpoint
    /// being destroyed each fail closed.
    pub const CALL_RECV: Self = Self(33);

    /// Answer one received call on an endpoint the calling task owns,
    /// releasing the blocked caller (Design D D3 — the server-side reply
    /// half; `.junie/next-pi-prompt.md`).
    ///
    /// Arguments: `endpoint: u64` — the bound call-endpoint id; `ticket: u64`
    /// — the ticket from [`SyscallNumber::CALL_RECV`]; `reply: *const u8` and
    /// `reply_len: usize` — the reply payload. Returns `0`, or `-errno`.
    ///
    /// The kernel enforces the endpoint's required **receive** capability
    /// against the caller before touching state, validates
    /// the buffer, and wakes the caller blocked in [`SyscallNumber::IPC_CALL`]
    /// for that ticket. A reply larger than the endpoint's `max_reply`, an
    /// unknown or already-answered ticket, or an unknown endpoint each fail
    /// closed.
    pub const CALL_REPLY: Self = Self(34);

    /// Block the calling task until the system user database leaves its
    /// *pending* (still-being-unlocked) state (
    /// `plans/PI.md` P11 — the reactive companion to
    /// [`SyscallNumber::USERS_DB_READ`]).
    ///
    /// Argument: `timeout_ns: u64` (`u64::MAX` for an effectively unbounded
    /// wait). Under design B `login` is spawned **before** the in-kernel
    /// unlock kthread mounts the encrypted root, so an early
    /// [`SyscallNumber::USERS_DB_READ`] reports [`crate::Errno::WouldBlock`]
    /// — the live-but-not-ready signal. Rather than re-reading in a yield
    /// loop (a busy spin), `login` calls this once: the
    /// kernel parks it off the run queue and wakes it the instant the unlock
    /// reaches a terminal outcome — a database is installed, or the unlock
    /// gives up with none — so the next read returns the database
    /// ([`crate::Errno`]-free) or the inert [`crate::Errno::NotImplemented`].
    /// Returns `Ok(0)` once the database is no longer pending (so the caller
    /// re-reads and re-classifies), or [`crate::Errno::TimedOut`] if the
    /// deadline elapses first. The kernel blocks cooperatively (the same park
    /// shape as [`SyscallNumber::HW_TREE_WAIT`]), never busy-spinning.
    ///
    /// Gated by [`crate::CapabilityId::USERS_READ`] — the same privilege as
    /// reading the database; only the authentication principal (login) waits
    /// on it (no ambient authority). A build with no
    /// users-database service wired is never pending, so the wait returns
    /// `Ok(0)` immediately and the subsequent read fails closed with
    /// [`crate::Errno::NotImplemented`].
    pub const USERS_DB_WAIT: Self = Self(35);

    /// Emit a structured diagnostic record to the kernel's system log.
    ///
    /// Arguments: `record: *const u8` — a non-null pointer to an encoded
    /// [`crate::LogRecordRef`] wire image (see [`crate::log`]); `len: usize`
    /// — its length. Returns `0`, or `-errno`.
    ///
    /// Gated by [`crate::CapabilityId::LOG_EMIT`]: the kernel verifies the
    /// capability, copies in at most [`crate::LOG_RECORD_MAX`] bytes, and
    /// fully validates the record before touching state.
    /// It then emits the record through its **diagnostic** log sink (the
    /// serial UART on a debug build, the video console on release),
    /// attributing it to the calling task — the caller cannot forge that
    /// attribution. This never reaches the hash-chained security audit log,
    /// which stays kernel-only. A malformed record fails
    /// closed with the decoder's [`crate::Errno`]; the
    /// record is best-effort and below the active level threshold is dropped
    /// in O(1).
    pub const LOG_EMIT: Self = Self(36);

    /// Publish a discovered child device node into the live hardware tree
    /// (recursive, user-space hardware discovery).
    ///
    /// Arguments: `node: *const u8` — a non-null pointer to a wire-encoded
    /// [`crate::HwNode`] (see [`crate::hwtree`]); `len: usize` — its length.
    /// Returns `0`, or `-errno`.
    ///
    /// Gated by [`crate::CapabilityId::HW_EMIT`]: the kernel verifies the
    /// capability, copies in at most [`crate::hwtree::HwNode::WIRE_LEN`]
    /// bytes, and fully decodes and validates the node before touching state. A user-space **bus** driver (PCIe, USB) calls this
    /// to publish each device it enumerates, so the device manager autoloads
    /// the matching driver in turn — discovery is data-driven, never a
    /// compiled-in list. The node is admitted **only** when
    /// every [`crate::hwtree::HwResource`] it requests is wholly contained
    /// within a device-resource grant the calling driver already holds, so an
    /// emitted child can never carry more authority than its emitter
    /// (no ambient authority; — a driver receives only
    /// its matched node's resources). Any malformed node, an unknown parent,
    /// or an out-of-grant resource fails closed; a
    /// successful publish bumps the hardware-tree generation, waking the
    /// device manager's reactive autoload (the same change channel
    /// [`SyscallNumber::HW_TREE_WAIT`] observes).
    pub const HW_EMIT_NODE: Self = Self(37);

    /// Remove a previously-published child device node — and its whole
    /// subtree — from the live hardware tree (hotplug
    /// removal: a removed node unloads its driver).
    ///
    /// Arguments: `node_id: u64` — the [`crate::HwNode::id`] of the node to
    /// remove; `flags: u32` — the [`crate::HwRemoveFlags`] posture word.
    /// Returns `0`, or `-errno`.
    ///
    /// Gated by [`crate::CapabilityId::HW_EMIT`], the **same** privilege as
    /// publishing ([`SyscallNumber::HW_EMIT_NODE`]): a user-space **bus**
    /// driver that enumerated a device and published it now reports that the
    /// device has gone (a USB port-down, a PCIe hot-remove). The kernel owns
    /// the topology, so removal is bounded exactly like publication
    /// (no ambient authority): the caller may remove **only**
    /// a node whose parent is the caller's *own* matched node — a child it
    /// itself published — never an arbitrary node it does not own. The whole
    /// subtree rooted at that node is removed (a bus child may itself have
    /// published grandchildren), so a stale descendant can never outlive its
    /// parent. An unknown id, or a node the caller does not own, fails closed
    /// with [`crate::Errno::NotFound`] / [`crate::Errno::PermissionDenied`]. A successful removal bumps the hardware-tree
    /// generation, waking the device manager's reactive watch (the same
    /// change channel [`SyscallNumber::HW_TREE_WAIT`] observes) so it unloads
    /// the driver bound to the vanished node — the
    /// symmetric counterpart of [`SyscallNumber::HW_EMIT_NODE`], which adds a
    /// node and leaves the *load* to the device manager.
    ///
    /// `flags` selects the removal posture. [`crate::HwRemoveFlags::empty`]
    /// is a **surprise removal** — a device that physically vanished — and
    /// always proceeds: a live volume on a departed device cannot be kept
    /// alive by pretending the device is still there.
    /// [`crate::HwRemoveFlags::ORDERLY`] is the **stop-if-idle** posture an
    /// administrator uses to retire a still-present device (stopping an
    /// assembled RAID array): the kernel refuses with [`crate::Errno::Busy`],
    /// removing nothing, while a volume is still attached on a block-service
    /// endpoint the node declares — the busy check is decided *atomically*
    /// with the removal, so an attach cannot race in between. A reserved flag
    /// bit fails closed with [`crate::Errno::OutOfRange`] before any state is
    /// touched.
    pub const HW_REMOVE_NODE: Self = Self(38);

    /// Allocate a message-signalled interrupt (MSI) vector for a PCI
    /// function and report the architecture-built doorbell to program into
    /// the function's MSI capability.
    ///
    /// Arguments: `out: *const u8` — a non-null pointer to a caller buffer
    /// the kernel fills with an encoded [`crate::MsiAllocation`] (the
    /// allocated virtual interrupt line plus the MSI doorbell address and
    /// data word); `out_len: usize` — its capacity. Returns the number of
    /// bytes written ([`crate::MsiAllocation::WIRE_LEN`]), or `-errno`.
    ///
    /// Gated by [`crate::CapabilityId::IRQ_BIND`] — the same privilege a
    /// driver needs to `irq_bind` the resulting line. The kernel's
    /// interrupt controller mints a free MSI vector, lazily brings the
    /// platform's MSI controller up, and **grants the calling task a device
    /// resource for the virtual line**, so the caller may both bind it and
    /// forward it (as an [`crate::hwtree::HwResource::irq`]) onto a child
    /// node it publishes through [`SyscallNumber::HW_EMIT_NODE`] — never
    /// ambient authority. A platform with no MSI controller fails closed
    /// with [`crate::Errno::NotImplemented`]; exhaustion of the vector space
    /// fails closed with [`crate::Errno::OutOfRange`]. The returned doorbell
    /// is opaque to the caller — a bus driver writes it verbatim into the
    /// function's MSI capability (the message-address/data registers) so the
    /// device's interrupt routes to the allocated line.
    pub const MSI_ALLOC: Self = Self(39);

    /// Create a cross-process **shared-memory region** the calling task owns
    /// and maps, and that it may then grant to another task
    /// (`plans/USB.md` — the URB transport data buffer).
    ///
    /// Arguments: `len: usize` — the region size in bytes (rounded up to
    /// whole pages); `id_out: *mut u64` — a user pointer the kernel writes
    /// the new region's kernel-allocated, unforgeable id to on success.
    /// Returns the base **user virtual address** the region is mapped at
    /// (`RW`, non-executable, cacheable, guard-bracketed), or `-errno`.
    ///
    /// The kernel allocates a physically-contiguous block of RAM it owns,
    /// zeroes it (no cross-process leak), maps it into the caller's own
    /// address space, records the region against the caller as its owner,
    /// and **grants the calling task a device resource for the region** (a
    /// [`crate::hwtree::HwResource::shared`]), so the owner may both map it
    /// and forward it onto a child node it publishes through
    /// [`SyscallNumber::HW_EMIT_NODE`] — never ambient authority. The region
    /// and its frames live until the owner and every grantee have released
    /// their mappings (or exited), at which point the frames are zeroed and
    /// freed. Gated by [`crate::CapabilityId::SHM`]; a zero length, frame
    /// exhaustion (deterministic OOM), or a build with no shared-memory
    /// facility wired fails closed.
    pub const SHM_CREATE: Self = Self(40);

    /// Map a shared-memory region the kernel has **granted** the calling
    /// task into its own address space (`plans/USB.md` — the class driver
    /// mapping the buffer its matched node carried).
    ///
    /// Arguments: `handle: u64` — an unforgeable, kernel-issued
    /// device-resource grant handle the driver received for the matched
    /// hardware-tree node it binds (a [`crate::hwtree::HwResourceKind::Shared`]
    /// region). Returns the base **user virtual address** the region is
    /// mapped at (`RW`, non-executable, cacheable, guard-bracketed), or
    /// `-errno`.
    ///
    /// The kernel resolves the handle **against the calling task**
    /// (rejecting forgery exactly as [`SyscallNumber::MMIO_MAP`] does),
    /// confirms it names a shared region, maps that region's existing
    /// kernel-owned frames into the caller's own address space, and accounts
    /// the mapping against the region so its frames are not freed while the
    /// caller still maps them. A driver therefore reaches exactly the one
    /// region the kernel granted it and no other process's buffer
    /// (no ambient authority). Gated by [`crate::CapabilityId::SHM`]; an
    /// unknown or non-owned handle, a grant of the wrong kind, a region that
    /// has been torn down, or a build with no shared-memory facility wired
    /// fails closed.
    pub const SHM_MAP: Self = Self(41);

    /// Release a shared-memory mapping the calling task established with
    /// [`SyscallNumber::SHM_CREATE`] or [`SyscallNumber::SHM_MAP`]
    /// (`plans/USB.md` — a per-device buffer released on hot-removal).
    ///
    /// Arguments: `base: u64` — the base user virtual address the map
    /// returned; `len: usize` — its length in bytes. Returns `0`, or
    /// `-errno`.
    ///
    /// The kernel validates the `(base, len)` names a shared mapping of the
    /// **calling task**, tears down only that mapping's page-table entries,
    /// and drops the caller's reference to the underlying region; when the
    /// owner and every grantee have released the region its frames are zeroed
    /// (zero-on-free) and returned to the allocator. Needs **no capability**:
    /// it only releases the caller's own mapping (the
    /// [`SyscallNumber::MEM_UNMAP`] posture). A `(base, len)` that does not
    /// name a live shared mapping of the caller fails closed with
    /// [`crate::Errno::NotFound`]; a build with no shared-memory facility
    /// wired fails closed with [`crate::Errno::NotImplemented`].
    pub const SHM_UNMAP: Self = Self(42);

    /// Create a kernel **wait-set**: a growable, caller-owned object that
    /// multiplexes readiness of several heterogeneous event sources so one
    /// process can service them all without a busy poll loop (`plans/USB.md`
    /// — the asynchronous host-controller event loop).
    ///
    /// Takes no arguments. Returns an opaque, kernel-minted wait-set handle
    /// (unforgeable in the same sense as an [`IrqHandle`]: re-checked against
    /// the calling task on every later use), or `-errno`.
    ///
    /// A wait-set is the scalable analogue of `epoll`/`kqueue`: membership is
    /// registered once with [`SyscallNumber::WAITSET_CTL`] and persists across
    /// waits, so [`SyscallNumber::WAITSET_WAIT`] passes only the set handle —
    /// never a per-wait array — and the set grows on demand rather than
    /// capping the number of sources at a fixed ceiling. Needs no capability:
    /// the set observes only resources the caller already holds, each
    /// owner-checked when it is added.
    pub const WAITSET_CREATE: Self = Self(43);

    /// Add or remove a member of a wait-set created with
    /// [`SyscallNumber::WAITSET_CREATE`] (`plans/USB.md`).
    ///
    /// Arguments: `set: u64` — the wait-set handle; `op: u32` — a
    /// [`crate::WaitSetOp`] (`Add` / `Del`); `kind: u32` — a
    /// [`crate::WaitSourceKind`]; `id: u64` — the resource the member names
    /// (per the kind's own docs: an IPC call-endpoint id the caller serves,
    /// an [`IrqHandle`] the caller bound, a child PID or
    /// [`crate::WAITSET_CHILD_ANY`], a seat id the caller leases, a message
    /// port the caller bound, or a pipe-read descriptor of the caller's own
    /// open table); `token: u64` — an opaque,
    /// caller-chosen value [`SyscallNumber::WAITSET_WAIT`] reports back when
    /// this member is ready. Returns `0`, or `-errno`.
    ///
    /// On `Add` the kernel **resolves and owner-checks the named resource
    /// against the calling task before recording it** (no ambient authority):
    /// an endpoint not owned by the caller, or an IRQ handle not bound by it,
    /// fails the call closed. The set thus only ever observes resources the
    /// caller already holds. A handle that is not the caller's own wait-set,
    /// an unknown `op`/`kind`, or `Del` of an absent member fails closed.
    pub const WAITSET_CTL: Self = Self(44);

    /// Block until **any one** member of a wait-set is ready, reporting which
    /// (`plans/USB.md`).
    ///
    /// Arguments: `set: u64` — the wait-set handle; `timeout_ns: u64` — a
    /// relative timeout, or [`u64::MAX`] for "no timeout" (block until a
    /// member is ready); `token_out: *mut u64` — a non-null user pointer the
    /// kernel writes the ready member's caller-chosen token to on success.
    /// Returns `0` (a member was ready, its token written to `token_out`), or
    /// `-errno` ([`crate::Errno::TimedOut`] when the timeout elapses with no
    /// member ready).
    ///
    /// The caller *parks* off the run queue between readiness checks — it is
    /// woken when an IPC request is posted to one of its member endpoints,
    /// when one of its member IRQ lines fires, or by the timeout — so an idle
    /// service burns no CPU (the charter forbids spinning a core). Each
    /// member is re-checked against the calling task as it is scanned, so a
    /// member whose resource was torn down fails that member closed. Needs no
    /// capability (it only observes resources the caller already holds, each
    /// owner-checked when added). A handle that is not the caller's own
    /// wait-set, or a faulting `token_out`, fails closed.
    pub const WAITSET_WAIT: Self = Self(45);

    /// Open a file or directory by absolute path, returning a per-process
    /// file descriptor (`PREREQUISITES.md` P-A).
    ///
    /// Arguments: `path: *const u8` (user pointer to the absolute path),
    /// `path_len: usize` (its length, at most [`crate::FS_PATH_MAX`]), and
    /// `flags: u32` ([`crate::OpenFlags`]). Returns a new file descriptor
    /// at or above [`crate::STD_STREAM_COUNT`] (the standard descriptors fd
    /// 0/1/2/3 are never reused), bound to the caller's per-process
    /// descriptor table, or `-errno`.
    ///
    /// The kernel copies the path in through the validated `copy_from_user`
    /// boundary, parses it with the shared VFS path parser, and resolves it
    /// against the mounted secured VFS under the **caller's real
    /// `Credentials`** (the kernel supplies identity, never the caller), so
    /// every per-inode owner/mode/ACL/`required_cap` check and mount-flag
    /// (`ro`/`nosuid`/`nodev`/`noexec`) decision stays kernel-side. Gated by
    /// [`crate::CapabilityId::FS_ACCESS`]; the per-path authority remains the
    /// inode model. With [`crate::OpenFlags::CREATE`] a missing regular file
    /// is created; [`crate::OpenFlags::EXCLUSIVE`] refuses an existing one;
    /// [`crate::OpenFlags::TRUNCATE`] empties it. An open of neither `READ`
    /// nor `WRITE` is a resolve-only handle for `fs_stat`/`fs_readdir`. A
    /// build with no filesystem service wired fails closed with
    /// [`Errno::NotImplemented`]; any resolution or permission failure fails
    /// closed.
    pub const FS_OPEN: Self = Self(46);
    /// Release a file descriptor returned by [`SyscallNumber::FS_OPEN`]
    /// (`PREREQUISITES.md` P-A).
    ///
    /// Argument: `fd: u32`. Returns `0`, or `-errno`. The kernel drops the
    /// open-handle entry from the caller's per-process descriptor table; a
    /// standard descriptor (fd 0/1/2/3) or an `fd` naming no open handle
    /// fails closed with [`Errno::NotFound`]. Needs
    /// [`crate::CapabilityId::FS_ACCESS`].
    pub const FS_CLOSE: Self = Self(47);
    /// Read up to `len` bytes from an open file at an explicit offset
    /// (`PREREQUISITES.md` P-A).
    ///
    /// Arguments: `fd: u32` (an open handle with read access), `offset: u64`
    /// (the byte offset to read from), `buf: *mut u8` (user pointer), and
    /// `len: usize` (at most [`crate::FS_IO_MAX`]). Returns the number of
    /// bytes read (`0` at end of file), or `-errno`. The kernel resolves the
    /// handle against the caller's descriptor table, re-authorises the read
    /// through the secured VFS, and copies the bytes out through the
    /// validated `copy_to_user` boundary. A handle without read access, or a
    /// standard descriptor, fails closed.
    pub const FS_READ: Self = Self(48);
    /// Write up to `len` bytes to an open file at an explicit offset
    /// (`PREREQUISITES.md` P-A).
    ///
    /// Arguments: `fd: u32` (an open handle with write access), `offset: u64`
    /// (ignored when the handle was opened [`crate::OpenFlags::APPEND`], which
    /// always writes at the current end of file), `buf: *const u8` (user
    /// pointer), and `len: usize` (at most [`crate::FS_IO_MAX`]). Returns the
    /// number of bytes written, or `-errno`. The kernel copies the buffer in
    /// through the validated `copy_from_user` boundary and re-authorises the
    /// write through the secured VFS; a read-only mount, a handle without
    /// write access, or a standard descriptor fails closed.
    pub const FS_WRITE: Self = Self(49);
    /// List the entries of an open directory into a caller buffer
    /// (`PREREQUISITES.md` P-A).
    ///
    /// Arguments: `fd: u32` (an open directory handle), `buf: *mut u8` (user
    /// pointer), and `len: usize`. Returns the number of bytes written: a
    /// packed stream of [`crate::DirEntry`] records. A buffer too small to
    /// hold the whole listing fails closed with [`Errno::BufferTooSmall`]
    /// (the listing is never truncated); the caller grows its buffer and
    /// retries. A handle that does not name a directory fails closed.
    pub const FS_READDIR: Self = Self(50);
    /// Report the structural metadata of an open file or directory
    /// (`PREREQUISITES.md` P-A).
    ///
    /// Arguments: `fd: u32` (any open handle, including a resolve-only one),
    /// `out: *mut u8` (user pointer), and `out_len: usize`. Returns the
    /// number of bytes written: one [`crate::FileStat`]
    /// ([`crate::FileStat::WIRE_LEN`] bytes). A buffer too small fails closed
    /// with [`Errno::BufferTooSmall`].
    pub const FS_STAT: Self = Self(51);
    /// Set the length of an open file (`PREREQUISITES.md` P-A).
    ///
    /// Arguments: `fd: u32` (an open handle with write access) and
    /// `size: u64`. Returns `0`, or `-errno`. The kernel re-authorises the
    /// operation through the secured VFS; a read-only mount, a directory
    /// handle, or a handle without write access fails closed.
    pub const FS_TRUNCATE: Self = Self(52);
    /// Flush an open handle's filesystem to its backing store
    /// (`PREREQUISITES.md` P-A).
    ///
    /// Argument: `fd: u32` (any open handle). Returns `0`, or `-errno`. The
    /// kernel flushes the filesystem backing the handle so prior writes are
    /// durable. A standard descriptor or an `fd` naming no open handle fails
    /// closed.
    pub const FS_SYNC: Self = Self(53);
    /// Create a directory by absolute path (`PREREQUISITES.md` P-A).
    ///
    /// Arguments: `path: *const u8` (user pointer), `path_len: usize` (at
    /// most [`crate::FS_PATH_MAX`]). Returns `0`, or `-errno`. Resolution and
    /// the permission/mount-flag model match [`SyscallNumber::FS_OPEN`]; a
    /// read-only mount, an existing target, or a denied parent fails closed.
    /// Gated by [`crate::CapabilityId::FS_ACCESS`].
    pub const FS_MKDIR: Self = Self(54);
    /// Remove a file or empty directory by absolute path
    /// (`PREREQUISITES.md` P-A).
    ///
    /// Arguments: `path: *const u8` (user pointer), `path_len: usize` (at
    /// most [`crate::FS_PATH_MAX`]). Returns `0`, or `-errno`. Resolution and
    /// the permission/mount-flag model match [`SyscallNumber::FS_OPEN`]; a
    /// non-empty directory, a read-only mount, a missing target, or a denied
    /// parent fails closed. Gated by [`crate::CapabilityId::FS_ACCESS`].
    pub const FS_UNLINK: Self = Self(55);
    /// Release a per-process DMA buffer previously carved by
    /// [`SyscallNumber::DMA_ALLOC`] — the symmetric free for the device-DMA
    /// allocator (`plans/PI.md` P10). A long-running driver that issues many
    /// transfers must reclaim each request's bounce buffers, or it leaks DMA
    /// frames until it exits.
    ///
    /// Arguments: `handle: u64` (the same unforgeable DMA-constraint grant
    /// handle [`SyscallNumber::DMA_ALLOC`] was called with) and `cpu_va: u64`
    /// (the CPU base address that `dma_alloc` returned). Returns `0`, or
    /// `-errno`. The kernel frees only a buffer live in the **calling task's**
    /// own DMA window (`caller.task_id` is kernel-trusted); a forged or
    /// foreign handle, or a `cpu_va` that is not the base of a live carve,
    /// fails closed without releasing anything, so a stale, double, or
    /// cross-task free can never release frames it does not own. The freed
    /// frames are zeroed before they return to the allocator (zero-on-free),
    /// so a later allocation cannot recover the buffer's bytes. Gated by
    /// [`crate::CapabilityId::MEM_DMA`].
    pub const DMA_FREE: Self = Self(56);
    /// Move a file or directory from one absolute path to another
    /// (`PREREQUISITES.md` P-A rename follow-up).
    ///
    /// Arguments: `src: *const u8`, `src_len: usize`, `dst: *const u8`,
    /// `dst_len: usize` (each length at most [`crate::FS_PATH_MAX`]).
    /// Returns `0`, or `-errno`. Both paths must resolve under the same
    /// mounted volume; the moved node keeps its identity and contents.
    /// Resolution and the permission/mount-flag model match
    /// [`SyscallNumber::FS_OPEN`]: search + write on both parent
    /// directories (and write on a directory moved to a new parent) are
    /// required, and a missing source, a non-empty directory destination, a
    /// directory-into-its-own-subtree move, a read-only mount, or a denied
    /// parent fails closed. A cross-mount move is refused with the
    /// dedicated [`crate::Errno::CrossVolume`] (the `EXDEV` equivalent), so
    /// a mover can fall back to copy-then-remove on exactly that condition.
    /// Gated by [`crate::CapabilityId::FS_ACCESS`].
    pub const FS_RENAME: Self = Self(57);
    /// Read the kernel-attested [`Origin`](crate::Origin) of the caller whose
    /// in-service call this server is currently handling
    /// (`PREREQUISITES.md` P-C).
    ///
    /// Arguments: `endpoint: u64` (a call endpoint the calling task owns),
    /// `ticket: u64` (the in-service ticket a prior
    /// [`CALL_RECV`](Self::CALL_RECV) returned), `origin: *mut u8` (user
    /// buffer), `origin_cap: usize` (its capacity, at least
    /// [`crate::ORIGIN_WIRE_LEN`]). On success the caller's attested origin is
    /// written little-endian to `origin` and its byte length returned.
    ///
    /// The origin was snapshotted from the *posting* task's own kernel state
    /// at call time, so it is authoritative and unforgeable by that caller.
    /// Like [`CALL_RECV`](Self::CALL_RECV)/[`CALL_REPLY`](Self::CALL_REPLY)
    /// the kernel confirms the reader owns the endpoint and holds its required
    /// receive capability before exposing anything; a foreign endpoint, an
    /// unknown or not-in-service ticket, or a buffer shorter than
    /// [`crate::ORIGIN_WIRE_LEN`] fails closed. The summary it carries is the
    /// caller's capability *membership* bitmap, never any capability token.
    pub const CALL_PEER_ORIGIN: Self = Self(58);
    /// Read the kernel's wall-clock time and its provenance state
    /// (`PREREQUISITES.md` P-D).
    ///
    /// Arguments: `out: *mut u8` (user buffer), `out_cap: usize` (its
    /// capacity, at least [`crate::WallClockReading::WIRE_LEN`]). On success
    /// the current [`WallClockReading`](crate::WallClockReading) — a
    /// [`Time64`](crate::Time64) instant plus a
    /// [`WallTimeState`](crate::WallTimeState) byte — is written
    /// little-endian to `out` and its byte length returned. A buffer shorter
    /// than the wire length fails closed.
    ///
    /// Unprivileged: any task may read the wall clock. Before a trusted
    /// source has set it the reading is the Unix epoch tagged
    /// [`WallTimeState::Unset`](crate::WallTimeState::Unset); ordering of
    /// events never relies on this value (the monotonic clock and sequence
    /// numbers are the ordering authority).
    pub const WALL_TIME_GET: Self = Self(59);
    /// Set the kernel's wall-clock time from a trusted source
    /// (`PREREQUISITES.md` P-D).
    ///
    /// Arguments: `time: *const u8` (a little-endian
    /// [`Time64`](crate::Time64), [`crate::Time64::WIRE_LEN`] bytes),
    /// `time_len: usize`, `state: u32` (the
    /// [`WallTimeState`](crate::WallTimeState) discriminant to record —
    /// `Firmware`, `Trusted`, or `Adjusted`). Returns `0`, or `-errno`.
    ///
    /// Gated by [`crate::CapabilityId::TIME_SET`]: only a principal trusted
    /// to drive the clock may call it. A malformed instant, a short buffer,
    /// or a `state` that is not a settable variant
    /// ([`Unset`](crate::WallTimeState::Unset) is rejected) fails closed.
    /// The monotonic clock is unaffected; only the wall-time offset and
    /// state change.
    pub const WALL_TIME_SET: Self = Self(60);
    /// Read the kernel's per-boot identifier ([`crate::BootId`])
    /// (`PREREQUISITES.md` P-E).
    ///
    /// Arguments: `out: *mut u8` (user buffer), `out_cap: usize` (its
    /// capacity, at least [`crate::BOOT_ID_LEN`]). On success the 16-byte
    /// [`BootId`](crate::BootId) the kernel minted for this boot is written to
    /// `out` and its byte length returned. A buffer shorter than
    /// [`crate::BOOT_ID_LEN`] fails closed.
    ///
    /// Unprivileged: the boot id is a public per-boot nonce, not a secret, so
    /// any task may read it (like [`Self::CLOCK_GET`] / [`Self::WALL_TIME_GET`]).
    /// If the kernel's random subsystem was not seeded in time to mint one the
    /// call fails closed with
    /// [`Errno::EntropyNotReady`] — the kernel never returns the all-zero
    /// [`BootId::UNSET`](crate::BootId::UNSET) sentinel as if it were a real id.
    pub const BOOT_ID_GET: Self = Self(61);
    /// Read the **unfiltered, global** kernel introspection view
    /// (`PREREQUISITES.md` P-C).
    ///
    /// Arguments: `domain: u32` (an [`IntrospectDomain`](crate::IntrospectDomain)
    /// discriminant), `arg: u64` (a domain-specific selector — a record
    /// offset for the paged domains, unused otherwise), `out: *mut u8` (user
    /// buffer), `out_cap: usize` (its capacity). On success the requested
    /// records are written to `out` little-endian and the byte length
    /// returned; otherwise `-errno`.
    ///
    /// Gated by [`crate::CapabilityId::SYSINFO_INTROSPECT`], held only by the
    /// user-space System Information service (`sysinfod`). The kernel primitive
    /// always returns the whole system's state and **never narrows by
    /// principal**: all per-client scoping is enforced by `sysinfod` against
    /// each requester's kernel-attested [`Origin`](crate::Origin). Every field
    /// is validated and the call fails closed (a bad domain, a short buffer,
    /// or an unresolvable target task all deny). For the per-task limits
    /// domain the target task's 128-bit [`ProcId`](crate::ProcId) is supplied
    /// in `out` on entry (a `u64` arg cannot carry it), which the kernel
    /// resolves against the capability table so the answer survives PID reuse.
    pub const SYSINFO_INTROSPECT: Self = Self(62);
    /// Read the character-cell geometry of the text console backing a
    /// standard stream (`PREREQUISITES.md` P-C — the `top` terminal UI).
    ///
    /// Arguments: `fd: u32` (a standard descriptor the caller owns —
    /// typically [`crate::STDOUT`]), `out: *mut u8` (user buffer), `out_cap:
    /// usize` (its capacity, at least [`crate::TerminalSize::WIRE_LEN`]). On
    /// success the console's [`TerminalSize`](crate::TerminalSize) is written
    /// little-endian to `out` and its byte length returned; otherwise
    /// `-errno`.
    ///
    /// Unprivileged, like [`Self::CLOCK_GET`] / [`Self::WALL_TIME_GET`]: a
    /// program may always ask how big its own terminal is. The kernel reports
    /// a size **only** for a console whose geometry it actually knows (a
    /// framebuffer text console); for a byte-stream console (a UART) the true
    /// size of the remote terminal is unknowable to the kernel, so the call
    /// fails closed with [`Errno::NotImplemented`] and the client terminal
    /// library applies the conventional 80×24 fallback — the kernel never
    /// fabricates a size. An `fd` that is not an open stream, or a buffer
    /// shorter than the wire length, also fails closed.
    pub const TERMINAL_SIZE: Self = Self(63);
    /// Deliver a control signal to another process (`plans/SPAWN.md` SP7,
    /// `plans/NEW-TASKBAR.md` T11).
    ///
    /// Arguments: `pid: i32` (the target) and `signal: u32` (a
    /// [`crate::Signal`] discriminant). Returns `0`, or `-errno`. The kernel
    /// identifies the sender from its own per-CPU current-task slot (never a
    /// caller-supplied identity) and settles the target rule in precedence
    /// order before anything is delivered:
    ///
    /// 1. a live **child** of the caller — the parent/child relationship is
    ///    the authority, so no capability is required, like [`Self::WAIT`];
    /// 2. else a target whose kernel-attested owner is the caller's own
    ///    principal — a principal already controls its own processes, so
    ///    again no capability is required;
    /// 3. else [`crate::CapabilityId::PROC_CONTROL`], because the target
    ///    belongs to a *different* principal. A caller without it is refused
    ///    with [`Errno::PermissionDenied`].
    ///
    /// A non-positive `pid`, or one naming no live task, fails closed with
    /// [`Errno::NotFound`]; a `signal` value that is not a defined
    /// [`crate::Signal`] fails closed with [`Errno::OutOfRange`]; a build
    /// with no process-signal service wired fails closed with
    /// [`Errno::NotImplemented`] rather than pretending the signal landed.
    ///
    /// Audited per call — delivering a signal is a security-relevant
    /// process-lifecycle decision — and a cross-principal decision (rules 2
    /// and 3, allowed or denied alike) additionally records the caller, the
    /// target, the signal, and which rule decided it.
    pub const SIGNAL: Self = Self(64);
    /// Change the calling process's working directory to `path`
    /// (`.junie/PREREQUISITES2.md` P2).
    ///
    /// Arguments: `path: *const u8` (user pointer) and `path_len: usize` (at
    /// most [`crate::FS_PATH_MAX`]). Returns `0`, or `-errno`. The kernel
    /// copies the path in through the validated `copy_from_user` boundary,
    /// resolves it — relative to the caller's current working directory when
    /// it is not absolute — with the shared path parser, and re-authorises it
    /// as a *searchable directory* through the secured VFS under the caller's
    /// real [`Credentials`](crate::capability), exactly as
    /// [`Self::FS_OPEN`] with [`crate::OpenFlags::DIRECTORY`] would. Only on
    /// success does the resolved, normalised absolute path become the
    /// process's new working directory, against which later relative paths
    /// (to [`Self::FS_OPEN`] and friends) resolve. A path that does not name
    /// a directory, or that the caller may not search, fails closed without
    /// changing the working directory; a build with no filesystem service
    /// wired fails closed with [`Errno::NotImplemented`]. Gated by
    /// [`crate::CapabilityId::FS_ACCESS`].
    pub const FS_CHDIR: Self = Self(65);
    /// Report the calling process's working directory into a caller buffer
    /// (`.junie/PREREQUISITES2.md` P2).
    ///
    /// Arguments: `buf: *mut u8` (user pointer) and `buf_cap: usize` (its
    /// capacity). Returns the number of bytes written — the working
    /// directory as a normalised absolute path (no trailing slash except the
    /// root `/`) — or `-errno`. A buffer too small to hold the whole path
    /// fails closed with [`Errno::BufferTooSmall`] (the path is never
    /// truncated); the caller grows its buffer and retries. Reading one's own
    /// working directory grants no authority over any other principal, so —
    /// unlike [`Self::FS_CHDIR`] — no capability is required and the call is
    /// not audited.
    pub const FS_GETCWD: Self = Self(66);
    /// Resolve a typed resource reference (`plans/ALIAS.md`) and open it to a
    /// new descriptor (`.junie/PREREQUISITES2.md` P5).
    ///
    /// Arguments: `reference: *const u8` (user pointer to the textual
    /// resource reference, e.g. `sys:random`), `reference_len: usize` (at most
    /// [`crate::RESOURCE_REF_MAX`]), and the [`crate::OpenFlags`] bits naming
    /// the access requested. Returns a non-negative descriptor number, or
    /// `-errno`.
    ///
    /// The kernel copies the reference in through the validated
    /// `copy_from_user` boundary, parses it with the single shared reference
    /// parser (`lib/resref`) — never a second parser — and resolves it through
    /// the capability-checked namespace resolver. A reference names a *typed
    /// non-filesystem resource* (there is no `/dev`, `/proc`, or `/sys`), so
    /// this is the resource-reference analogue of [`Self::FS_OPEN`]: the
    /// descriptor it returns is read and written with [`Self::FS_READ`] /
    /// [`Self::FS_WRITE`] and released with [`Self::FS_CLOSE`], but its
    /// backing is the resolved resource rather than a filesystem path.
    ///
    /// Authorisation is per namespace and selector and is decided from the
    /// kernel-attested caller identity (never a caller-supplied one), so the
    /// call carries no blanket dispatch capability: an unprivileged resource
    /// (`sys:random`, `sys:null`) needs none, while a privileged namespace is
    /// checked inside the resolver and fails closed. A malformed reference, an
    /// unknown or not-yet-served namespace, or a selector the caller may not
    /// reach fails closed without minting a descriptor. Every resolution is
    /// audited — opening a resource is a security-relevant decision.
    pub const RESOURCE_OPEN: Self = Self(67);
    /// Read the calling task's **own** kernel-attested [`crate::Origin`].
    ///
    /// Arguments: `out: *mut u8` (user buffer) and `out_cap: usize` (its
    /// capacity, at least [`crate::ORIGIN_WIRE_LEN`]). On success the caller's
    /// own [`Origin`](crate::Origin) — trust domain, owning uid/gid, task id,
    /// process-instance [`ProcId`](crate::ProcId), and the non-secret
    /// effective-capability summary — is written little-endian to `out` and
    /// its byte length returned; a buffer shorter than
    /// [`crate::ORIGIN_WIRE_LEN`] fails closed.
    ///
    /// This is the self-directed twin of [`Self::CALL_PEER_ORIGIN`]: where
    /// that lets a server learn the identity of the *peer* it is servicing,
    /// this lets a task learn its *own*. Every field is read from the caller's
    /// own kernel-held task record (never a caller-supplied value), so a task
    /// can neither forge another principal's origin nor inflate its own — the
    /// summary carries the capability *membership* bitmap only, no capability
    /// tokens. Unprivileged: a task may always learn its own identity (like
    /// [`Self::CLOCK_GET`] / [`Self::BOOT_ID_GET`]) and doing so grants no
    /// authority, so no capability is required and the call is not audited.
    pub const SELF_ORIGIN: Self = Self(68);
    /// Administer the user-account and group databases
    /// (`plans/CAPABILITY_USE.md` CU4).
    ///
    /// Arguments: `req: *const u8` + `req_len: usize` — one versioned, typed
    /// [`crate::users_admin::UsersAdminRequest`] record (create / modify /
    /// delete / lock / unlock an account, edit its grants, replace its
    /// stored password record, create / delete a group, or list either
    /// database's non-secret fields) — and `out: *mut u8` + `out_cap: usize`,
    /// the response buffer the list operations fill (mutating operations
    /// write nothing). Returns the response byte length (`0` for a mutating
    /// operation) or a negative [`Errno`].
    ///
    /// Gated on `CAP_USER_ADMIN` at dispatch and audited per call: editing
    /// the account databases is the account-administration authority and is
    /// never ambient. The kernel additionally enforces, inside the handler,
    /// that a grant edit never widens an account beyond the *caller's own*
    /// effective capability set (delegation narrows), that the last active
    /// administrator can be neither deleted nor locked nor stripped of
    /// `CAP_USER_ADMIN` (user management cannot be bricked), and that every
    /// field passes the same fail-closed `users-v1` validation the on-disk
    /// format enforces. A change binds at the *next* spawn or login; running
    /// processes keep the capability record they were derived with. Password
    /// material crosses this boundary only as a ready salted PBKDF2 record
    /// built by the caller, and no operation ever returns stored password
    /// material.
    pub const USERS_ADMIN: Self = Self(69);
    /// Switch a seat's foreground session — retarget which text console an
    /// unowned seat's input drains to (`plans/DISPLAY.md` D3, the
    /// `chvt`/`VT_ACTIVATE` analogue).
    ///
    /// Arguments: `seat_id: u64` (the seat to retarget) and `console: u32`
    /// (the index of the installed
    /// text console that becomes the seat's foreground). Returns `0` or a
    /// negative [`Errno`]: an unknown seat or console index fails closed
    /// with [`Errno::NotFound`] before any state changes, so a typo can
    /// never strand input on a console that does not exist.
    ///
    /// Gated on `CAP_SEAT_ADMIN` at dispatch and audited per call: moving
    /// the foreground redirects every subsequent keystroke of an unowned
    /// seat, the classic console-hijack primitive, so it is the
    /// seat-multiplexing authority's alone — `CAP_DISPLAY` owns one lease
    /// and cannot re-route a seat. A held seat keeps routing to its owner;
    /// the new foreground takes effect when the lease ends.
    pub const SEAT_SWITCH: Self = Self(70);
    /// Forcibly revoke a seat's current lease — evict a wedged or
    /// switched-away owner (`plans/DISPLAY.md` D3, the DRM
    /// `DROP_MASTER`-by-an-administrator analogue).
    ///
    /// Argument: `seat_id: u64` (the seat whose lease is revoked).
    /// Returns `0` or a negative [`Errno`]:
    /// an unknown seat fails closed with [`Errno::NotFound`], and revoking
    /// an unowned seat refuses with [`Errno::SeatNotOwner`] (there is no
    /// lease to revoke) rather than reporting a success that changed
    /// nothing.
    ///
    /// Gated on `CAP_SEAT_ADMIN` at dispatch and audited per call — the
    /// record carries the evicted owner's task id, so every eviction is
    /// attributable. The seat becomes acquirable immediately and its input
    /// returns to the text foreground; the evicted owner's next owner-gated
    /// call (`display_release` / `keyboard_read` / a future present) fails
    /// closed with the distinct [`Errno::SeatRevoked`], so a well-behaved
    /// compositor learns it lost the seat rather than scribbling over the
    /// new foreground.
    pub const SEAT_REVOKE: Self = Self(71);

    /// Grant (or release) the console's controlling (foreground)
    /// ownership — the drain right on its input queue and the target the
    /// line discipline signals on `^C`/`^Z` (`plans/SPAWN.md` SP9,
    /// `plans/DISPLAY.md` D5 — the `tcsetpgrp` analogue, without the
    /// signal races).
    ///
    /// Arguments: `fd: u32` (a readable inherited standard-stream
    /// descriptor of the **caller's own** table — the console it names is
    /// the one whose ownership changes, the same fd-scoped authority
    /// [`Self::STREAM_INPUT_MODE`] uses) and `pid: i32` (a **live child of
    /// the caller** to make the owner, or `0` to release). While an owner
    /// is recorded, **only the owner** drains the console's input
    /// ([`Self::STREAM_READ`]) or changes its line discipline
    /// ([`Self::STREAM_INPUT_MODE`]) — every other task is refused with
    /// [`crate::Errno::NotForeground`] — and the cooked-mode line
    /// discipline consumes `^C`/`^Z` and delivers
    /// [`crate::Signal::Interrupt`] / [`crate::Signal::Stop`] to the owner
    /// instead of queueing the byte. With no owner (or in raw/secret mode)
    /// every byte flows to the reader unchanged. Returns an error code
    /// (`Ok(0)` on success).
    ///
    /// Fails closed, with layered, capability-minimal authority: a
    /// non-readable or unbacked `fd` and an unknown console are refused
    /// ([`crate::Errno::NotFound`] / [`crate::Errno::NotImplemented`]); a
    /// non-zero `pid` that is not a live child of the caller is
    /// [`crate::Errno::NotFound`] (the drain right only moves down the
    /// spawn chain — inherited and intersected, never widened); and the
    /// slot transition is owner-checked — a grant is honoured only from an
    /// unowned console, the recorded granter, or the current owner, and a
    /// release only from the granter or the owner, anything else refused
    /// with [`crate::Errno::NotForeground`] so a bystander can neither
    /// take the drain right nor open the console by clearing it. A dead
    /// owner never wedges the console: `exit` releases its ownership and
    /// the read gate clears an owner proven dead (task ids are never
    /// reused).
    pub const CONSOLE_FOREGROUND: Self = Self(72);

    /// Create a pipe: a bounded, kernel-buffered unidirectional byte
    /// stream connecting two descriptors of the **caller's own** open
    /// table (`plans/SPAWN.md` SP10 — the `cmd | cmd` primitive).
    ///
    /// Arguments: `out: *mut u32` (a non-null user pointer the kernel
    /// writes two `u32` descriptors into — the read end first, then the
    /// write end). Both descriptors draw from the same per-process
    /// allocator [`Self::FS_OPEN`] and [`Self::RESOURCE_OPEN`] use, are
    /// read/written through [`Self::FS_READ`] / [`Self::FS_WRITE`] (the
    /// file offset is meaningless for a pipe and ignored), and are closed
    /// through [`Self::FS_CLOSE`]. A read on an empty pipe **parks** the
    /// caller until bytes arrive or every write end is closed (then
    /// end-of-stream, `0`); a write to a full pipe parks until space
    /// frees, and a write with every read end closed fails closed with
    /// [`crate::Errno::BrokenPipe`]. An end is handed to a child by
    /// naming its descriptor in a [`crate::SpawnAttach`] wire
    /// ([`SyscallNumber::SPAWN`]). Unprivileged: a pipe reaches only the
    /// caller's own table and carries no authority of its own —
    /// transferring an end rides the `CAP_PROC_SPAWN`-gated spawn.
    pub const PIPE_CREATE: Self = Self(73);

    /// Set the POSIX permission bits of the file or directory at an
    /// absolute path (the `chmod(2)` shape).
    ///
    /// Arguments: `path: *const u8` (a non-null user pointer),
    /// `path_len: usize` (at most [`crate::FS_PATH_MAX`]), and
    /// `mode: u32` — the new permission bits, at most
    /// [`crate::FS_MODE_MASK`] (the `rwx` triads plus the
    /// setuid/setgid/sticky bits); a raw word carrying any higher bit
    /// fails closed with [`crate::Errno::OutOfRange`] at dispatch. The
    /// file-type is not the caller's to change and is not part of the
    /// word. Gated on `CAP_FS_ACCESS` like the other path-taking
    /// filesystem calls; the per-inode rule is the secured VFS's — only
    /// the inode's **owner** may change its mode (holding a capability
    /// does not override ownership), the covering mount must be
    /// writable, and ownership, ACL, and capability gate are untouched.
    pub const FS_SET_MODE: Self = Self(74);

    /// Resolve a published port name to its live IPC endpoint id.
    ///
    /// Arguments: `name: *const u8` (a non-null user pointer to the ASCII
    /// name bytes) and `name_len: usize` (at most
    /// [`crate::PORT_NAME_MAX_LEN`]). The kernel copies the bytes in
    /// through the validated `copy_from_user` boundary, validates them
    /// against the [`crate::PortName`] grammar (fail closed — a byte
    /// sequence that is not a well-formed name is refused before the
    /// registry is consulted), and looks the name up in the kernel's
    /// named-port registry. Returns the bound endpoint id — the value the
    /// caller then passes to [`Self::IPC_SEND`] / [`Self::IPC_RECV`] — or
    /// [`crate::Errno::NotFound`] when no port is currently published
    /// under that name.
    ///
    /// Unprivileged and unaudited, like the other pure observers
    /// ([`Self::CAP_QUERY`], [`Self::CLOCK_GET`]): resolving a name grants
    /// nothing — every send is still capability-checked at the port by
    /// [`Self::IPC_SEND`], and publication itself is a kernel-side,
    /// bind-authority-checked operation. This is how a process reaches a
    /// well-known service port (a desktop input feed, a system service)
    /// without a compiled-in endpoint number.
    pub const PORT_RESOLVE: Self = Self(75);

    /// Map a byte range of an open file into the calling process's own
    /// address space as a demand-paged, read-only private mapping
    /// (the `mmap(2)` shape; see [`crate::memory`]).
    ///
    /// Arguments: `fd: u32` (a descriptor the caller opened for reading,
    /// backed by a filesystem path), `offset: u64` (the file byte offset
    /// the mapping starts at; must be page-aligned), and `len: u64` (the
    /// mapping length in bytes, rounded up to whole pages). Returns the
    /// page-aligned base address of the new region. No page is read or
    /// backed at call time: each page is populated on first access by the
    /// kernel's fault path, reading through the secured VFS under the
    /// **mapping-time** identity (uid + capability snapshot, the same
    /// authority model as the open descriptor itself), so a 20 TB file
    /// costs only the pages actually touched. A page whose first byte
    /// lies at or past end-of-file is not backed: touching it terminates
    /// the faulting process (the `SIGBUS` analogue, fail closed); a page
    /// straddling end-of-file is zero-filled past the end. The mapping is
    /// always read-only and never executable (W^X); it survives a later
    /// `fs_close` of `fd` (the region carries its own authority snapshot,
    /// exactly as an open descriptor would). Unprivileged like
    /// [`SyscallNumber::MEM_MAP`] — the region lands only in the caller's
    /// own hardware-isolated space — but the handler requires the
    /// descriptor to be open for reading and `CAP_FS_ACCESS`, the same
    /// gate `fs_read` applies. The page-rounded extent is charged against
    /// the caller's `AddressSpaceBytes` limit.
    pub const FILE_MAP: Self = Self(76);

    /// Release a region previously returned by [`SyscallNumber::FILE_MAP`]
    /// from the calling process's own address space.
    ///
    /// Arguments: `base: u64` (the region's base, exactly as returned by
    /// `file_map`) and `len: u64` (the region's full length in bytes, as
    /// requested at map time). Only the whole region can be released;
    /// a partial or unknown range fails closed with
    /// [`crate::Errno::NotFound`]. Pages never touched were never backed
    /// and cost nothing to release; resident pages are unmapped and their
    /// frames zeroed on free (secret hygiene). Unprivileged, mirroring
    /// [`SyscallNumber::MEM_UNMAP`].
    pub const FILE_UNMAP: Self = Self(77);

    /// Inject one decoded pointer event into the kernel seat registry
    /// (`plans/PI.md` P11 — input follows the surface owner; the pointer
    /// analogue of [`SyscallNumber::KEY_INJECT`]).
    ///
    /// Arguments: `seat: u64` (the seat the event belongs to — the seat
    /// whose pointing device produced it, the boot seat `0` for a directly
    /// attached device; an unknown id fails closed with
    /// [`crate::Errno::NotFound`]), `buf: *const u8` (one
    /// [`crate::input::PointerInput`] record) and `len: usize` (its length,
    /// [`crate::input::PointerInput::WIRE_LEN`]). Returns the number of
    /// bytes consumed, or a negative error code. The pointer-input driver
    /// (virtio-input / USB HID) emits the *device-resolved event* — a
    /// motion, button edge, or scroll step — and the kernel seat registry
    /// decides the destination by who currently holds that seat: while the
    /// desktop (window manager) owns it the whole record is routed to the
    /// seat's pointer channel (drained by [`SyscallNumber::POINTER_READ`]);
    /// while the seat is unowned the record is consumed and discarded —
    /// the text console has no pointer consumer, and dropping at the
    /// arbiter keeps the routing policy out of the device driver exactly
    /// as for key edges. Gated by [`crate::CapabilityId::INPUT_INJECT`]:
    /// feeding the system's input stream is privileged, never ambient. A
    /// malformed record is refused fail-closed.
    pub const POINTER_INJECT: Self = Self(78);

    /// Read one decoded pointer event from a seat's pointer channel
    /// (`plans/PI.md` P11 — pointer input for the desktop; the pointer
    /// analogue of [`SyscallNumber::KEYBOARD_READ`]).
    ///
    /// Arguments: `seat: u64` (the seat whose channel is drained; an
    /// unknown id fails closed with [`crate::Errno::NotFound`]),
    /// `buf: *mut u8` (a buffer of at least
    /// [`crate::input::PointerInput::WIRE_LEN`] bytes) and `len: usize`
    /// (its length). Returns the number of bytes written — one
    /// [`crate::input::PointerInput`] record — or `0` when the channel is
    /// momentarily drained; a buffer too small to hold a record fails
    /// closed with [`crate::Errno::BufferTooSmall`]. The task that owns the
    /// seat (the window manager / desktop session) drains the records the
    /// seat registry routed to it while it held the seat. Gated by
    /// [`crate::CapabilityId::INPUT_READ`] **and** owner-gated against the
    /// seat's live lease: a caller that does not hold the seat is refused
    /// with [`crate::Errno::SeatNotOwner`] (or
    /// [`crate::Errno::SeatRevoked`] after an administrative eviction), so
    /// pointer input is delivered only to whoever currently owns the
    /// surface, and an unattached channel denies rather than leaking to a
    /// device (`plans/DISPLAY.md`).
    pub const POINTER_READ: Self = Self(79);

    /// Attach a filesystem driver to a runtime block source and publish
    /// the volume's root (`plans/DEVICES.md` D3b).
    ///
    /// Arguments: a non-null pointer to an encoded
    /// [`crate::volume::VolumeAttachRequest`] and its length (at most
    /// [`crate::volume::VOLUME_ATTACH_MAX_LEN`]). The request names a
    /// block-service endpoint + shared data window ([`crate::blkio`],
    /// both held as kernel grants by the calling volume manager), the
    /// probed partition extent, the filesystem type, and the catalog name
    /// the root is projected under (`/Storage/<name>`). The kernel
    /// re-validates everything against live state — the endpoint, the
    /// window, the device geometry, the extent bounds, the name — opens
    /// the filesystem read-only or read-write per the device's write
    /// policy, mounts it with the removable-media flags
    /// (`nosuid,nodev,noexec`), and publishes its stable identity into
    /// the volume forest so `id::<volume-id>/…` paths resolve. Requires
    /// `CAP_FS_MOUNT`; every attach decision is audited.
    pub const VOLUME_ATTACH: Self = Self(80);

    /// Detach a runtime-attached volume: flush it, retract its mount, and
    /// unpublish its root (`plans/DEVICES.md` D3b).
    ///
    /// Arguments: a non-null pointer to an encoded
    /// [`crate::volume::VolumeDetachRequest`] (the volume's stable
    /// 16-byte identity plus the force byte) and its length (exactly
    /// [`crate::volume::VOLUME_DETACH_LEN`]). Only a volume attached
    /// through [`SyscallNumber::VOLUME_ATTACH`] can be detached — the
    /// boot volumes are permanent and refuse this path. The volume is
    /// flushed first and a plain detach fails closed on a flush error (or
    /// an unavailable, surprise-removed volume) rather than discarding
    /// uncommitted data; a **force** detach (`plans/DEVICES.md` D4b)
    /// retracts the volume anyway, deliberately discarding the retained
    /// set with its own audit event. Requires `CAP_FS_MOUNT`; every
    /// detach decision is audited.
    pub const VOLUME_DETACH: Self = Self(81);

    /// Grant the serving task of a call endpoint the right to map a shared
    /// memory region the caller owns (`plans/DISPLAY.md` D7a — the display
    /// client hands its frame buffer to the display service).
    ///
    /// Arguments: the region id a prior [`SyscallNumber::SHM_CREATE`]
    /// returned, then the call-endpoint id whose **serving task** receives
    /// the grant. The caller must itself hold a `Shared` grant for the
    /// region (it can map what it shares — delegation never widens
    /// authority), and the endpoint must have a live server; the recipient
    /// is resolved kernel-side from the endpoint at grant time, never a
    /// caller-supplied (recyclable) PID, so a grant cannot land on a reused
    /// task id. Returns the minted unforgeable grant handle, which the
    /// caller forwards in-band for the recipient's
    /// [`SyscallNumber::SHM_MAP`] — the handle is owner-checked there, so
    /// the number is useless to a bystander. Requires `CAP_SHM`; every
    /// mint is audited, exactly as `shm_create`.
    pub const SHM_GRANT: Self = Self(82);

    /// Report whether the in-flight caller of a served call endpoint holds
    /// a seat's live lease (`plans/DISPLAY.md` D7a — the display service's
    /// per-present check).
    ///
    /// Arguments: the endpoint id, the ticket of the in-service call
    /// (exactly as [`SyscallNumber::CALL_PEER_ORIGIN`]), then the seat id.
    /// The caller must own the endpoint and hold its receive capability;
    /// the check is valid only between `call_recv` and `call_reply`, so a
    /// server learns seat facts only about a task it is actively
    /// servicing — seat ownership is never enumerable (that listing stays
    /// behind `CAP_SYSINFO_HW`). Returns the live lease's generation
    /// (`>= 1`) when the peer holds the seat; fails closed with
    /// [`crate::Errno::SeatNotOwner`] (another task holds it or it is
    /// unowned), [`crate::Errno::SeatRevoked`] (the peer's lease was
    /// revoked and is unacknowledged), or [`crate::Errno::NotFound`] (no
    /// such seat, endpoint, or in-flight ticket). No capability beyond the
    /// endpoint's own receive gate: the authority is serving the in-flight
    /// call, exactly as `call_peer_origin`. Not audited per call — it is
    /// the per-frame present gate (the kernel-side `PresentGate` is not
    /// audited per check either); the security decision it feeds (a refused
    /// present) is the service's to log.
    pub const CALL_PEER_SEAT: Self = Self(83);

    /// Read one extended attribute of the file or directory at an absolute
    /// path (the `getxattr(2)` shape).
    ///
    /// Arguments: `path: *const u8` + `path_len: usize` (at most
    /// [`crate::FS_PATH_MAX`]), `key: *const u8` + `key_len: usize` (a
    /// `lib/fsmeta`-grammar `namespace.rest` key, `1..=`
    /// [`crate::FS_ATTR_KEY_MAX`] bytes), and `value_out: *mut u8` +
    /// `value_out_len: usize` (the caller's buffer). Returns the number of
    /// value bytes written. Gated on `CAP_FS_ACCESS`; the per-inode rule is
    /// the secured VFS's — the caller needs read permission on the node,
    /// the `system`/`trusted` namespaces are refused outright, and a
    /// `required_cap` gate on the node is honoured. Fails closed with
    /// [`crate::Errno::NoData`] when the node carries no such attribute
    /// (a value may legitimately be empty, so absence is never an empty
    /// read), [`crate::Errno::BufferTooSmall`] when the value does not fit
    /// (never truncated), and [`crate::Errno::NotSupported`] on a mount
    /// whose format stores no attributes.
    pub const FS_ATTR_GET: Self = Self(84);

    /// Set (insert or replace) one extended attribute of the file or
    /// directory at an absolute path (the `setxattr(2)` shape).
    ///
    /// Arguments: `path`/`path_len`, `key`/`key_len` (as
    /// [`Self::FS_ATTR_GET`]), and `value: *const u8` + `value_len: usize`
    /// (at most [`crate::FS_ATTR_VALUE_MAX`] bytes; the value is opaque to
    /// the kernel). The write is one copy-on-write transaction in the
    /// driver — fully applied or not at all. Gated on `CAP_FS_ACCESS`; the
    /// secured VFS requires write permission on the node, a writable
    /// mount, a valid `lib/fsmeta` key in a non-privileged namespace, and
    /// honours a `required_cap` gate. Refusals include
    /// [`crate::Errno::NoSpace`] (the per-inode attribute bounds) and
    /// [`crate::Errno::NotSupported`] (a mount without attribute storage).
    pub const FS_ATTR_SET: Self = Self(85);

    /// Enumerate the extended-attribute keys of the file or directory at
    /// an absolute path, one key per call (the `fs_readdir` iteration
    /// shape rather than `listxattr(2)`'s packed buffer).
    ///
    /// Arguments: `path`/`path_len`, `index: u64` (the position to yield),
    /// and `key_out: *mut u8` + `key_out_len: usize`. Returns the key's
    /// byte length (written into `key_out`), or `0` once `index` is past
    /// the last visible attribute — a real key is never empty, so `0`
    /// unambiguously means end-of-list. Keys whose namespace the caller
    /// may not read (`system`/`trusted`) are omitted, never revealed.
    /// Gated on `CAP_FS_ACCESS`; the secured VFS requires read permission
    /// on the node. Iteration order is the driver's stable on-disk order.
    pub const FS_ATTR_LIST: Self = Self(86);

    /// Remove one extended attribute of the file or directory at an
    /// absolute path (the `removexattr(2)` shape).
    ///
    /// Arguments: `path`/`path_len` and `key`/`key_len` (as
    /// [`Self::FS_ATTR_GET`]). One copy-on-write transaction. Gated on
    /// `CAP_FS_ACCESS`; the secured VFS requires write permission on the
    /// node, a writable mount, and a non-privileged namespace. Fails
    /// closed with [`crate::Errno::NoData`] when the node carries no such
    /// attribute and [`crate::Errno::NotSupported`] on a mount without
    /// attribute storage.
    pub const FS_ATTR_REMOVE: Self = Self(87);

    /// Bind an asynchronous IPC message port owned by the calling task.
    ///
    /// Arguments: `endpoint: u64` (the port id senders will name),
    /// `max_payload: usize`, `capacity: usize` (both fail-closed bounds:
    /// the payload cap is limited by the global ABI message ceiling and
    /// the capacity bounds the kernel memory one port may pin). The port
    /// accepts [`Self::IPC_SEND`]s from any capable sender — each message
    /// carries its sender's kernel-attested [`crate::Origin`], so the
    /// owner authenticates senders on receive — and is drained only by
    /// its owner through [`Self::IPC_RECV`], parked via a wait-set member
    /// of kind [`crate::WaitSourceKind::Port`]. A reserved well-known
    /// endpoint id ([`crate::ipc::is_reserved_endpoint`]) additionally
    /// requires `CAP_IPC_BIND_PRIVILEGED`, exactly as
    /// [`Self::CALL_CREATE`] does, so a squatter cannot claim a service
    /// rendezvous; an id already bound fails closed with
    /// [`crate::Errno::AlreadyExists`]. The port is torn down when its
    /// owner exits.
    pub const PORT_BIND: Self = Self(88);

    /// Read the kernel's boot-static machine summary
    /// ([`crate::BootFacts`]): the CPU architecture, the boot CPU's
    /// discovered model name ([`crate::CpuName`]), the number of
    /// processor cores brought under the scheduler, and the installed
    /// physical memory the boot path discovered.
    ///
    /// Arguments: `out` (user pointer) and `out_cap` (its capacity in
    /// bytes). On success the kernel writes the
    /// [`crate::BOOT_FACTS_WIRE_LEN`]-byte encoding and returns the byte
    /// count; a capacity below the wire length fails closed with
    /// [`crate::Errno::BufferTooSmall`]. The facts are minted once at boot
    /// from kernel-attested state and never change; like
    /// [`Self::BOOT_ID_GET`] the value is public machine shape, not live
    /// state, so the call is unprivileged and not audited. Live figures
    /// (memory usage, per-process detail) stay behind the
    /// capability-gated System Information API.
    pub const BOOT_FACTS_GET: Self = Self(89);

    /// Delegate one of the caller's own open **filesystem** descriptors to
    /// another live process as a one-shot grant
    /// (`plans/CAPABILITY_USE.md` CU6 — the user-mediated file picker's
    /// hand-off).
    ///
    /// Arguments: `fd` (a descriptor of the caller's own open table backed
    /// by a filesystem path — a pipe, resource, or already-delegated
    /// descriptor is refused, so delegation never chains) and `pid` (the
    /// recipient's kernel task id, taken from a kernel-attested source such
    /// as `call_peer_origin` — task ids are never reused, so the grant can
    /// never land on a recycled identity). The kernel captures the
    /// *caller's* identity and effective capability set with the
    /// descriptor's path and open flags, and mints the recipient an
    /// unforgeable handle that resolves only when presented by the
    /// recipient itself ([`Self::FD_REDEEM`]). The handle value travels
    /// back to the caller, who forwards it in-band (e.g. over the window
    /// channel); the number is useless to a bystander. Delegation never
    /// widens authority: the redeemed descriptor's every operation is
    /// re-authorised through the secured VFS under the *grantor's* captured
    /// identity, exactly as the grantor's own descriptor would be.
    pub const FD_GRANT: Self = Self(90);

    /// Redeem a [`Self::FD_GRANT`] handle minted to the calling task,
    /// installing the delegated file into the caller's own open table.
    ///
    /// Arguments: `handle` (the grant handle received in-band). The
    /// redemption is **one-shot**: the grant record is consumed only when
    /// the descriptor allocation succeeds, so a table-full refusal leaves
    /// the grant intact for a retry and a redeemed handle can never be
    /// redeemed twice. A handle minted to another task resolves to nothing
    /// (`NotFound`), indistinguishable from a handle that never existed.
    /// The call is unprivileged: receiving user-mediated, already-checked
    /// authority is the point of the delegation, and every later operation
    /// on the descriptor is still VFS-checked under the grantor's captured
    /// identity.
    pub const FD_REDEEM: Self = Self(91);
    /// Mark the calling process's entire anonymous memory — current and
    /// future — as pinned: ineligible for the compressed `ramzip` tier and
    /// any future lower swap tier (`plans/STRESSTEST.md` ST2, the API
    /// behind `plans/SWAPSWAPSWAP.md` section 5's pinned class).
    ///
    /// No arguments. Returns an error code (`Ok(0)` on success; already
    /// pinned is success — the process is in the requested state). Gated by
    /// [`crate::CapabilityId::MEM_PIN`] and audited per call: exempting
    /// memory from pressure management system-wide is a
    /// denial-of-service lever against every other tenant. Bounded by the
    /// caller's effective [`crate::LimitKind::PinnedMemoryBytes`] soft
    /// bound: a pin whose anonymous bytes already exceed the bound fails
    /// closed with [`crate::Errno::OutOfRange`], and while pinned the same
    /// bound caps further anonymous growth (`mem_map`, demand-grown
    /// stack). Pinning is process-scoped state: it is not inherited across
    /// [`SyscallNumber::SPAWN`] (a child starts unpinned) and is cleared
    /// on exit. It grants no residency promise beyond "never enters a swap
    /// tier": pages are still faulted lazily, zero-on-free and encryption
    /// guarantees are unchanged, and the process stays killable.
    pub const MEM_PIN: Self = Self(92);
    /// Clear the calling process's [`SyscallNumber::MEM_PIN`] mark,
    /// restoring its anonymous memory's eligibility for the compressed
    /// tier (`plans/STRESSTEST.md` ST2).
    ///
    /// No arguments. Returns an error code (`Ok(0)` on success; already
    /// unpinned is success). Requires no capability — releasing the
    /// caller's own exemption narrows its footprint and grants nothing
    /// (the `mem_unmap` posture) — but is audited per call like
    /// [`SyscallNumber::MEM_PIN`], so the audit trail carries both edges
    /// of every pin window.
    pub const MEM_UNPIN: Self = Self(93);

    /// Operate on the calling process's own **signal intake** — the
    /// fail-closed signal-observation opt-in (`plans/STRESSTEST.md` ST3).
    ///
    /// Arguments: `op` (a [`crate::SignalIntakeOp`] discriminant; an unknown
    /// value fails closed with [`Errno::OutOfRange`]). Returns a
    /// value-or-negative-errno `u64`: `Enable`/`Disable` return `0`, `Take`
    /// returns the drained signal's wire discriminant
    /// ([`crate::Signal::as_u32`]).
    ///
    /// With the intake enabled, a termination-request signal
    /// ([`crate::Signal::Interrupt`] or [`crate::Signal::Terminate`])
    /// delivered to the process — by a parent's [`Self::SIGNAL`] or by the
    /// console line discipline's foreground `^C` — is recorded as one
    /// pending observable event instead of terminating it. The pending
    /// event is waitable through a wait-set member of kind
    /// [`crate::WaitSourceKind::Signal`] and drained with
    /// [`crate::SignalIntakeOp::Take`], so the observer stays event-driven
    /// (never a poll loop). The signals stay honest: [`crate::Signal::Kill`]
    /// is never observable or maskable, `Stop`/`Continue` stay
    /// scheduler-side, and a **second** termination-request signal arriving
    /// while one is pending undrained escalates to the default terminate
    /// path — an opted-in process that stops draining stays killable with a
    /// plain `^C ^C`, no capability, no privileged override.
    ///
    /// Own-process disposition needs no capability (the same tier as
    /// [`Self::STREAM_INPUT_MODE`]); every call is audited like
    /// [`Self::SIGNAL`] itself, so the trail carries the opt-in, the
    /// opt-out, and each observed delivery's drain. The opt-in is
    /// process-scoped state: never inherited across [`Self::SPAWN`] and
    /// cleared on exit.
    pub const SIGNAL_INTAKE: Self = Self(94);

    /// Set the calling task's **scheduling class** — enter or leave the
    /// strict-priority real-time band (`plans/USB.md`; `SchedClass`).
    ///
    /// Argument: `realtime` (`u32` boolean — non-zero requests the
    /// strict-priority real-time scheduling class, zero requests the fair
    /// time-shared class; the `SchedClass` type lives in `kernel/sched/api`).
    /// Returns an error code
    /// (`Ok(0)` on success). Self-only: a task can set only *its own* class,
    /// so there is no target-task argument and no way to reclass another
    /// principal (no ambient authority).
    ///
    /// Gated by [`crate::CapabilityId::SCHED_REALTIME`]: a real-time task is
    /// dispatched ahead of every time-shared task on its CPU and is never
    /// preempted by one, so a CPU-bound workload cannot delay it — the
    /// guarantee an interrupt-serving driver needs (the microkernel
    /// threaded-IRQ / `SCHED_FIFO` analogue). The whole syscall carries the
    /// gate, in both directions: a task's scheduling class is per-task state
    /// and the capability is static (a signed manifest request intersected
    /// with the user's grants), so only a capability holder is ever
    /// real-time and only a holder ever needs to leave the class — gating
    /// both directions denies nothing a legitimate caller could do while
    /// keeping the privileged direction (entering) firmly closed to everyone
    /// else (fail closed; no ambient authority). The change governs the
    /// task's next enqueue onward; the usual caller elevates itself once at
    /// start-up, then blocks on its device IRQ, so every subsequent wake is
    /// strict-priority. Every call is audited: entering or leaving strict
    /// priority is a security-relevant scheduling decision, and the volume
    /// is low (once per driver start), so the record cannot drown the log.
    pub const SCHED_SET_REALTIME: Self = Self(95);

    /// Set the owning user and/or group of the file or directory at an
    /// absolute path (the `chown(2)` / `chgrp(2)` shape).
    ///
    /// Arguments: `path: *const u8` (a non-null user pointer),
    /// `path_len: usize` (at most [`crate::FS_PATH_MAX`]), `uid: u32` and
    /// `gid: u32` — the new owning user and group. Either field may be
    /// [`crate::FS_OWNER_UNCHANGED`] to leave that field as it is, so an
    /// owner-only or group-only change touches only the field it names; a
    /// call leaving *both* unchanged is a well-formed no-op.
    ///
    /// Gated on `CAP_FS_ACCESS` at dispatch like the other path-taking
    /// filesystem calls; the per-inode rule is the secured VFS's and is
    /// stricter than a mode change. Reassigning the **uid**, or setting a
    /// **gid** the caller is not a member of, is the privileged
    /// [`crate::CapabilityId::FS_CHOWN`] operation (the Unix `CAP_CHOWN`
    /// model) and is refused without it. Absent that capability, only the
    /// node's **owner** may change the group, and only to a group the caller
    /// already belongs to (its egid or a supplementary group) — the standard
    /// unprivileged `chgrp`. The covering mount must be writable. Any
    /// successful ownership change **clears the setuid and setgid bits**, so
    /// a reassigned file can never become a setuid-to-someone-else
    /// escalation. Fail closed: a refused change leaves the node untouched.
    pub const FS_SET_OWNER: Self = Self(96);

    /// Create a pseudo-terminal (PTY): one kernel object joining a
    /// **master** end (held by a terminal emulator) and a **slave** end
    /// (wired as a child's fd 0/1/2), whose slave carries the same
    /// console-class line discipline a hardware-console-backed shell gets
    /// (`plans/PTY.md`). This is the primitive the graphical terminal hosts
    /// its shell over, in place of two raw pipes with no discipline.
    ///
    /// Arguments: `out: *mut u32` (a non-null user pointer the kernel
    /// writes two `u32` descriptors into — the master end first, then the
    /// slave end), and `rows: u32` / `cols: u32`, the pty's initial
    /// character-cell geometry (the emulator's own `COLS`×`ROWS`). Each
    /// dimension must be non-zero and fit a `u16`; a zero or oversized
    /// dimension fails closed with [`crate::Errno::OutOfRange`] before any
    /// state is touched.
    ///
    /// Both descriptors draw from the same per-process allocator
    /// [`Self::FS_OPEN`] and [`Self::PIPE_CREATE`] use, are read/written
    /// through [`Self::FS_READ`] / [`Self::FS_WRITE`] (the file offset is
    /// meaningless and ignored), and are closed through [`Self::FS_CLOSE`].
    /// A **master write** is the terminal's keystrokes fed through the input
    /// discipline (cooked-mode local echo onto the master's read side,
    /// `^C`/`^Z` delivered as [`crate::Signal::Interrupt`] /
    /// [`crate::Signal::Stop`] to the slave's foreground job); a **master
    /// read** drains the slave's (cooked) program output; a **slave read**
    /// drains the input; a **slave write** is cooked (`ONLCR`) onto the
    /// output. A read on an empty ring **parks** the caller until bytes
    /// arrive or every peer end is closed (then end-of-stream, `0`); a
    /// write to a full ring parks until space frees, and a write with every
    /// peer end closed fails closed with [`crate::Errno::BrokenPipe`].
    ///
    /// The slave is a *tty*: [`Self::STREAM_INPUT_MODE`],
    /// [`Self::TERMINAL_SIZE`], and [`Self::CONSOLE_FOREGROUND`] all
    /// recognise a slave descriptor and route to the pty's own discipline,
    /// exactly as they do for a console-backed stream. An end is handed to a
    /// child by naming its descriptor in a [`crate::SpawnAttach`] wire
    /// ([`SyscallNumber::SPAWN`]). Unprivileged, like [`Self::PIPE_CREATE`]:
    /// a pty reaches only the caller's own table and carries no authority of
    /// its own — transferring the slave rides the `CAP_PROC_SPAWN`-gated
    /// spawn.
    pub const PTY_CREATE: Self = Self(97);

    /// Set a pseudo-terminal's character-cell geometry from its **master**
    /// end — the graphical terminal's window-resize path (`plans/PTY.md`).
    ///
    /// A pty is created ([`Self::PTY_CREATE`]) with a fixed initial
    /// `rows`×`cols`; when the user drag-resizes the terminal window the
    /// emulator recomputes the new character grid and calls this to update
    /// the shared [`crate::TerminalSize`] both ends observe, so the shell's
    /// prompt sizing ([`Self::TERMINAL_SIZE`]) and any full-screen program
    /// track the real window. It is the tty `TIOCSWINSZ` analogue.
    ///
    /// Arguments: `fd: u32` (a pty **master** descriptor of the caller —
    /// anything else fails closed with [`crate::Errno::NotFound`], never
    /// leaking which case occurred), and `rows: u32` / `cols: u32`, the new
    /// geometry. Each dimension must be non-zero and fit a `u16`; a zero or
    /// oversized dimension fails closed with [`crate::Errno::OutOfRange`]
    /// before any state is touched. Unprivileged, exactly like
    /// [`Self::PTY_CREATE`]: it reaches only the caller's own pty and carries
    /// no authority of its own.
    pub const PTY_SET_SIZE: Self = Self(98);

    /// Post a request to a call endpoint **without blocking**, arming a
    /// per-request deadline, and receive the opaque ticket correlating its
    /// reply (`plans/FIX-IO.md` IO1 — the asynchronous half of
    /// [`SyscallNumber::IPC_CALL`]).
    ///
    /// [`SyscallNumber::IPC_CALL`] bundles post + park + reap with no
    /// deadline, so one wedged callee parks the caller forever and a caller
    /// can drive only one device at a time. This splits the post out: it
    /// enqueues the request, wakes the bound server, arms a one-shot deadline,
    /// and returns immediately so the caller can multiplex many outstanding
    /// requests (one per device) on a wait-set
    /// ([`crate::WaitSourceKind::CallReply`]) and reap each with
    /// [`SyscallNumber::CALL_REAP`].
    ///
    /// Arguments: `endpoint: u64` — the call-endpoint id; `request: *const u8`
    /// and `request_len: usize` — the request payload; `ticket_out: *mut u64`
    /// — receives the minted ticket; `deadline_ns: u64` — the relative
    /// timeout after which the reap reports [`crate::Errno::TimedOut`]
    /// (`u64::MAX` = no deadline, the `waitset_wait`/`irq_wait` convention).
    /// Returns `0`, or `-errno`.
    ///
    /// The same capability, per-endpoint grant, and size checks as
    /// [`SyscallNumber::IPC_CALL`] are enforced kernel-side before anything is
    /// posted (no new authority; the endpoint re-checks its send capability).
    /// A closed or unknown endpoint, a missing capability, or an over-capacity
    /// endpoint each fail closed. A build with no call-endpoint registry
    /// wired fails closed with [`crate::Errno::NotImplemented`].
    pub const CALL_POST: Self = Self(99);

    /// Reap the reply to a request posted with [`SyscallNumber::CALL_POST`],
    /// **without blocking** (`plans/FIX-IO.md` IO1).
    ///
    /// Arguments: `endpoint: u64` — the call-endpoint id; `ticket: u64` — the
    /// ticket [`SyscallNumber::CALL_POST`] minted; `reply: *mut u8` and
    /// `reply_cap: usize` — the buffer the reply is copied into. Returns the
    /// number of reply bytes written (`>= 0`), or `-errno`.
    ///
    /// Non-blocking: returns [`crate::Errno::WouldBlock`] while the reply is
    /// still pending, [`crate::Errno::TimedOut`] once the ticket's deadline
    /// has elapsed (the kernel retires the ticket and best-effort cancels the
    /// in-flight request — a wedged device thus fails closed deterministically
    /// rather than parking the caller forever), and [`crate::Errno::NotFound`]
    /// for a cancelled ticket, a torn-down endpoint, or a ticket that is not
    /// this caller's (no existence oracle — a foreign ticket is
    /// indistinguishable from an unknown one). A reply larger than `reply_cap`
    /// fails closed with [`crate::Errno::BufferTooSmall`]. The caller parks on
    /// a [`crate::WaitSourceKind::CallReply`] wait-set member between reaps; the
    /// reap itself never blocks.
    pub const CALL_REAP: Self = Self(100);

    /// Withdraw one outstanding request posted with
    /// [`SyscallNumber::CALL_POST`], freeing the endpoint slot deterministically
    /// (`plans/FIX-IO.md` IO1).
    ///
    /// Arguments: `endpoint: u64` — the call-endpoint id; `ticket: u64` — the
    /// ticket to withdraw. Returns `0` if a call the caller posted was
    /// cancelled, else `-errno`.
    ///
    /// A per-ticket form of the task-exit call scrub: a consumer abandoning a
    /// wedged transfer cancels it so the slot is not held until the endpoint
    /// tears down. Only the ticket's own poster may cancel it; a foreign,
    /// unknown, or already-completed ticket fails closed with
    /// [`crate::Errno::NotFound`] (no existence oracle).
    pub const CALL_CANCEL: Self = Self(101);

    /// Publish the recovery health of the fault domain the calling driver
    /// *owns* — its own interior hardware-tree node — into the live tree
    /// (`plans/FIX-IO.md` IO4, cross-process fault-domain propagation).
    ///
    /// A bus/hub/controller driver that owns an interior node beneath which a
    /// group of devices hang turns a controller-wide blip (an HC reset, a hub
    /// mid-reset) into *one* fault-domain event: it records the node's
    /// [`crate::blkio::FaultDomainState`] here, and the reactive tree
    /// observers — the device manager — see a coherent recovery episode
    /// across the subtree rather than N spurious child removals. This is a
    /// **distinct** signal from the surprise-removal hotplug path
    /// ([`SyscallNumber::HW_REMOVE_NODE`]): the node stays present, only its
    /// health changes, so a merely-recovering subtree is never torn down.
    ///
    /// Argument: `health: u64` — the [`crate::blkio::FaultDomainState::as_u8`]
    /// discriminant of the new health (`0` Healthy, `1` Recovering, `2`
    /// Offline). Any other value fails closed with [`Errno::OutOfRange`].
    /// Returns `0`, or `-errno`.
    ///
    /// The kernel resolves the *caller's own* matched node from its task id
    /// (never a caller-supplied node id), so a driver can only ever set the
    /// health of the interior node it was autoloaded for — no ambient
    /// authority, no forging another driver's health. It shares the
    /// [`crate::CapabilityId::HW_EMIT`] grant with the emit/remove hotplug
    /// path (a driver that may reshape its own subtree may report its own
    /// health) and is audited per call (low-volume, security-relevant). A
    /// caller with no loaded node, or a build with no hardware-tree store
    /// wired, fails closed ([`Errno::PermissionDenied`] /
    /// [`Errno::NotImplemented`]).
    pub const HW_NODE_HEALTH: Self = Self(102);

    /// Report the hardware-tree node id the calling driver was autoloaded for
    /// — the driver's own place in the discovered topology
    /// (`plans/FIX-IO.md` IO4, leaf-side fault-domain attribution).
    ///
    /// A user-space driver receives its device-resource grants at spawn, but
    /// not its node's *identity*, so it cannot locate itself in the hardware
    /// tree ([`SyscallNumber::HW_TREE_READ`]) to resolve its fault-domain
    /// ancestors. This returns that identity: the id of the node the driver
    /// bound to, so a leaf block driver can read the published
    /// [`crate::blkio::FaultDomainState`] of its parent bus/hub/controller
    /// chain ([`crate::hwtree::ancestor_imposed_status`]) and attribute a
    /// controller-wide blip to the fault domain rather than to the disk.
    ///
    /// Takes no argument. Returns the caller's own node id (a non-negative
    /// [`crate::hwtree::HwNode`] id) on success, or `-errno`.
    ///
    /// The kernel resolves the node from the caller's task id (never a
    /// caller-supplied id), so a driver only ever learns its *own* identity —
    /// no ambient authority and no window onto the global tree. It is the
    /// unprivileged self-identity baseline (like reading one's own pid or
    /// growing one's own address space with [`SyscallNumber::MEM_MAP`]): it
    /// needs no capability and is not audited. A caller with no matched node
    /// (not an autoloaded driver) fails closed with [`Errno::NotFound`].
    pub const HW_SELF_NODE: Self = Self(103);

    /// Change a process's time-shared scheduling service level
    /// ([`crate::SchedPriority`]) — the Switchboard's "lower priority"
    /// recovery action (`plans/NEW-TASKBAR.md` T12).
    ///
    /// Arguments: `pid` (sign-extended `i32`, the target process) and
    /// `priority` (a [`crate::SchedPriority`] wire discriminant; `0` and
    /// unknown values fail closed with [`Errno::OutOfRange`]).
    ///
    /// The target rule mirrors [`SyscallNumber::SIGNAL`]: the caller may
    /// act on its **own child**, else a process of its **own principal**,
    /// else it needs [`crate::CapabilityId::PROC_CONTROL`] — so the
    /// dispatcher cannot gate the call with one capability and the handler
    /// decides per target. **Raising** the level (toward
    /// [`crate::SchedPriority::High`]) additionally requires
    /// `CAP_PROC_CONTROL` regardless of the target rule, so no user can
    /// weight their own work above other principals' fair share; lowering
    /// and re-stating the current level follow the plain target rule.
    /// Cross-principal outcomes and every raise attempt are audited.
    pub const SCHED_SET_PRIORITY: Self = Self(104);

    /// End the machine's power state: flush every mounted volume, then
    /// power the platform off or reset it (`plans/NEW-TASKBAR.md` T13).
    ///
    /// Argument: `action` (a [`crate::PowerAction`] wire discriminant; `0`
    /// and unknown values fail closed with [`Errno::OutOfRange`]).
    ///
    /// Gated flat on [`crate::CapabilityId::SYSTEM_POWER`] — unlike
    /// [`Self::SIGNAL`] there is no per-target tier to decide, because the
    /// target is the whole machine — and audited on every call, allowed or
    /// refused. The handler flushes **every** mounted volume before it asks
    /// the platform to stop (where [`Self::FS_SYNC`] flushes only the one
    /// filesystem behind a caller's handle), so a shutdown never abandons
    /// buffered writes.
    ///
    /// On success the platform stops and the call never returns. It returns
    /// only when the machine is still running: [`Errno::NotSupported`] when
    /// the port has no primitive for the requested transition, or the
    /// flush's own error when a device fault means the volumes are not
    /// durable — in both cases the caller reports the refusal rather than
    /// assuming the machine went down.
    pub const SYSTEM_POWER: Self = Self(105);

    /// Grant the serving task of one call endpoint the right to *call*
    /// another call endpoint the caller already holds — the endpoint half of
    /// grant delegation, and the exact sibling of [`Self::SHM_GRANT`]
    /// (`plans/FIX-IO.md` `IO6b`).
    ///
    /// A grant-restricted endpoint is reachable only by a task holding the
    /// per-endpoint [`crate::HwResource::endpoint`] grant, which until now
    /// could be acquired only by creating the endpoint or inheriting it from
    /// a matched hardware node at spawn. A process that must drive several
    /// devices as one — a RAID array composing its member disks — could
    /// therefore never be assembled, because no task can hold client
    /// authority over endpoints belonging to several matched nodes. This
    /// completes the delegation primitive rather than widening authority.
    ///
    /// Arguments: `endpoint` (the endpoint id the caller already holds a
    /// grant for) and `recipient` (the endpoint whose **live serving task**
    /// receives the delegated grant — never a caller-supplied, recyclable
    /// PID). Returns the minted grant handle, which the caller forwards
    /// in-band to the recipient; the handle resolves only when presented by
    /// the recipient task itself, so the number is useless to a bystander.
    ///
    /// Narrowing-only and fail closed: the caller's *own* grant is checked
    /// before any endpoint state is read, so a task can delegate only an
    /// endpoint it may already call. A grant the caller does not hold and an
    /// unknown recipient endpoint are the same [`Errno::NotFound`] with
    /// nothing minted, so the reply confirms nothing about foreign
    /// endpoints. Gated on [`crate::CapabilityId::IPC_ENDPOINT`] — the
    /// capability the endpoint resource itself declares, exactly as
    /// [`Self::SHM_GRANT`] is gated on the shared-region resource's
    /// `CAP_SHM` — and audited on every mint.
    pub const CALL_GRANT: Self = Self(106);

    /// Read the operator's one-boot login choice
    /// ([`crate::BootSession`]), as made in the pre-boot Supervisor with
    /// `continue text` / `continue gui`.
    ///
    /// No arguments. Returns the [`crate::BootSession`] discriminant; a
    /// boot the operator never diverted reports
    /// [`crate::BootSession::Unset`] and the stored `os.loginType` default
    /// decides. Recorded once by the boot path and immutable thereafter,
    /// so — like [`Self::BOOT_FACTS_GET`] — the value is public boot state
    /// rather than live state: it grants no authority, names no account,
    /// and reveals no secret, hence unprivileged and not audited.
    pub const BOOT_SESSION_GET: Self = Self(107);

    /// Inclusive upper bound on the syscall identifier space in `abi-v1`.
    pub const MAX: u16 = 1023;

    /// Wrap a raw value, validating that it falls inside the syscall table.
    ///
    /// Returns [`Errno::OutOfRange`] if `raw` exceeds [`SyscallNumber::MAX`].
    pub const fn from_raw(raw: u16) -> Result<Self, Errno> {
        if raw > Self::MAX {
            return Err(Errno::OutOfRange);
        }
        Ok(Self(raw))
    }

    /// Raw on-wire value.
    #[must_use]
    pub const fn as_u16(self) -> u16 {
        self.0
    }
}

/// Maximum length, in bytes, of the textual resource reference a
/// [`SyscallNumber::RESOURCE_OPEN`] call may pass.
///
/// The wire bound on the reference string the kernel copies in before it
/// hands the bytes to the single shared reference parser (`lib/resref`),
/// which enforces its own identical maximum (`tairix_resref::MAX_REF_LEN`) —
/// so a reference the ABI accepts always fits the parser's bound and the two
/// cannot drift. A longer reference fails closed with
/// [`Errno::LengthOutOfRange`] before any resolution work is done.
pub const RESOURCE_REF_MAX: usize = 1024;

/// The `pid` argument to [`SyscallNumber::WAIT`] that selects "any child".
///
/// Passing this rather than a specific PID waits for whichever of the
/// caller's children exits next (the POSIX `waitpid(-1, …)` convention).
/// A named constant keeps the sentinel from appearing as a bare `-1` at
/// every call site.
pub const WAIT_PID_ANY: i32 = -1;

/// Flags accepted by [`SyscallNumber::WAIT`].
///
/// A `#[repr(transparent)]` newtype over the `u32` flags register so the wire
/// representation is exactly the integer the syscall trampoline passes,
/// mirroring [`crate::MapFlags`]. Only the bits named here are defined; every
/// other bit is reserved and must be zero. [`WaitFlags::from_bits`] rejects a
/// value with any reserved bit set, so a future flag cannot be silently
/// ignored by an older kernel (validate every input, fail closed).
#[repr(transparent)]
#[derive(Copy, Clone, Debug, Eq, PartialEq, Hash, Default)]
pub struct WaitFlags(u32);

impl WaitFlags {
    /// Do not block: report the child's status immediately if one is
    /// reapable, otherwise return [`Errno::WouldBlock`] without parking the
    /// caller.
    ///
    /// This is the non-blocking *poll* the shell's job control uses to report
    /// finished background jobs before the next prompt: with the bit set,
    /// [`SyscallNumber::WAIT`] either reaps an already-exited child (returning
    /// its PID, exactly as the blocking form does) or — when a matching child
    /// exists but has not exited yet — returns [`Errno::WouldBlock`], the
    /// established `abi-v1` "nothing yet, retry" signal (the same one
    /// [`SyscallNumber::USERS_DB_READ`] and the wait-set use). A poll that
    /// finds no reapable child is not a security decision, so it is recorded
    /// below the error level and cannot flood the audit log the way a
    /// per-call error would. With the bit clear, `wait` blocks until a child
    /// becomes reapable (never busy-polls).
    pub const NONBLOCK: Self = Self(1 << 0);

    /// Also report a child *stopped* by [`crate::Signal::Stop`] — the
    /// `WUNTRACED` analogue the shell's job control uses (`plans/SPAWN.md`
    /// SP9).
    ///
    /// With the bit set, [`SyscallNumber::WAIT`] additionally completes for
    /// a child freshly stopped by [`crate::Signal::Stop`]: it returns the
    /// child's PID and writes a *stopped* [`crate::WaitStatusRecord`]
    /// through `status` — **without reaping the child**, which stays
    /// waitable and resumable through [`crate::Signal::Continue`]. Each
    /// stop is reported exactly once (edge-triggered); a `Continue` re-arms
    /// the report for a later stop. With the bit clear a stopped child is
    /// invisible to `wait`, exactly as before. Combines with
    /// [`Self::NONBLOCK`]: the poll then also reports a pending stop
    /// instead of [`crate::Errno::WouldBlock`].
    pub const STOPPED: Self = Self(1 << 1);

    /// The set of all defined flag bits.
    ///
    /// Any bit outside this mask is reserved and rejected by
    /// [`WaitFlags::from_bits`].
    const DEFINED_BITS: u32 = Self::NONBLOCK.0 | Self::STOPPED.0;

    /// An empty flag set (blocking wait, no options).
    #[must_use]
    pub const fn empty() -> Self {
        Self(0)
    }

    /// Raw flag bits, as carried on the ABI.
    #[must_use]
    pub const fn bits(self) -> u32 {
        self.0
    }

    /// Build a flag set from raw bits, rejecting any reserved bit.
    ///
    /// # Errors
    ///
    /// Returns [`Errno::OutOfRange`] if `bits` sets any reserved
    /// (currently-undefined) bit.
    pub const fn from_bits(bits: u32) -> Result<Self, Errno> {
        if bits & !Self::DEFINED_BITS != 0 {
            return Err(Errno::OutOfRange);
        }
        Ok(Self(bits))
    }

    /// Whether every bit set in `other` is also set in `self`.
    #[must_use]
    pub const fn contains(self, other: Self) -> bool {
        self.0 & other.0 == other.0
    }

    /// Whether the caller asked for a non-blocking poll.
    #[must_use]
    pub const fn is_nonblock(self) -> bool {
        self.contains(Self::NONBLOCK)
    }

    /// Whether the caller asked to be told about stopped children too.
    #[must_use]
    pub const fn is_stopped(self) -> bool {
        self.contains(Self::STOPPED)
    }
}

/// Opaque, kernel-issued handle to a bound hardware interrupt line.
///
/// Returned by the `irq_bind` syscall and consumed by `irq_wait`. The
/// inner `u64` is unforgeable in the sense that the kernel rejects any
/// `irq_wait` whose `handle` was not previously minted for the calling
/// task (capabilities are unforgeable tokens; —
/// no trusted-caller shortcuts). The wire representation is the raw
/// `u64`; the wrapper exists so call sites cannot confuse it with
/// arbitrary integer arguments.
#[repr(transparent)]
#[derive(Copy, Clone, Debug, Eq, PartialEq, Hash, Ord, PartialOrd)]
pub struct IrqHandle(u64);

impl IrqHandle {
    /// Reserved invalid value.
    ///
    /// The kernel must never mint this value; it is reserved so a
    /// caller-zeroed buffer cannot be mistaken for a live handle.
    pub const INVALID: Self = Self(0);

    /// Wrap a raw value as a handle.
    ///
    /// Reserved for the kernel's IRQ allocator. User-space code
    /// receives handles from `irq_bind`; constructing one by hand
    /// gains nothing because the kernel re-checks the handle against
    /// the caller's binding on every `irq_wait`.
    #[must_use]
    pub const fn from_raw(raw: u64) -> Self {
        Self(raw)
    }

    /// Raw on-wire value.
    #[must_use]
    pub const fn as_u64(self) -> u64 {
        self.0
    }
}

#[cfg(test)]
mod tests {
    use super::{IrqHandle, SyscallNumber, WaitFlags, SYSCALL_TABLE_HASH_LEN};
    use crate::Errno;

    #[test]
    fn well_known_numbers_are_frozen() {
        // Numeric assignments are part of abi-v1; do not renumber.
        assert_eq!(SyscallNumber::YIELD.as_u16(), 0);
        assert_eq!(SyscallNumber::EXIT.as_u16(), 1);
        assert_eq!(SyscallNumber::IPC_SEND.as_u16(), 2);
        assert_eq!(SyscallNumber::IPC_RECV.as_u16(), 3);
        assert_eq!(SyscallNumber::CAP_QUERY.as_u16(), 4);
        assert_eq!(SyscallNumber::CAP_DELEGATE.as_u16(), 5);
        assert_eq!(SyscallNumber::CAP_REVOKE.as_u16(), 6);
        assert_eq!(SyscallNumber::CLOCK_GET.as_u16(), 7);
        assert_eq!(SyscallNumber::IRQ_BIND.as_u16(), 8);
        assert_eq!(SyscallNumber::IRQ_WAIT.as_u16(), 9);
        assert_eq!(SyscallNumber::RANDOM_GET.as_u16(), 10);
        assert_eq!(SyscallNumber::STREAM_WRITE.as_u16(), 11);
        assert_eq!(SyscallNumber::SPAWN.as_u16(), 12);
        assert_eq!(SyscallNumber::STREAM_READ.as_u16(), 13);
        assert_eq!(SyscallNumber::MEM_MAP.as_u16(), 14);
        assert_eq!(SyscallNumber::MEM_UNMAP.as_u16(), 15);
        assert_eq!(SyscallNumber::WAIT.as_u16(), 16);
        assert_eq!(SyscallNumber::RLIMIT_GET.as_u16(), 17);
        assert_eq!(SyscallNumber::RLIMIT_SET.as_u16(), 18);
        assert_eq!(SyscallNumber::USERS_DB_READ.as_u16(), 19);
        assert_eq!(SyscallNumber::CONSOLE_COUNT.as_u16(), 20);
        assert_eq!(SyscallNumber::STREAM_INPUT_MODE.as_u16(), 21);
        assert_eq!(SyscallNumber::KEY_INJECT.as_u16(), 22);
        assert_eq!(SyscallNumber::DISPLAY_ACQUIRE.as_u16(), 23);
        assert_eq!(SyscallNumber::DISPLAY_RELEASE.as_u16(), 24);
        assert_eq!(SyscallNumber::KEYBOARD_READ.as_u16(), 25);
        assert_eq!(SyscallNumber::MMIO_MAP.as_u16(), 26);
        assert_eq!(SyscallNumber::DMA_ALLOC.as_u16(), 27);
        assert_eq!(SyscallNumber::RESOURCE_GRANTS.as_u16(), 28);
        assert_eq!(SyscallNumber::HW_TREE_READ.as_u16(), 29);
        assert_eq!(SyscallNumber::HW_TREE_WAIT.as_u16(), 30);
        assert_eq!(SyscallNumber::IPC_CALL.as_u16(), 31);
        assert_eq!(SyscallNumber::CALL_CREATE.as_u16(), 32);
        assert_eq!(SyscallNumber::CALL_RECV.as_u16(), 33);
        assert_eq!(SyscallNumber::CALL_REPLY.as_u16(), 34);
        assert_eq!(SyscallNumber::USERS_DB_WAIT.as_u16(), 35);
        assert_eq!(SyscallNumber::LOG_EMIT.as_u16(), 36);
        assert_eq!(SyscallNumber::HW_EMIT_NODE.as_u16(), 37);
        assert_eq!(SyscallNumber::HW_REMOVE_NODE.as_u16(), 38);
        assert_eq!(SyscallNumber::MSI_ALLOC.as_u16(), 39);
        assert_eq!(SyscallNumber::SHM_CREATE.as_u16(), 40);
        assert_eq!(SyscallNumber::SHM_MAP.as_u16(), 41);
        assert_eq!(SyscallNumber::SHM_UNMAP.as_u16(), 42);
        assert_eq!(SyscallNumber::WAITSET_CREATE.as_u16(), 43);
        assert_eq!(SyscallNumber::WAITSET_CTL.as_u16(), 44);
        assert_eq!(SyscallNumber::WAITSET_WAIT.as_u16(), 45);
        assert_eq!(SyscallNumber::FS_OPEN.as_u16(), 46);
        assert_eq!(SyscallNumber::FS_CLOSE.as_u16(), 47);
        assert_eq!(SyscallNumber::FS_READ.as_u16(), 48);
        assert_eq!(SyscallNumber::FS_WRITE.as_u16(), 49);
        assert_eq!(SyscallNumber::FS_READDIR.as_u16(), 50);
        assert_eq!(SyscallNumber::FS_STAT.as_u16(), 51);
        assert_eq!(SyscallNumber::FS_TRUNCATE.as_u16(), 52);
        assert_eq!(SyscallNumber::FS_SYNC.as_u16(), 53);
        assert_eq!(SyscallNumber::FS_MKDIR.as_u16(), 54);
        assert_eq!(SyscallNumber::FS_UNLINK.as_u16(), 55);
        assert_eq!(SyscallNumber::DMA_FREE.as_u16(), 56);
        assert_eq!(SyscallNumber::FS_RENAME.as_u16(), 57);
        assert_eq!(SyscallNumber::CALL_PEER_ORIGIN.as_u16(), 58);
        assert_eq!(SyscallNumber::WALL_TIME_GET.as_u16(), 59);
        assert_eq!(SyscallNumber::WALL_TIME_SET.as_u16(), 60);
        assert_eq!(SyscallNumber::BOOT_ID_GET.as_u16(), 61);
        assert_eq!(SyscallNumber::SYSINFO_INTROSPECT.as_u16(), 62);
        assert_eq!(SyscallNumber::TERMINAL_SIZE.as_u16(), 63);
        assert_eq!(SyscallNumber::SIGNAL.as_u16(), 64);
        assert_eq!(SyscallNumber::FS_CHDIR.as_u16(), 65);
        assert_eq!(SyscallNumber::FS_GETCWD.as_u16(), 66);
        assert_eq!(SyscallNumber::RESOURCE_OPEN.as_u16(), 67);
        assert_eq!(SyscallNumber::SELF_ORIGIN.as_u16(), 68);
        assert_eq!(SyscallNumber::USERS_ADMIN.as_u16(), 69);
        assert_eq!(SyscallNumber::PORT_BIND.as_u16(), 88);
        assert_eq!(SyscallNumber::BOOT_FACTS_GET.as_u16(), 89);
        assert_eq!(SyscallNumber::SCHED_SET_REALTIME.as_u16(), 95);
        assert_eq!(SyscallNumber::FS_SET_OWNER.as_u16(), 96);
        assert_eq!(SyscallNumber::PTY_CREATE.as_u16(), 97);
        assert_eq!(SyscallNumber::PTY_SET_SIZE.as_u16(), 98);
        assert_eq!(SyscallNumber::CALL_POST.as_u16(), 99);
        assert_eq!(SyscallNumber::CALL_REAP.as_u16(), 100);
        assert_eq!(SyscallNumber::CALL_CANCEL.as_u16(), 101);
    }

    #[test]
    fn irq_handle_round_trips_and_invalid_is_zero() {
        assert_eq!(IrqHandle::INVALID.as_u64(), 0);
        let h = IrqHandle::from_raw(0xDEAD_BEEF_CAFE_F00D);
        assert_eq!(h.as_u64(), 0xDEAD_BEEF_CAFE_F00D);
        assert_ne!(h, IrqHandle::INVALID);
    }

    #[test]
    fn from_raw_enforces_table_bounds() {
        assert_eq!(
            SyscallNumber::from_raw(SyscallNumber::MAX).map(SyscallNumber::as_u16),
            Ok(1023)
        );
        assert_eq!(
            SyscallNumber::from_raw(SyscallNumber::MAX + 1),
            Err(Errno::OutOfRange),
        );
    }

    #[test]
    fn hash_length_matches_sha256() {
        assert_eq!(SYSCALL_TABLE_HASH_LEN, 32);
    }

    #[test]
    fn wait_flags_empty_is_blocking() {
        let f = WaitFlags::empty();
        assert_eq!(f.bits(), 0);
        assert!(!f.is_nonblock());
    }

    #[test]
    fn wait_flags_nonblock_round_trips() {
        let f = WaitFlags::NONBLOCK;
        assert!(f.is_nonblock());
        let again = WaitFlags::from_bits(f.bits()).expect("defined bit");
        assert_eq!(again, f);
    }

    #[test]
    fn wait_flags_stopped_round_trips_and_combines() {
        let f = WaitFlags::STOPPED;
        assert!(f.is_stopped());
        assert!(!f.is_nonblock());
        assert_eq!(WaitFlags::from_bits(f.bits()), Ok(f));
        // The stop-report request combines with the non-blocking poll.
        let both = WaitFlags::from_bits(WaitFlags::NONBLOCK.bits() | WaitFlags::STOPPED.bits())
            .expect("defined bits");
        assert!(both.is_nonblock());
        assert!(both.is_stopped());
    }

    #[test]
    fn wait_flags_reserved_bits_are_rejected() {
        // Bit 2 is the first reserved bit today.
        assert_eq!(WaitFlags::from_bits(1 << 2), Err(Errno::OutOfRange));
        assert_eq!(WaitFlags::from_bits(u32::MAX), Err(Errno::OutOfRange));
    }
}
