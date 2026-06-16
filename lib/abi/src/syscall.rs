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
    /// Send a message to an IPC endpoint.
    pub const IPC_SEND: Self = Self(2);
    /// Receive a message from an IPC endpoint.
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
    /// against handle forgery (`AGENTS.md` §5.4).
    pub const IRQ_WAIT: Self = Self(9);
    /// Fill a user buffer with cryptographically secure random bytes.
    ///
    /// Arguments: `buf: *mut u8` (user pointer), `len: usize`,
    /// `flags: u32` ([`crate::RandomFlags`]). Returns the number of
    /// bytes written. The kernel draws from its CSPRNG-backed output
    /// reserve (`AGENTS.md` §22); the call is unprivileged (drawing
    /// randomness needs no capability), but a `len` above
    /// [`crate::RANDOM_REQUEST_MAX_BYTES`] is refused. With
    /// [`crate::RandomFlags::NON_BLOCKING`] set and the kernel RNG not
    /// yet seeded it returns [`crate::Errno::EntropyNotReady`] rather
    /// than blocking.
    pub const RANDOM_GET: Self = Self(10);
    /// Write a byte buffer to one of the calling process's inherited
    /// standard streams (`AGENTS.md` §20).
    ///
    /// Arguments: `fd: u32` (the standard descriptor — [`crate::STDOUT`],
    /// [`crate::STDERR`], or [`crate::STDINFO`]), `buf: *const u8` (user
    /// pointer), `len: usize`. Returns the number of bytes written. The
    /// kernel resolves `fd` against the caller's per-process descriptor
    /// table ([`crate::DescriptorTable`]) — the inherited descriptor, not
    /// an ambient device, is the authority (§20) — then copies the buffer
    /// through the validated `copy_from_user` boundary (`AGENTS.md` §5.4)
    /// and emits it to that descriptor's kernel stream backing. In the
    /// bootstrap session every backing is the discovered console (the
    /// detected framebuffer when present, else the first discovered UART,
    /// `plans/PI.md` P6), so use of a console-backed stream additionally
    /// requires [`crate::CapabilityId::CONSOLE_WRITE`]. An `fd` that is
    /// not a writable inherited stream fails closed; a build with no
    /// backing wired fails closed with [`crate::Errno::NotImplemented`].
    pub const STREAM_WRITE: Self = Self(11);
    /// Spawn a new process from an embedded program named by an absolute
    /// path (`plans/SPAWN.md` SP3, `AGENTS.md` §16.5).
    ///
    /// Arguments: `path: *const u8` (user pointer to the program's
    /// absolute path), `path_len: usize`, and `console: u64` — which
    /// system console the child's standard streams attach to
    /// (`AGENTS.md` §20 — the spawner, never the program, decides the
    /// backing). Passing [`CONSOLE_INHERIT`](crate::CONSOLE_INHERIT)
    /// attaches the child to the **caller's own** descriptor table
    /// (the default session shape: a child stays on its parent's
    /// console); any other value names a console index reported by
    /// [`SyscallNumber::CONSOLE_COUNT`] and an index with no installed
    /// console fails closed with [`crate::Errno::NotFound`]. The kernel
    /// copies the path in through the validated `copy_from_user`
    /// boundary (`AGENTS.md` §5.4), looks it up in the kernel's
    /// embedded-program registry, builds a fresh **hardware-isolated**
    /// address space for it (§4), registers it as a runnable process,
    /// and returns the new process's PID; the caller keeps running (a
    /// true concurrent spawn, not an `exec`-style hand-off). Gated by
    /// [`crate::CapabilityId::PROC_SPAWN`] — spawning materialises a new
    /// principal and hands it the CPU, so it is privileged rather than
    /// ambient (`AGENTS.md` §4). The spawned program receives only the
    /// intersection of its own signed manifest request and its user's
    /// grants (§16.5); spawn authority does not widen the child's
    /// authority. A build with no spawn service wired, or a path naming
    /// no registered program, fails closed (`AGENTS.md` §2.9).
    pub const SPAWN: Self = Self(12);
    /// Read a byte buffer from the calling process's inherited standard
    /// input (`AGENTS.md` §20).
    ///
    /// Arguments: `fd: u32` (the standard descriptor — normally
    /// [`crate::STDIN`]), `buf: *mut u8` (user pointer), `len: usize`.
    /// Returns the number of bytes read. The kernel resolves `fd` against
    /// the caller's per-process descriptor table ([`crate::DescriptorTable`])
    /// and reads from that descriptor's kernel stream backing — in the
    /// bootstrap session the first discovered keyboard/UART input source
    /// (`plans/PI.md` P6) — into a bounded kernel staging buffer, copying it
    /// out through the validated `copy_to_user` boundary (`AGENTS.md`
    /// §5.4). The input counterpart of [`SyscallNumber::STREAM_WRITE`]: a
    /// short read (fewer bytes than `len`, possibly zero when no input is
    /// pending) is valid, so the caller loops. Use of a console-backed
    /// stream additionally requires [`crate::CapabilityId::CONSOLE_READ`].
    /// An `fd` that is not a readable inherited stream fails closed; a
    /// build with no backing wired fails closed with
    /// [`crate::Errno::NotImplemented`].
    pub const STREAM_READ: Self = Self(13);
    /// Map a fresh anonymous `RW` region into the calling process's own
    /// address space (`plans/SPAWN.md` SP5).
    ///
    /// Arguments: `len: usize` (bytes, rounded up to whole pages),
    /// `flags: u32` ([`crate::MapFlags`]), `addr_hint: u64` (a page-aligned
    /// placement hint; `0` means "kernel chooses"). Returns the base address
    /// of the new region. The region is zeroed before it is visible, is
    /// always `RW` and never executable (`AGENTS.md` §19.2 — W^X), and is
    /// mapped only into the **caller's own** hardware-isolated address space
    /// (`AGENTS.md` §4 — no global user heap, no cross-process mapping). The
    /// call is unprivileged — growing one's own address space needs no
    /// capability (`AGENTS.md` §16.6 precedent) — but the kernel validates
    /// every argument and fails closed (`AGENTS.md` §5.4). A frame- or
    /// page-table-allocation failure returns [`crate::Errno::OutOfMemory`]
    /// rather than panicking (`AGENTS.md` §4 / §2.9); a build with no memory
    /// service wired fails closed with [`crate::Errno::NotImplemented`].
    pub const MEM_MAP: Self = Self(14);
    /// Release a region previously returned by [`SyscallNumber::MEM_MAP`]
    /// from the calling process's own address space (`plans/SPAWN.md` SP5).
    ///
    /// Arguments: `base: u64` (the region's base, as returned by `mem_map`)
    /// and `len: usize` (its length in bytes). The frames reclaimed are
    /// zeroed on free (`AGENTS.md` §4 — secret hygiene). Unmapping a region
    /// that was never mapped, or a partial/over-long range, fails closed
    /// (`AGENTS.md` §5.4); a build with no memory service wired fails closed
    /// with [`crate::Errno::NotImplemented`].
    pub const MEM_UNMAP: Self = Self(15);
    /// Wait for a child process to exit, reaping it and reporting its
    /// exit code (`plans/SPAWN.md` SP6).
    ///
    /// Arguments: `pid: i32` (the child to wait for, or [`WAIT_ANY`] to
    /// wait for any of the caller's children) and `status: *mut i32` (a
    /// non-null user pointer the kernel writes the reaped child's exit
    /// code into). Returns the reaped child's PID. A process may only wait
    /// on its **own** children — waiting reaps a child the caller spawned,
    /// so it grants no authority over anything else and needs no
    /// capability (`AGENTS.md` §16.6 precedent — "list my own processes");
    /// the kernel validates the parent/child relationship and fails closed
    /// (`AGENTS.md` §5.4). Waiting on a `pid` that is not a child of the
    /// caller fails closed with [`crate::Errno::NotFound`]; a build with no
    /// process-wait service wired fails closed with
    /// [`crate::Errno::NotImplemented`].
    pub const WAIT: Self = Self(16);
    /// Read the calling process's effective limit for one resource
    /// (`AGENTS.md` §24.3).
    ///
    /// Arguments: `kind: u32` (a [`crate::LimitKind`] discriminant) and
    /// `out: *mut ros_resource_limit_t` (a non-null user pointer the kernel
    /// writes the encoded [`crate::ResourceLimit`] into). Returns an error
    /// code (`Ok(0)` on success). Observing one's *own* effective limit is
    /// the unprivileged baseline — it grants no authority and needs no
    /// capability (`AGENTS.md` §16.6 precedent) — but the kernel validates
    /// `kind` and the pointer and fails closed (`AGENTS.md` §5.4). An
    /// unassigned `kind` fails with [`crate::Errno::OutOfRange`]; a build
    /// with no resource-limit service wired fails closed with
    /// [`crate::Errno::NotImplemented`].
    pub const RLIMIT_GET: Self = Self(17);
    /// Set the calling process's limit for one resource (`AGENTS.md` §24.3).
    ///
    /// Arguments: `kind: u32` (a [`crate::LimitKind`] discriminant) and
    /// `in: *const ros_resource_limit_t` (a non-null user pointer to the
    /// encoded [`crate::ResourceLimit`] to install). Returns an error code
    /// (`Ok(0)` on success). A process may freely *lower* a bound, but
    /// *raising* a hard bound — or setting any bound above the inherited
    /// ceiling — requires [`crate::CapabilityId::RLIMIT_RAISE`] (§24.3) and
    /// otherwise fails with [`crate::Errno::PermissionDenied`]. A malformed
    /// pair (`soft > hard`) or an unassigned `kind` fails closed with
    /// [`crate::Errno::OutOfRange`]; a build with no resource-limit service
    /// wired fails closed with [`crate::Errno::NotImplemented`].
    pub const RLIMIT_SET: Self = Self(18);
    /// Read the system user database (`/System/Security/Users`) the kernel
    /// loaded off the mounted root volume at boot (`AGENTS.md` §5.1,
    /// `plans/PI.md` P11).
    ///
    /// Arguments: `buf: *mut u8` (a non-null user pointer) and
    /// `len: usize` (the buffer's capacity). Returns the number of bytes
    /// copied — the database's exact `users-v1` text, which the caller
    /// parses with the same fail-closed `lib/users` parser the kernel
    /// used. Gated by [`crate::CapabilityId::USERS_READ`]: the text
    /// carries every account's salted password record, so only the
    /// authentication principal (login) may read it (`AGENTS.md` §4 — no
    /// ambient authority). A buffer smaller than the database fails
    /// closed with [`crate::Errno::BufferTooSmall`] — the kernel never
    /// truncates a credential database (`AGENTS.md` §2.9); sizing the
    /// buffer at the format's own 64 KiB maximum always suffices. A build
    /// holding no database — no root volume mounted, or the record was
    /// refused at boot — fails closed with [`crate::Errno::NotFound`], so
    /// a system without accounts refuses every login rather than
    /// inventing one (`AGENTS.md` §5.4.5).
    pub const USERS_DB_READ: Self = Self(19);
    /// Report how many system text consoles are installed
    /// (`AGENTS.md` §20, `plans/PI.md` P11).
    ///
    /// No arguments. Returns the number of console stream backings the
    /// boot path installed — each one an independent text console (the
    /// video console, a UART) a spawner may attach a child's standard
    /// streams to through [`SyscallNumber::SPAWN`]'s `console`
    /// argument. PID 1 `init` uses it to start one login session per
    /// discovered console (`plans/PI.md` P11 — the video console and
    /// the UART are separate session contexts). Gated by
    /// [`crate::CapabilityId::CONSOLE_WRITE`]: console topology belongs
    /// to the principals that drive consoles, not to every task
    /// (`AGENTS.md` §5.4).
    pub const CONSOLE_COUNT: Self = Self(20);
    /// Set whether one of the calling process's inherited input streams
    /// echoes the bytes it reads back to its console (`AGENTS.md` §20,
    /// `plans/PI.md` P11 — terminal local echo).
    ///
    /// Arguments: `fd: u32` (the input descriptor — normally
    /// [`crate::STDIN`]) and `enabled: u32` (`0` disables echo, any other
    /// value enables it). Returns an error code (`Ok(0)` on success).
    /// Echo is the line-discipline behaviour of the console *backing*:
    /// while it is on, every byte a [`SyscallNumber::STREAM_READ`] of `fd`
    /// consumes is written back to that descriptor's console output so an
    /// interactive user sees what they type. The kernel performs the echo
    /// itself (it owns the line discipline), so a reader needs no separate
    /// [`crate::CapabilityId::CONSOLE_WRITE`] for it; the call is the
    /// program's contract for suppressing echo — login disables it around
    /// a password read so the secret is never rendered, then restores it
    /// (`AGENTS.md` §5.4 — fail closed; never echo a credential). Console
    /// echo defaults to **on**. Gated by
    /// [`crate::CapabilityId::CONSOLE_READ`]: terminal echo belongs to the
    /// principal that reads the console, never to every task. An `fd` that
    /// is not a readable inherited stream fails closed with
    /// [`crate::Errno::NotFound`]; a build with no console wired fails
    /// closed with [`crate::Errno::NotImplemented`].
    pub const STREAM_ECHO: Self = Self(21);
    /// Inject one decoded keyboard *key edge* into the kernel input-focus
    /// arbiter (`AGENTS.md` §20, `plans/PI.md` P11 — input follows the
    /// surface owner).
    ///
    /// Arguments: `buf: *const u8` (one [`crate::input::KeyInput`] record)
    /// and `len: usize` (its length, [`crate::input::KeyInput::WIRE_LEN`]).
    /// Returns the number of bytes consumed, or a negative error code. The
    /// keyboard-input driver that decoded a directly attached keyboard
    /// (USB-HID / PS-2) emits the *device-resolved key edge* — a pressed or
    /// released [`crate::input::KeyValue`] plus the held
    /// [`crate::input::Modifiers`] — and the kernel arbiter decides both
    /// the **encoding** and the **destination** by who currently holds
    /// input focus (`plans/PI.md` P11): with the text console foreground it
    /// encodes the press to its console (tty) bytes through the shared
    /// `lib/keymap` map and enqueues them on the focused console's input
    /// queue (drained by a [`SyscallNumber::STREAM_READ`]); with the
    /// desktop (window manager) foreground it routes the whole record to
    /// the kernel keyboard channel (drained by
    /// [`SyscallNumber::KEYBOARD_READ`]). The driver no longer chooses the
    /// encoding or the destination — that policy left the device
    /// (`AGENTS.md` §17.4). Gated by
    /// [`crate::CapabilityId::INPUT_INJECT`]: feeding the system's keyboard
    /// stream is privileged, never ambient (`AGENTS.md` §4). A malformed
    /// record is refused fail-closed (`AGENTS.md` §5.4 / §2.9).
    pub const KEY_INJECT: Self = Self(22);
    /// Acquire ownership of the display and claim keyboard input focus
    /// (`AGENTS.md` §10, §17.3; `plans/PI.md` P11 — input follows the
    /// surface owner).
    ///
    /// No arguments. Returns an error code (`Ok(0)` on success). The
    /// compositing window manager calls this when it takes over the
    /// screen: the kernel input-focus arbiter switches its foreground from
    /// the text console to the desktop keyboard channel, so subsequently
    /// injected key edges ([`SyscallNumber::KEY_INJECT`]) are delivered as
    /// [`crate::input::KeyInput`] records the manager drains with
    /// [`SyscallNumber::KEYBOARD_READ`] — the same keyboard stream now
    /// following the new surface owner automatically (the desktop analogue
    /// of "input follows the foreground tty", `AGENTS.md` §20). Gated by
    /// [`crate::CapabilityId::DISPLAY`]: owning the display is privileged,
    /// never ambient (`AGENTS.md` §4).
    pub const DISPLAY_ACQUIRE: Self = Self(23);
    /// Release the display and return keyboard input focus to the text
    /// console (`AGENTS.md` §10, §17.3; `plans/PI.md` P11).
    ///
    /// No arguments. Returns an error code (`Ok(0)` on success). The
    /// inverse of [`SyscallNumber::DISPLAY_ACQUIRE`]: the window manager
    /// calls it when it relinquishes the screen, and the kernel input-focus
    /// arbiter returns its foreground to the text console so a login/shell
    /// once again receives the keyboard. Gated by
    /// [`crate::CapabilityId::DISPLAY`].
    pub const DISPLAY_RELEASE: Self = Self(24);
    /// Read one decoded keyboard event from the kernel keyboard channel
    /// (`AGENTS.md` §10; `plans/PI.md` P11 — keyboard input for the
    /// desktop).
    ///
    /// Arguments: `buf: *mut u8` (a buffer of at least
    /// [`crate::input::KeyInput::WIRE_LEN`] bytes) and `len: usize` (its
    /// length). Returns the number of bytes written — one
    /// [`crate::input::KeyInput`] record — or `0` when the channel is
    /// momentarily drained; a buffer too small to hold a record fails
    /// closed with [`crate::Errno::BufferTooSmall`] (`AGENTS.md` §2.9). The
    /// principal that owns the display (the window manager / desktop
    /// session) drains the records the arbiter routed to it while it held
    /// focus. Gated by [`crate::CapabilityId::INPUT_READ`]: a keyboard
    /// stream is delivered only to whoever currently owns the surface, and
    /// an unattached channel denies rather than leaking to a device
    /// (`AGENTS.md` §4, §5.4, §20).
    pub const KEYBOARD_READ: Self = Self(25);
    /// Map a granted device MMIO register window into the calling
    /// driver's own address space (`AGENTS.md` §4 / §18.3; `plans/PI.md`
    /// P10 chunk 5d-0 — the `DriverHost` MMIO/DMA surface reachable over
    /// IPC).
    ///
    /// Argument: `handle: u64` — an unforgeable, kernel-issued
    /// device-resource grant handle the driver received for the matched
    /// hardware-tree node it binds (one handle per [`crate::hwtree::HwResource`]
    /// the node requested). The kernel resolves the handle **against the
    /// calling task** (handle forgery is rejected exactly as
    /// [`SyscallNumber::IRQ_WAIT`] re-checks its binding, `AGENTS.md` §5.4),
    /// confirms the grant names a memory window
    /// ([`crate::hwtree::HwResourceKind::Mmio`] / `BusWindow`), and maps
    /// **only** that granted region — caching disabled — into the caller's
    /// own hardware-isolated address space, returning its base user virtual
    /// address. A driver can therefore never synthesise a pointer to
    /// arbitrary physical memory: it maps a region the kernel chose to
    /// grant it, and nothing more (§4 — no ambient authority). Gated by
    /// [`crate::CapabilityId::MMIO_MAP`]; an unknown or non-owned handle, a
    /// grant of the wrong kind, or a build with no map facility wired fails
    /// closed (`AGENTS.md` §2.9).
    pub const MMIO_MAP: Self = Self(26);

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

