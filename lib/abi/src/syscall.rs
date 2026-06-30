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
    /// path (`plans/SPAWN.md` SP3).
    ///
    /// Arguments: `path: *const u8` (user pointer to the program's
    /// absolute path), `path_len: usize`, and `console: u64` — which
    /// system console the child's standard streams attach to
    /// (the spawner, never the program, decides the
    /// backing). Passing [`CONSOLE_INHERIT`](crate::CONSOLE_INHERIT)
    /// attaches the child to the **caller's own** descriptor table
    /// (the default session shape: a child stays on its parent's
    /// console); any other value names a console index reported by
    /// [`SyscallNumber::CONSOLE_COUNT`] and an index with no installed
    /// console fails closed with [`crate::Errno::NotFound`]. The kernel
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
    /// wait for any of the caller's children) and `status: *mut i32` (a
    /// non-null user pointer the kernel writes the reaped child's exit
    /// code into). Returns the reaped child's PID. A process may only wait
    /// on its **own** children — waiting reaps a child the caller spawned,
    /// so it grants no authority over anything else and needs no
    /// capability (precedent — "list my own processes");
    /// the kernel validates the parent/child relationship and fails closed. Waiting on a `pid` that is not a child of the
    /// caller fails closed with [`crate::Errno::NotFound`]; a build with no
    /// process-wait service wired fails closed with
    /// [`crate::Errno::NotImplemented`].
    pub const WAIT: Self = Self(16);
    /// Read the calling process's effective limit for one resource.
    ///
    /// Arguments: `kind: u32` (a [`crate::LimitKind`] discriminant) and
    /// `out: *mut ros_resource_limit_t` (a non-null user pointer the kernel
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
    /// `in: *const ros_resource_limit_t` (a non-null user pointer to the
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
    /// video console, a UART) a spawner may attach a child's standard
    /// streams to through [`SyscallNumber::SPAWN`]'s `console`
    /// argument. PID 1 `init` uses it to start one login session per
    /// discovered console (`plans/PI.md` P11 — the video console and
    /// the UART are separate session contexts). Gated by
    /// [`crate::CapabilityId::CONSOLE_WRITE`]: console topology belongs
    /// to the principals that drive consoles, not to every task.
    pub const CONSOLE_COUNT: Self = Self(20);
    /// Set whether one of the calling process's inherited input streams
    /// echoes the bytes it reads back to its console (
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
    /// (fail closed; never echo a credential). Console
    /// echo defaults to **on**. Gated by
    /// [`crate::CapabilityId::CONSOLE_READ`]: terminal echo belongs to the
    /// principal that reads the console, never to every task. An `fd` that
    /// is not a readable inherited stream fails closed with
    /// [`crate::Errno::NotFound`]; a build with no console wired fails
    /// closed with [`crate::Errno::NotImplemented`].
    pub const STREAM_ECHO: Self = Self(21);
    /// Inject one decoded keyboard *key edge* into the kernel input-focus
    /// arbiter (`plans/PI.md` P11 — input follows the
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
    /// encoding or the destination — that policy left the device. Gated by
    /// [`crate::CapabilityId::INPUT_INJECT`]: feeding the system's keyboard
    /// stream is privileged, never ambient. A malformed
    /// record is refused fail-closed.
    pub const KEY_INJECT: Self = Self(22);
    /// Acquire ownership of the display and claim keyboard input focus
    /// (`plans/PI.md` P11 — input follows the
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
    /// of "input follows the foreground tty"). Gated by
    /// [`crate::CapabilityId::DISPLAY`]: owning the display is privileged,
    /// never ambient.
    pub const DISPLAY_ACQUIRE: Self = Self(23);
    /// Release the display and return keyboard input focus to the text
    /// console (`plans/PI.md` P11).
    ///
    /// No arguments. Returns an error code (`Ok(0)` on success). The
    /// inverse of [`SyscallNumber::DISPLAY_ACQUIRE`]: the window manager
    /// calls it when it relinquishes the screen, and the kernel input-focus
    /// arbiter returns its foreground to the text console so a login/shell
    /// once again receives the keyboard. Gated by
    /// [`crate::CapabilityId::DISPLAY`].
    pub const DISPLAY_RELEASE: Self = Self(24);
    /// Read one decoded keyboard event from the kernel keyboard channel
    /// (`plans/PI.md` P11 — keyboard input for the
    /// desktop).
    ///
    /// Arguments: `buf: *mut u8` (a buffer of at least
    /// [`crate::input::KeyInput::WIRE_LEN`] bytes) and `len: usize` (its
    /// length). Returns the number of bytes written — one
    /// [`crate::input::KeyInput`] record — or `0` when the channel is
    /// momentarily drained; a buffer too small to hold a record fails
    /// closed with [`crate::Errno::BufferTooSmall`]. The
    /// principal that owns the display (the window manager / desktop
    /// session) drains the records the arbiter routed to it while it held
    /// focus. Gated by [`crate::CapabilityId::INPUT_READ`]: a keyboard
    /// stream is delivered only to whoever currently owns the surface, and
    /// an unattached channel denies rather than leaking to a device.
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
    /// / [`SyscallNumber::WAIT`]), never busy-spinning. A
    /// reply larger than `reply_cap` fails closed with
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
    /// Argument: `node_id: u64` — the [`crate::HwNode::id`] of the node to
    /// remove. Returns `0`, or `-errno`.
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
    /// [`crate::WaitSourceKind`] (`Endpoint` / `Irq`); `id: u64` — the
    /// resource the member names (an IPC call-endpoint id the caller serves,
    /// or an [`IrqHandle`] the caller bound); `token: u64` — an opaque,
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
    /// directory-into-its-own-subtree move, a read-only mount, a
    /// cross-mount move, or a denied parent fails closed. Gated by
    /// [`crate::CapabilityId::FS_ACCESS`].
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
/// every call site.
pub const WAIT_PID_ANY: i32 = -1;

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