/// The `pid` argument to [`SyscallNumber::WAIT`] that selects "any child".
///
/// Passing this rather than a specific PID waits for whichever of the
/// caller's children exits next (the POSIX `waitpid(-1, …)` convention).
/// A named constant keeps the sentinel from appearing as a bare `-1` at
/// every call site (`AGENTS.md` §2.11).
pub const WAIT_ANY: i32 = -1;

/// Opaque, kernel-issued handle to a bound hardware interrupt line.
///
/// Returned by the `irq_bind` syscall and consumed by `irq_wait`. The
/// inner `u64` is unforgeable in the sense that the kernel rejects any
/// `irq_wait` whose `handle` was not previously minted for the calling
/// task (`AGENTS.md` §5.2 — capabilities are unforgeable tokens; §5.4 —
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
    use super::{IrqHandle, SyscallNumber, SYSCALL_TABLE_HASH_LEN};
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
        assert_eq!(SyscallNumber::STREAM_ECHO.as_u16(), 21);
        assert_eq!(SyscallNumber::KEY_INJECT.as_u16(), 22);
        assert_eq!(SyscallNumber::DISPLAY_ACQUIRE.as_u16(), 23);
        assert_eq!(SyscallNumber::DISPLAY_RELEASE.as_u16(), 24);
        assert_eq!(SyscallNumber::KEYBOARD_READ.as_u16(), 25);
        assert_eq!(SyscallNumber::MMIO_MAP.as_u16(), 26);
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
}
