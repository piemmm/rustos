//! Generated `abi-v1` dispatch table.
//!
//! Every syscall entering the kernel — from any architecture port —
//! lands in [`Dispatcher::dispatch`]. The dispatcher performs the
//! five steps mandated by and forwards the call to
//! the owning subsystem via the [`SyscallHandlers`] trait. The trait
//! is implemented in `kernel/core`'s wiring layer so this crate stays
//! decoupled from `kernel/ipc`, `kernel/sched`, and friends
//! (no bloat).

use rustos_abi::{
    spec_for, AbiType, CapabilityId, Errno, IrqHandle, MapFlags, OpenFlags, RandomFlags,
    SyscallNumber, SyscallSpec, ENCODED_TABLE, PROC_ID_HEX_LEN, SYSCALL_MAX_ARGS,
};
use rustos_crypto::{sha256, Sha256Digest};
use rustos_kernel_sec::{TaskCapabilities, TaskId};
use rustos_log::{Field, Sink};
use rustos_util::fmt::{format_hex_u64, format_i32};

use crate::audit::{record, AuditEvent};

/// SHA-256 fingerprint of [`rustos_abi::ENCODED_TABLE`].
///
/// The value is **derived at build time** by this crate's `build.rs`
/// from `rustos_abi::ENCODED_TABLE` — the single source of truth — and `include!`d here. There is no
/// hand-maintained literal to edit or to let drift out of sync with the
/// table it fingerprints: changing the syscall table re-derives this
/// value on the next build. The kernel still re-checks it via
/// [`verify_table_hash`] at the syscall-registration phase of
/// `kernel_main`, and `cargo xtask abi-check` cross-checks the linked
/// value against a freshly computed digest; refusal to boot beats
/// silently dispatching against an ABI the user space never agreed to.
pub const SYSCALL_TABLE_HASH: Sha256Digest =
    include!(concat!(env!("OUT_DIR"), "/syscall_table_hash.rs"));

/// Re-compute the SHA-256 of [`rustos_abi::ENCODED_TABLE`] and compare it
/// to [`SYSCALL_TABLE_HASH`].
///
/// # Errors
///
/// Returns [`Errno::AbiVersionUnsupported`] when the two diverge, which
/// can only happen if the dependency graph contains a `rustos-abi`
/// older or newer than the one this crate was built against.
pub fn verify_table_hash() -> Result<(), Errno> {
    if sha256(&ENCODED_TABLE) == SYSCALL_TABLE_HASH {
        Ok(())
    } else {
        Err(Errno::AbiVersionUnsupported)
    }
}

/// Register-passed argument tuple delivered to [`Dispatcher::dispatch`].
///
/// Architecture entry stubs build this from the caller's argument
/// registers (Stage 3). Unused slots must be zero — the dispatcher
/// rejects anything else with [`Errno::LengthOutOfRange`] so a buggy
/// trampoline cannot smuggle data past a syscall's declared
/// `arg_count`.
#[repr(transparent)]
#[derive(Copy, Clone, Debug, Eq, PartialEq)]
pub struct RawArgs(pub [u64; SYSCALL_MAX_ARGS]);

// SAFETY-INVARIANT: `RawArgs` is `#[repr(transparent)]` over
// `[u64; SYSCALL_MAX_ARGS]`. The x86_64 syscall trampoline
// (`kernel/arch/x86_64::syscall_entry`) builds the argument frame as a
// raw `[u64; SYSCALL_MAX_ARGS]` on the kernel stack and the binding
// kernel binary reinterprets that frame as a `RawArgs` via the public
// tuple-struct constructor `RawArgs(arr)`. Locking the layout here
// keeps that bridge sound under future field additions: any change
// that breaks size, alignment, or representation will fail the build
// at the call site rather than silently desync the ABI. (no interface creep) — this is a compile-time invariant
// assertion, not a new public surface.
const _RAW_ARGS_LAYOUT_MATCHES_ARRAY: () = {
    assert!(core::mem::size_of::<RawArgs>() == core::mem::size_of::<[u64; SYSCALL_MAX_ARGS]>());
    assert!(core::mem::align_of::<RawArgs>() == core::mem::align_of::<[u64; SYSCALL_MAX_ARGS]>());
};

impl RawArgs {
    /// All-zero argument tuple. Used by argument-less syscalls and as
    /// the default in tests.
    pub const ZERO: Self = Self([0; SYSCALL_MAX_ARGS]);

    /// Borrow the underlying register array.
    #[must_use]
    pub const fn as_array(&self) -> &[u64; SYSCALL_MAX_ARGS] {
        &self.0
    }
}

/// Identification of the task that issued a syscall.
///
/// The dispatcher never trusts caller-supplied identity — `task_id`
/// and `caps` are produced by the per-CPU current-task slot owned by
/// `kernel/sched`. The architecture stub passes both through verbatim;
/// the dispatcher uses them for capability checking and audit
/// attribution only.
pub struct CallerContext<'a> {
    /// Identifier carried in audit records.
    pub task_id: TaskId,
    /// Effective capability set, already intersected with the user
    /// grant and manifest request (see `kernel/sec`).
    pub caps: &'a TaskCapabilities,
}

/// Return type of a single dispatched syscall.
///
/// `Ok(value)` is the unsigned return register; the architecture stub
/// maps it back onto the ABI return type declared in
/// [`SyscallSpec::ret`]. `Err(errno)` is delivered to user space as a
/// negative integer in the standard ABI form.
pub type SyscallResult = Result<u64, Errno>;

/// Pluggable subsystem hooks.
///
/// One implementation per kernel build — `kernel/core` provides the
/// production wiring; tests substitute a mock. Each method receives
/// the [`CallerContext`] and the already-validated arguments, never
/// the raw [`RawArgs`].
///
/// Methods return a [`SyscallResult`]; the dispatcher converts an
/// `Err` into the [`AuditEvent::SyscallHandlerRejected`] audit record
/// for security-relevant calls before returning the same `Err` to the
/// caller.
pub trait SyscallHandlers {
    /// Voluntarily yield the CPU.
    fn yield_now(&self, caller: &CallerContext<'_>) -> SyscallResult;
    /// Terminate the calling process with `code`.
    fn exit(&self, caller: &CallerContext<'_>, code: i32) -> SyscallResult;
    /// Send a message to an IPC endpoint.
    fn ipc_send(
        &self,
        caller: &CallerContext<'_>,
        endpoint: u64,
        ptr: u64,
        len: usize,
    ) -> SyscallResult;
    /// Receive a message from an IPC endpoint.
    fn ipc_recv(
        &self,
        caller: &CallerContext<'_>,
        endpoint: u64,
        ptr: u64,
        len: usize,
    ) -> SyscallResult;
    /// Query whether the caller holds `cap`.
    fn cap_query(&self, caller: &CallerContext<'_>, cap: CapabilityId) -> SyscallResult;
    /// Delegate a capability set to another task.
    fn cap_delegate(&self, caller: &CallerContext<'_>, target: u64, set_ptr: u64) -> SyscallResult;
    /// Revoke `cap` from the task identified by `target`.
    fn cap_revoke(
        &self,
        caller: &CallerContext<'_>,
        target: u64,
        cap: CapabilityId,
    ) -> SyscallResult;
    /// Read the monotonic clock (nanoseconds since boot).
    fn clock_get(&self, caller: &CallerContext<'_>) -> SyscallResult;
    /// Bind the calling task to a hardware interrupt line.
    ///
    /// `line` is the architecture-defined IRQ identifier. The
    /// implementation returns the freshly minted [`IrqHandle`] as a
    /// `u64` in [`SyscallResult::Ok`]; subsequent `irq_wait` calls
    /// must present that handle. The implementation is responsible
    /// for refusing duplicate bindings, refusing lines outside the
    /// platform's allowable range, and recording the binding against
    /// the calling task so the handle cannot be forged.
    fn irq_bind(&self, caller: &CallerContext<'_>, line: u32) -> SyscallResult;
    /// Block the calling task until `handle` fires, with timeout.
    ///
    /// On wake-up the implementation returns `Ok(0)`. On timeout
    /// expiry it returns `Err(Errno::TimedOut)`. The implementation
    /// must re-check that `handle` was minted for the calling task
    /// before performing any state transition and must mask the
    /// underlying line at the controller before resuming the waiter,
    /// so the same edge cannot stampede the driver
    /// (`docs/src/security/irq.md`).
    fn irq_wait(
        &self,
        caller: &CallerContext<'_>,
        handle: IrqHandle,
        timeout_ns: u64,
    ) -> SyscallResult;
    /// Fill the user buffer at `buf` with up to `len` cryptographically
    /// secure random bytes drawn from the kernel output reserve, returning the number of bytes written.
    ///
    /// The dispatcher has already validated that `buf` is non-null, that
    /// `len` fits in `usize`, and that `flags` carries no reserved bit.
    /// The implementation must refuse a `len` above
    /// [`rustos_abi::RANDOM_REQUEST_MAX_BYTES`] with
    /// [`Errno::LengthOutOfRange`], and — when `flags` requests
    /// non-blocking behaviour and the RNG is not yet seeded — return
    /// [`Errno::EntropyNotReady`] rather than blocking.
    fn random_get(
        &self,
        caller: &CallerContext<'_>,
        buf: u64,
        len: usize,
        flags: RandomFlags,
    ) -> SyscallResult;
    /// Write the `len` bytes at user pointer `buf` to the calling
    /// process's standard stream `fd`, returning the number of bytes
    /// written.
    ///
    /// The dispatcher has already checked the caller holds
    /// [`CapabilityId::CONSOLE_WRITE`], that `buf` is non-null, that `fd`
    /// fits in `u32`, and that `len` fits in `usize`. The implementation
    /// resolves `fd` against the caller's per-process descriptor table
    /// (`rustos_abi::DescriptorTable`): an `fd` that is not a writable
    /// inherited stream fails closed (the
    /// descriptor, not an ambient device, is the authority). It then
    /// copies the buffer through the validated `copy_from_user` boundary and emits it to that descriptor's kernel stream
    /// backing — in the bootstrap session the discovered console (the
    /// detected framebuffer when present, else the first discovered UART,
    /// `plans/PI.md` P6). A build with no backing wired must fail closed
    /// with [`Errno::NotImplemented`] rather than silently discarding the
    /// bytes.
    fn stream_write(
        &self,
        caller: &CallerContext<'_>,
        fd: u32,
        buf: u64,
        len: usize,
    ) -> SyscallResult;
    /// Spawn a new process from the embedded program named by the
    /// absolute path `(path, path_len)`, returning the new process's PID.
    ///
    /// The dispatcher has already checked the caller holds
    /// [`CapabilityId::PROC_SPAWN`], that `path` is non-null, and that
    /// `path_len` fits in `usize`. The implementation copies the path in
    /// through the validated `copy_from_user` boundary, looks it up in the kernel's embedded-program registry,
    /// builds a fresh hardware-isolated address space for it,
    /// registers it as a runnable process, and returns its PID; the
    /// caller keeps running (`plans/SPAWN.md` SP3 — a true concurrent
    /// spawn, not an `exec`-style hand-off). `console` selects the
    /// child's standard-stream attachment:
    /// [`rustos_abi::CONSOLE_INHERIT`] attaches the child to the
    /// caller's own descriptor table, any other value names an
    /// installed console index and the implementation must fail closed
    /// with [`Errno::NotFound`] when no console is installed at it. A
    /// build with no spawn service wired must fail closed with
    /// [`Errno::NotImplemented`], and a path naming no registered
    /// program with [`Errno::NotFound`], rather than silently doing
    /// nothing.
    fn spawn(
        &self,
        caller: &CallerContext<'_>,
        path: u64,
        path_len: usize,
        console: u64,
    ) -> SyscallResult;
    /// Read up to `len` bytes from the calling process's standard stream
    /// `fd` into the user buffer at `buf`, returning the number of bytes
    /// read.
    ///
    /// The dispatcher has already checked the caller holds
    /// [`CapabilityId::CONSOLE_READ`], that `buf` is non-null, that `fd`
    /// fits in `u32`, and that `len` fits in `usize`. The implementation
    /// resolves `fd` against the caller's per-process descriptor table
    /// (`rustos_abi::DescriptorTable`): an `fd` that is not a readable
    /// inherited stream fails closed. It then
    /// reads from that descriptor's kernel stream backing — in the
    /// bootstrap session the first discovered keyboard/UART input source
    /// (`plans/PI.md` P6) — into a bounded kernel staging buffer and
    /// copies it out through the validated `copy_to_user` boundary. A short read (fewer bytes than `len`, possibly
    /// zero when no input is pending) is valid, so the caller loops. A
    /// build with no backing wired must fail closed with
    /// [`Errno::NotImplemented`] rather than fabricating input.
    fn stream_read(
        &self,
        caller: &CallerContext<'_>,
        fd: u32,
        buf: u64,
        len: usize,
    ) -> SyscallResult;
    /// Map `len` bytes of fresh anonymous `RW` memory into the calling
    /// process's own address space, returning the base address of the new
    /// region (`plans/SPAWN.md` SP5).
    ///
    /// The dispatcher has already validated that `len` fits in `usize`,
    /// that `flags` carries no reserved bit, and that `addr_hint` is a
    /// well-formed `u64`. The implementation maps the region only into the
    /// caller's **own** hardware-isolated address space (no global user heap, no cross-process mapping), zeroes it before it
    /// is visible, and never makes it executable (W^X).
    /// A frame- or page-table-allocation failure must return
    /// [`Errno::OutOfMemory`] rather than panicking;
    /// a build with no memory service wired must fail closed with
    /// [`Errno::NotImplemented`]. A zero `len` is rejected with
    /// [`Errno::LengthOutOfRange`].
    fn mem_map(
        &self,
        caller: &CallerContext<'_>,
        len: usize,
        flags: MapFlags,
        addr_hint: u64,
    ) -> SyscallResult;
    /// Release the region of `len` bytes based at `base` previously returned
    /// by [`SyscallHandlers::mem_map`] from the calling process's own
    /// address space (`plans/SPAWN.md` SP5).
    ///
    /// The dispatcher has already validated that `base` is a well-formed
    /// `u64` and that `len` fits in `usize`. The implementation zeroes the
    /// frames it reclaims (secret hygiene) and fails closed
    /// when `(base, len)` does not name a region the caller mapped. A build with no memory service wired must fail
    /// closed with [`Errno::NotImplemented`]; a zero `len` is rejected with
    /// [`Errno::LengthOutOfRange`]. Returns `Ok(0)` on success.
    fn mem_unmap(&self, caller: &CallerContext<'_>, base: u64, len: usize) -> SyscallResult;
    /// Wait for a child of the calling process to exit, reaping it and
    /// writing its exit code to the user `status` pointer; returns the
    /// reaped child's PID (`plans/SPAWN.md` SP6).
    ///
    /// The dispatcher has already validated that `pid` is a sign-extended
    /// `i32` and that `status` is a non-null `UserPtr`. `pid` is either a
    /// specific child's PID or [`rustos_abi::WAIT_PID_ANY`] (wait for any
    /// child). The implementation validates the parent/child relationship —
    /// a process may only reap its **own** children
    /// — blocks the caller until a child is reapable, and copies the exit
    /// code out through the validated `copy_to_user` boundary. A `pid` that
    /// is not a child of the caller must fail closed with
    /// [`Errno::NotFound`]; a build with no process-wait service wired must
    /// fail closed with [`Errno::NotImplemented`] rather than fabricating a
    /// reaped child.
    fn wait(&self, caller: &CallerContext<'_>, pid: i32, status: u64) -> SyscallResult;

    /// Read the calling task's effective limit for resource `kind`, writing
    /// the encoded [`rustos_abi::ResourceLimit`] to the user `out` pointer.
    ///
    /// The dispatcher has already validated that `kind` fits in a `u32`
    /// (upper bits zero) and that `out` is a non-null `UserPtr`. The
    /// implementation validates `kind` against [`rustos_abi::LimitKind`] and
    /// fails closed on an unassigned value (validate every
    /// input). Returns `Ok(0)` on success.
    ///
    /// The default implementation fails closed with [`Errno::NotImplemented`]: a kernel build with no resource-limit service
    /// wired never fabricates a limit. The enforcement is installed in
    /// `kernel/core`.
    fn rlimit_get(&self, _caller: &CallerContext<'_>, _kind: u32, _out: u64) -> SyscallResult {
        Err(Errno::NotImplemented)
    }

    /// Install the calling task's limit for resource `kind` from the encoded
    /// [`rustos_abi::ResourceLimit`] at the user `value` pointer.
    ///
    /// The dispatcher has already validated that `kind` fits in a `u32` and
    /// that `value` is a non-null `UserPtr`. The implementation copies the
    /// limit in through the validated `copy_from_user` boundary, validates
    /// `kind` and the soft/hard pair, and — when the request would *raise* a
    /// hard bound above the inherited ceiling — refuses with
    /// [`Errno::PermissionDenied`] unless the caller holds
    /// [`rustos_abi::CapabilityId::RLIMIT_RAISE`]. Returns `Ok(0)` on
    /// success.
    ///
    /// The default implementation fails closed with [`Errno::NotImplemented`]; the enforcement is installed in `kernel/core`.
    fn rlimit_set(&self, _caller: &CallerContext<'_>, _kind: u32, _value: u64) -> SyscallResult {
        Err(Errno::NotImplemented)
    }

    /// Copy the system user database (`/System/Security/Users`) the kernel
    /// loaded at boot out to the user buffer at `buf` (
    /// `plans/PI.md` P11).
    ///
    /// The dispatcher has already checked
    /// [`rustos_abi::CapabilityId::USERS_READ`] and that `buf` is a
    /// non-null `UserPtr`. The implementation bounds `len`, copies the
    /// database's exact `users-v1` text through the validated
    /// `copy_to_user` boundary, and returns the byte
    /// count. A buffer smaller than the database must fail closed with
    /// [`Errno::BufferTooSmall`] — a credential database is never
    /// truncated; a kernel holding no database must
    /// fail closed with [`Errno::NotFound`].
    ///
    /// The default implementation fails closed with
    /// [`Errno::NotImplemented`]: a kernel build with
    /// no users-database service wired never fabricates accounts. The
    /// service is installed in `kernel/core`.
    fn users_db_read(&self, _caller: &CallerContext<'_>, _buf: u64, _len: usize) -> SyscallResult {
        Err(Errno::NotImplemented)
    }

    /// Report how many system text consoles are installed (`plans/PI.md` P11).
    ///
    /// The dispatcher has already checked the caller holds
    /// [`CapabilityId::CONSOLE_WRITE`]. The implementation returns the
    /// length of the boot-installed console list — the index space the
    /// `spawn` syscall's `console` argument selects from. PID 1 `init`
    /// uses it to start one login session per discovered console.
    ///
    /// The default implementation fails closed with
    /// [`Errno::NotImplemented`]: a kernel build with
    /// no console list wired never fabricates a console topology. The
    /// real count is installed in `kernel/core`.
    fn console_count(&self, _caller: &CallerContext<'_>) -> SyscallResult {
        Err(Errno::NotImplemented)
    }

    /// Set whether one of the calling process's inherited input streams
    /// echoes the bytes it reads back to its console (
    /// `plans/PI.md` P11 — terminal local echo).
    ///
    /// The dispatcher has already checked the caller holds
    /// [`CapabilityId::CONSOLE_READ`]. `fd` must be a readable inherited
    /// stream and `enabled` is the ABI's `0`-disables/non-zero-enables
    /// flag. The implementation toggles the resolved console's echo flag;
    /// login disables echo around a password read so the secret is never
    /// rendered, then restores it (never echo a
    /// credential).
    ///
    /// The default implementation fails closed with
    /// [`Errno::NotImplemented`]: a kernel build with
    /// no console list wired has no echo to toggle. The real handler is
    /// installed in `kernel/core`.
    fn stream_echo(&self, _caller: &CallerContext<'_>, _fd: u32, _enabled: u32) -> SyscallResult {
        Err(Errno::NotImplemented)
    }

    /// Inject one decoded keyboard *key edge* into the kernel input-focus
    /// arbiter (`plans/PI.md` P11 — input follows the
    /// surface owner).
    ///
    /// The dispatcher has already checked the caller holds
    /// [`CapabilityId::INPUT_INJECT`] and that `buf` is a non-null
    /// `UserPtr`. The implementation copies up to `len` bytes in through
    /// the validated `copy_from_user` boundary, decodes
    /// one [`rustos_abi::input::KeyInput`] record fail-closed, and hands it
    /// to the arbiter, which decides the encoding and destination by who
    /// holds focus: with the text console foreground it encodes the press
    /// to console (tty) bytes and enqueues them on the focused console's
    /// input queue; with the desktop foreground it routes the record to the
    /// kernel keyboard channel. The driver no longer chooses the encoding
    /// or destination. Returns the number of bytes
    /// consumed from the record.
    ///
    /// The default implementation fails closed with
    /// [`Errno::NotImplemented`]: a kernel build with no
    /// input-focus arbiter wired has nowhere to route the edge. The real
    /// handler is installed in `kernel/core`.
    fn key_inject(&self, _caller: &CallerContext<'_>, _buf: u64, _len: usize) -> SyscallResult {
        Err(Errno::NotImplemented)
    }

    /// Acquire ownership of the display and claim keyboard input focus
    /// (`plans/PI.md` P11 — input follows the
    /// surface owner).
    ///
    /// The dispatcher has already checked the caller holds
    /// [`CapabilityId::DISPLAY`]. The implementation switches the
    /// input-focus arbiter's foreground to the desktop keyboard channel, so
    /// subsequently injected key edges ([`Self::key_inject`]) are delivered
    /// as records the display owner drains with [`Self::keyboard_read`].
    ///
    /// The default implementation fails closed with
    /// [`Errno::NotImplemented`]: a build with no
    /// arbiter wired owns no display to acquire. The real handler is
    /// installed in `kernel/core`.
    fn display_acquire(&self, _caller: &CallerContext<'_>) -> SyscallResult {
        Err(Errno::NotImplemented)
    }

    /// Release the display and return keyboard input focus to the text
    /// console (`plans/PI.md` P11).
    ///
    /// The inverse of [`Self::display_acquire`]; the dispatcher has already
    /// checked the caller holds [`CapabilityId::DISPLAY`]. The default
    /// implementation fails closed with [`Errno::NotImplemented`].
    fn display_release(&self, _caller: &CallerContext<'_>) -> SyscallResult {
        Err(Errno::NotImplemented)
    }

    /// Read one decoded keyboard event from the kernel keyboard channel
    /// (`plans/PI.md` P11 — keyboard input for the
    /// desktop).
    ///
    /// The dispatcher has already checked the caller holds
    /// [`CapabilityId::INPUT_READ`] and that `buf` is a non-null `UserPtr`.
    /// The implementation drains one [`rustos_abi::input::KeyInput`] record
    /// the arbiter routed to the channel into `buf` (at least
    /// [`rustos_abi::input::KeyInput::WIRE_LEN`] bytes), copies it out
    /// through the validated boundary, and returns the
    /// number of bytes written — or `0` when the channel is drained.
    ///
    /// The default implementation fails closed with
    /// [`Errno::NotImplemented`]: a build with no
    /// arbiter wired has no channel to drain. The real handler is installed
    /// in `kernel/core`.
    fn keyboard_read(&self, _caller: &CallerContext<'_>, _buf: u64, _len: usize) -> SyscallResult {
        Err(Errno::NotImplemented)
    }

    /// Map a granted device MMIO register window into the calling driver's
    /// own address space (`plans/PI.md` P10 chunk
    /// 5d-0).
    ///
    /// The dispatcher has already checked the caller holds
    /// [`CapabilityId::MMIO_MAP`]. `handle` is an unforgeable, kernel-issued
    /// device-resource grant the driver received for the hardware-tree node
    /// it binds, and `[offset, offset + len)` names the sub-region *within*
    /// that grant to map; the implementation resolves the handle **against
    /// the calling task** (rejecting forgery exactly as `irq_wait` re-checks
    /// its binding), confirms the grant names a memory
    /// window and the sub-region lies wholly inside it, maps only that
    /// sub-region — caching disabled — into the caller's own address space,
    /// and returns its base user virtual address. A driver therefore never
    /// reaches physical memory the kernel did not grant it (no ambient
    /// authority), and a driver granted a large outbound bus aperture maps
    /// just the single BAR it enumerated rather than the whole window.
    ///
    /// The default implementation fails closed with
    /// [`Errno::NotImplemented`]: a build with neither a
    /// grant table nor a map facility wired has nothing to map. The real
    /// handler is installed in `kernel/core`.
    fn mmio_map(
        &self,
        _caller: &CallerContext<'_>,
        _handle: u64,
        _offset: u64,
        _len: usize,
    ) -> SyscallResult {
        Err(Errno::NotImplemented)
    }

    /// Allocate a DMA-coherent buffer for the calling driver, bounded by a
    /// granted device DMA constraint (`plans/PI.md`
    /// P10 chunk 5d-0).
    ///
    /// The dispatcher has already checked the caller holds
    /// [`CapabilityId::MEM_DMA`]. `handle` is an unforgeable, kernel-issued
    /// device-resource grant the driver received for the hardware-tree node
    /// it binds; the implementation resolves it **against the calling task**
    /// (rejecting forgery exactly as [`Self::mmio_map`]),
    /// confirms the grant names a DMA constraint, carves a physically
    /// contiguous, zeroed, coherent region of `len` bytes whose physical
    /// extent lies within the grant's addressing limit,
    /// maps it `RW`, non-executable, into the caller's own address space,
    /// writes the buffer's device-visible base to the user pointer
    /// `device_out`, and returns its base user virtual address. A driver
    /// therefore reaches no memory the kernel did not grant it (no
    /// ambient authority).
    ///
    /// The default implementation fails closed with
    /// [`Errno::NotImplemented`]: a build with neither a
    /// grant table nor a DMA facility wired has nothing to allocate. The
    /// real handler is installed in `kernel/core`.
    fn dma_alloc(
        &self,
        _caller: &CallerContext<'_>,
        _handle: u64,
        _len: usize,
        _device_out: u64,
    ) -> SyscallResult {
        Err(Errno::NotImplemented)
    }

    /// Release a DMA-coherent buffer previously carved by [`Self::dma_alloc`]
    /// — the symmetric free for the device-DMA allocator (`plans/PI.md` P10).
    ///
    /// The dispatcher has already checked the caller holds
    /// [`CapabilityId::MEM_DMA`] (symmetric with [`Self::dma_alloc`]).
    /// `handle` is the same unforgeable grant the carve used; the
    /// implementation resolves it **against the calling task** (rejecting
    /// forgery exactly as [`Self::dma_alloc`]), confirms it names a DMA
    /// constraint, and releases the buffer whose CPU virtual base is
    /// `cpu_va` from the caller's own address space, zeroing every backing
    /// byte (zero-on-free) before its frames return to the allocator. Only
    /// `cpu_va` is taken from the caller; a `cpu_va` that is not the base of a
    /// live carve in *this task's* DMA window fails closed (covering a stale,
    /// double, or cross-task free) without releasing anything. A long-running
    /// driver reclaims each request's bounce buffers through this rather than
    /// leaking DMA frames until it exits.
    ///
    /// The default implementation fails closed with
    /// [`Errno::NotImplemented`]: a build with neither a grant table nor a
    /// DMA facility wired has nothing to free. The real handler is installed
    /// in `kernel/core`.
    fn dma_free(&self, _caller: &CallerContext<'_>, _handle: u64, _cpu_va: u64) -> SyscallResult {
        Err(Errno::NotImplemented)
    }

    /// Enumerate the device-resource grants the kernel minted for the
    /// calling driver task, delivering its unforgeable handles
    /// (`plans/PI.md` P10 chunk 5d-2).
    ///
    /// The dispatcher has already checked `buf` is a non-null `UserPtr`;
    /// the call needs no capability (a task reads only its *own* grants,
    /// which confers no authority — the own-process-observer
    /// baseline). The implementation serialises the calling task's grant
    /// set as consecutive [`rustos_abi::hwtree::GrantedResource`] records,
    /// copies them out through the validated boundary,
    /// and returns the total byte count — `0` for a task with no grants. A
    /// buffer too small for the whole set fails closed with
    /// [`Errno::BufferTooSmall`] rather than delivering a partial list.
    ///
    /// The default implementation fails closed with
    /// [`Errno::NotImplemented`]: a build with no grant
    /// table wired has nothing to enumerate. The real handler is installed
    /// in `kernel/core`.
    fn resource_grants(
        &self,
        _caller: &CallerContext<'_>,
        _buf: u64,
        _len: usize,
    ) -> SyscallResult {
        Err(Errno::NotImplemented)
    }

    /// Copy the discovered hardware tree out to the calling task — the
    /// read-only System Information API hardware view.
    ///
    /// The dispatcher has already checked the caller holds
    /// [`CapabilityId::SYSINFO_HW`] and that `buf` is a non-null `UserPtr`.
    /// The implementation serialises the store's current snapshot as a
    /// [`rustos_abi::hwtree::HwTreeHeader`] (generation + node count)
    /// followed by that many [`rustos_abi::hwtree::HwNode`] records, copies
    /// them out through the validated boundary, and
    /// returns the total byte count. A buffer too small for the whole
    /// snapshot fails closed with [`Errno::BufferTooSmall`] rather than
    /// truncating the inventory.
    ///
    /// The default implementation fails closed with
    /// [`Errno::NotImplemented`]: a build with no
    /// hardware-tree source wired has no tree to read. The real handler is
    /// installed in `kernel/core`.
    fn hw_tree_read(&self, _caller: &CallerContext<'_>, _buf: u64, _len: usize) -> SyscallResult {
        Err(Errno::NotImplemented)
    }

    /// Block the calling task until the hardware tree changes past
    /// `last_generation` (reactive re-match and
    /// hotplug).
    ///
    /// The dispatcher has already checked the caller holds
    /// [`CapabilityId::SYSINFO_HW`]. The implementation returns `Ok(0)` once
    /// the store's generation differs from `last_generation` — a node was
    /// seeded, appended, or removed — and [`Errno::TimedOut`] if
    /// `timeout_ns` elapses first, blocking cooperatively in between (the
    /// same shape as [`Self::irq_wait`] / [`Self::wait`]), never
    /// busy-spinning.
    ///
    /// The default implementation fails closed with
    /// [`Errno::NotImplemented`]: a build with no
    /// hardware-tree source wired has nothing to wait on. The real handler
    /// is installed in `kernel/core`.
    fn hw_tree_wait(
        &self,
        _caller: &CallerContext<'_>,
        _last_generation: u64,
        _timeout_ns: u64,
    ) -> SyscallResult {
        Err(Errno::NotImplemented)
    }

    /// Block the calling task until the system user database leaves its
    /// pending (still-being-unlocked) state (`plans/PI.md`
    /// P11 — the reactive companion to [`Self::users_db_read`]).
    ///
    /// The dispatcher has already checked the caller holds
    /// [`CapabilityId::USERS_READ`]. The implementation returns `Ok(0)` once
    /// the database is no longer pending — the unlock installed one, or gave
    /// up with none — and [`Errno::TimedOut`] if `timeout_ns` elapses first,
    /// blocking cooperatively in between (the same shape as
    /// [`Self::hw_tree_wait`]), never busy-spinning. It is
    /// what replaces `login` re-reading [`Self::users_db_read`] in a yield
    /// loop, which audited one ERROR per poll.
    ///
    /// The default implementation returns `Ok(0)` immediately: a build with
    /// no users-database service wired is never pending, so the caller's
    /// subsequent [`Self::users_db_read`] fails closed with
    /// [`Errno::NotImplemented`]. The real handler is
    /// installed in `kernel/core`.
    fn users_db_wait(&self, _caller: &CallerContext<'_>, _timeout_ns: u64) -> SyscallResult {
        Ok(0)
    }

    /// Make a synchronous capability-checked call to a kernel-owned IPC call
    /// endpoint: post the request, block until the reply arrives, and copy it
    /// out (Design D D2b).
    ///
    /// The dispatcher has already checked `request` and `reply` are non-null
    /// `UserPtr`s. The implementation resolves `endpoint` against the kernel
    /// call-endpoint registry, enforces the endpoint's required send
    /// capability against the **caller's** effective set before posting
    /// (no ambient authority), copies the request in and
    /// the reply out through the validated boundary, and blocks the caller
    /// cooperatively until the reply arrives (the same park shape as
    /// [`Self::hw_tree_wait`] / [`Self::wait`]), never busy-spinning. It returns the number of reply bytes written, or
    /// fails closed: [`Errno::BufferTooSmall`] if the reply exceeds
    /// `reply_cap`, [`Errno::PermissionDenied`] without the send capability,
    /// [`Errno::NotFound`] for an unknown or destroyed endpoint.
    ///
    /// The default implementation fails closed with
    /// [`Errno::NotImplemented`]: a build with no
    /// call-endpoint registry wired has nothing to call. The real handler is
    /// installed in `kernel/core`.
    fn ipc_call(
        &self,
        _caller: &CallerContext<'_>,
        _endpoint: u64,
        _request: u64,
        _request_len: usize,
        _reply: u64,
        _reply_cap: usize,
    ) -> SyscallResult {
        Err(Errno::NotImplemented)
    }

    /// Create and register a kernel-owned synchronous call endpoint the
    /// calling task then serves (Design D D3 — the
    /// server half of [`Self::ipc_call`]).
    ///
    /// The dispatcher has already checked `send_caps` and `recv_caps` are
    /// non-null `UserPtr`s. The implementation copies both `CapabilitySet`
    /// wire images in through the validated
    /// boundary, builds the endpoint with the caller as creator (the
    /// bind-time `CAP_IPC_BIND_PRIVILEGED` check for a restricted sender runs
    /// inside the endpoint constructor), and binds it under
    /// `endpoint` — failing closed with [`Errno::AlreadyExists`] if the id is
    /// live. Returns `Ok(0)` on success.
    ///
    /// The default implementation fails closed with
    /// [`Errno::NotImplemented`]; the real handler is
    /// installed in `kernel/core`.
    // Six `abi-v1` arguments plus the kernel-trusted caller context: the
    // shape is the syscall's own (endpoint id, two `CapabilitySet` pointers,
    // and the three payload/capacity bounds), not an accidental parameter
    // pile, so the count is intrinsic (justified allow).
    #[allow(clippy::too_many_arguments)]
    fn call_create(
        &self,
        _caller: &CallerContext<'_>,
        _endpoint: u64,
        _send_caps: u64,
        _recv_caps: u64,
        _max_request: usize,
        _max_reply: usize,
        _capacity: usize,
    ) -> SyscallResult {
        Err(Errno::NotImplemented)
    }

    /// Receive the next request posted to a call endpoint the calling task
    /// owns, blocking until one arrives (Design D D3 — the
    /// server-side receive half).
    ///
    /// The dispatcher has already checked `buf` and `ticket_out` are non-null
    /// `UserPtr`s. The implementation resolves `endpoint`, enforces the
    /// endpoint's required **receive** capability against the caller before
    /// touching state, and either copies one request out
    /// (returning its byte length and writing its ticket to `ticket_out`) or
    /// blocks cooperatively until one is posted (never busy-spinning). A request larger than `buf_cap` fails closed with
    /// [`Errno::BufferTooSmall`] and is left queued.
    ///
    /// The default implementation fails closed with
    /// [`Errno::NotImplemented`]; the real handler is installed in
    /// `kernel/core`.
    fn call_recv(
        &self,
        _caller: &CallerContext<'_>,
        _endpoint: u64,
        _buf: u64,
        _buf_cap: usize,
        _ticket_out: u64,
    ) -> SyscallResult {
        Err(Errno::NotImplemented)
    }

    /// Answer one received call on an endpoint the calling task owns, waking
    /// the blocked caller (Design D D3 — the server-side
    /// reply half).
    ///
    /// The dispatcher has already checked `reply` is a non-null `UserPtr`.
    /// The implementation resolves `endpoint`, enforces the endpoint's
    /// required **receive** capability against the caller, copies the reply
    /// in through the validated boundary, and completes `ticket` (waking the
    /// caller blocked in [`Self::ipc_call`]). A reply larger than the
    /// endpoint's `max_reply`, an unknown or already-answered ticket, or an
    /// unknown endpoint each fail closed. Returns
    /// `Ok(0)` on success.
    ///
    /// The default implementation fails closed with
    /// [`Errno::NotImplemented`]; the real handler is installed in
    /// `kernel/core`.
    fn call_reply(
        &self,
        _caller: &CallerContext<'_>,
        _endpoint: u64,
        _ticket: u64,
        _reply: u64,
        _reply_len: usize,
    ) -> SyscallResult {
        Err(Errno::NotImplemented)
    }

    /// Read the kernel-attested [`rustos_abi::Origin`] of the caller whose
    /// in-service call this server is currently handling (P-C — the
    /// server-side identity attestation half).
    ///
    /// The dispatcher has already checked `origin` is a non-null `UserPtr`.
    /// The implementation resolves `endpoint`, enforces the endpoint's
    /// required **receive** capability against the caller and confirms it is
    /// the owning task — both before touching state — then looks up the
    /// attested origin captured for `ticket` when the call was posted and
    /// copies its wire image out (returning its byte length). A foreign
    /// endpoint, an unknown or not-in-service ticket, or a buffer shorter
    /// than [`rustos_abi::ORIGIN_WIRE_LEN`] fails closed; the origin is never
    /// caller-supplied, so it cannot be forged.
    ///
    /// The default implementation fails closed with
    /// [`Errno::NotImplemented`]; the real handler is installed in
    /// `kernel/core`.
    fn call_peer_origin(
        &self,
        _caller: &CallerContext<'_>,
        _endpoint: u64,
        _ticket: u64,
        _origin: u64,
        _origin_cap: usize,
    ) -> SyscallResult {
        Err(Errno::NotImplemented)
    }

    /// Read the kernel wall-clock time and its provenance state (P-D).
    ///
    /// The dispatcher has already checked `out` is a non-null `UserPtr`; the
    /// call is unprivileged (like `clock_get`). The implementation reads the
    /// monotonic clock on the issuing CPU, projects the stored wall instant
    /// forward by the elapsed monotonic time, and copies the
    /// [`rustos_abi::WallClockReading`] (a [`rustos_abi::Time64`] plus a
    /// [`rustos_abi::WallTimeState`] byte) out, returning its byte length. A
    /// buffer shorter than [`rustos_abi::WallClockReading::WIRE_LEN`] fails
    /// closed. Before a trusted source has set it the reading is the Unix
    /// epoch tagged `Unset`.
    ///
    /// The default implementation fails closed with
    /// [`Errno::NotImplemented`]; the real handler is installed in
    /// `kernel/core`.
    fn wall_time_get(
        &self,
        _caller: &CallerContext<'_>,
        _out: u64,
        _out_cap: usize,
    ) -> SyscallResult {
        Err(Errno::NotImplemented)
    }

    /// Set the kernel wall-clock time from a trusted source (P-D).
    ///
    /// The dispatcher has already checked the caller holds
    /// [`CapabilityId::TIME_SET`] and that `time` is a non-null `UserPtr`.
    /// The implementation validates `state` is a settable
    /// [`rustos_abi::WallTimeState`] (rejecting `Unset` and any undefined
    /// discriminant), copies in a [`rustos_abi::Time64`] through the
    /// validated boundary, rejects a non-canonical instant, and records the
    /// new wall offset and state — leaving the monotonic clock untouched.
    /// Returns `Ok(0)`; a malformed instant, a short buffer, or a
    /// non-settable state fails closed.
    ///
    /// The default implementation fails closed with
    /// [`Errno::NotImplemented`]; the real handler is installed in
    /// `kernel/core`.
    fn wall_time_set(
        &self,
        _caller: &CallerContext<'_>,
        _time: u64,
        _time_len: usize,
        _state: u32,
    ) -> SyscallResult {
        Err(Errno::NotImplemented)
    }

    /// Read the kernel's per-boot identifier (P-E).
    ///
    /// The dispatcher has already checked `out` is a non-null `UserPtr`; the
    /// call is unprivileged (the boot id is a public per-boot nonce, not a
    /// secret). The implementation copies the 16-byte
    /// [`rustos_abi::BootId`] minted for this boot out to `out` and returns
    /// its byte length. A buffer shorter than [`rustos_abi::BOOT_ID_LEN`]
    /// fails closed, as does a boot whose random subsystem could not be
    /// seeded in time (the kernel reports `EntropyNotReady` rather than the
    /// all-zero [`rustos_abi::BootId::UNSET`] sentinel).
    ///
    /// The default implementation fails closed with
    /// [`Errno::NotImplemented`]; the real handler is installed in
    /// `kernel/core`.
    fn boot_id_get(
        &self,
        _caller: &CallerContext<'_>,
        _out: u64,
        _out_cap: usize,
    ) -> SyscallResult {
        Err(Errno::NotImplemented)
    }

    /// Emit a structured diagnostic record to the kernel's diagnostic log
    /// sink.
    ///
    /// The dispatcher has already checked the caller holds
    /// [`CapabilityId::LOG_EMIT`] and that `record` is a non-null `UserPtr`.
    /// The implementation copies in at most [`rustos_abi::LOG_RECORD_MAX`]
    /// bytes through the validated boundary, fully validates the record with
    /// [`rustos_abi::decode_log_record`], and emits it
    /// through the kernel's **diagnostic** `log_sink` — never the
    /// hash-chained security audit log — attributing it
    /// to the calling task (the caller cannot forge that attribution).
    /// Returns `Ok(0)` once accepted; a malformed record fails closed.
    ///
    /// The default implementation fails closed with
    /// [`Errno::NotImplemented`]; the real handler is installed in
    /// `kernel/core`.
    fn log_emit(&self, _caller: &CallerContext<'_>, _record: u64, _len: usize) -> SyscallResult {
        Err(Errno::NotImplemented)
    }

    /// Publish a discovered child device node into the live hardware tree
    /// (recursive, user-space hardware discovery).
    ///
    /// The dispatcher has already checked the caller holds
    /// [`CapabilityId::HW_EMIT`] and that `node` is a non-null `UserPtr`. The
    /// implementation copies in at most [`rustos_abi::hwtree::HwNode::WIRE_LEN`]
    /// bytes through the validated boundary, fully decodes the node with the
    /// fail-closed [`rustos_abi::HwNode::from_bytes`] parser, and admits it **only** when every
    /// [`rustos_abi::hwtree::HwResource`] it requests is wholly contained
    /// within a device-resource grant the calling task already holds — so an
    /// emitted child can never carry more authority than its emitter
    /// (no ambient authority;). On success it appends
    /// the node to the live hardware tree, bumping the generation that wakes
    /// the device manager's reactive autoload. A malformed node, an unknown
    /// parent, or an out-of-grant resource fails closed.
    /// Returns `Ok(0)` once published.
    ///
    /// The default implementation fails closed with
    /// [`Errno::NotImplemented`]; the real handler is installed in
    /// `kernel/core`.
    fn hw_emit_node(&self, _caller: &CallerContext<'_>, _node: u64, _len: usize) -> SyscallResult {
        Err(Errno::NotImplemented)
    }

    /// Remove a previously-published child device node — and its subtree —
    /// from the live hardware tree (hotplug removal).
    ///
    /// The dispatcher has already checked the caller holds
    /// [`CapabilityId::HW_EMIT`] (the same privilege as
    /// [`Self::hw_emit_node`]). The implementation resolves the caller's
    /// *own* matched node and removes `node_id` **only** when its parent is
    /// that node — a child the caller itself published — together with every
    /// transitive descendant, so a driver can never retire a node it does not
    /// own and no stale descendant outlives its parent (no
    /// ambient authority). On success it bumps the hardware-tree generation,
    /// waking the device manager's reactive watch so it unloads the driver
    /// bound to the vanished node — the symmetric counterpart of
    /// [`Self::hw_emit_node`]. An unknown id, or a node the caller does not
    /// own, fails closed. Returns `Ok(0)` once removed.
    ///
    /// The default implementation fails closed with
    /// [`Errno::NotImplemented`]; the real handler is installed in
    /// `kernel/core`.
    fn hw_remove_node(&self, _caller: &CallerContext<'_>, _node_id: u64) -> SyscallResult {
        Err(Errno::NotImplemented)
    }

    /// Allocate a message-signalled interrupt vector and report the
    /// architecture-built MSI doorbell for a PCI function.
    ///
    /// The dispatcher has already checked the caller holds
    /// [`CapabilityId::IRQ_BIND`] and that `out` is a non-null `UserPtr`.
    /// The implementation allocates a free MSI vector, brings the
    /// platform's MSI controller up if it is not already, **grants the
    /// calling task a device resource for the resulting virtual interrupt
    /// line** (so it may both `irq_bind` it and forward it as an
    /// [`rustos_abi::hwtree::HwResource::irq`] onto a child node), and writes
    /// the encoded [`rustos_abi::MsiAllocation`] into the caller's `out`
    /// buffer through the validated boundary, returning the number of bytes
    /// written. A platform with no MSI controller fails closed with
    /// [`Errno::NotImplemented`]; an exhausted vector space fails closed with
    /// [`Errno::OutOfRange`]; a buffer shorter than
    /// [`rustos_abi::MsiAllocation::WIRE_LEN`] fails closed.
    ///
    /// The default implementation fails closed with
    /// [`Errno::NotImplemented`]; the real handler is installed in
    /// `kernel/core`.
    fn msi_alloc(&self, _caller: &CallerContext<'_>, _out: u64, _out_len: usize) -> SyscallResult {
        Err(Errno::NotImplemented)
    }

    /// Create a cross-process shared-memory region the caller owns and maps,
    /// and may then grant to another task (`plans/USB.md`).
    ///
    /// The dispatcher has already checked the caller holds
    /// [`CapabilityId::SHM`] and that `id_out` is a non-null `UserPtr`. The
    /// implementation allocates a physically-contiguous, **zeroed** block of
    /// RAM the kernel owns, maps it into the caller's own address space,
    /// records the region against the caller as its owner, **grants the
    /// calling task a device resource for the region** (so it may forward it
    /// onto a child node it publishes), writes the kernel-minted region id to
    /// `id_out` through the validated boundary, and returns the base user
    /// virtual address. A zero length, frame exhaustion, or a build with no
    /// facility wired fails closed.
    ///
    /// The default implementation fails closed with
    /// [`Errno::NotImplemented`]; the real handler is installed in
    /// `kernel/core`.
    fn shm_create(&self, _caller: &CallerContext<'_>, _len: usize, _id_out: u64) -> SyscallResult {
        Err(Errno::NotImplemented)
    }

    /// Map a shared-memory region the kernel has granted the calling task
    /// into its own address space (`plans/USB.md`).
    ///
    /// The dispatcher has already checked the caller holds
    /// [`CapabilityId::SHM`]. `handle` is an unforgeable, kernel-issued
    /// device-resource grant the driver received for the hardware-tree node
    /// it binds; the implementation resolves it **against the calling task**
    /// (rejecting forgery exactly as [`Self::mmio_map`]), confirms the grant
    /// names a shared region, maps that region's existing kernel-owned frames
    /// into the caller's own address space, accounts the mapping so the
    /// frames are not freed while the caller still maps them, and returns its
    /// base user virtual address. An unknown or non-owned handle, a grant of
    /// the wrong kind, or a torn-down region fails closed.
    ///
    /// The default implementation fails closed with
    /// [`Errno::NotImplemented`]; the real handler is installed in
    /// `kernel/core`.
    fn shm_map(&self, _caller: &CallerContext<'_>, _handle: u64) -> SyscallResult {
        Err(Errno::NotImplemented)
    }

    /// Release a shared-memory mapping the calling task established with
    /// [`Self::shm_create`] or [`Self::shm_map`] (`plans/USB.md`).
    ///
    /// The call needs no capability (it releases only the caller's own
    /// mapping, the [`SyscallNumber::MEM_UNMAP`] posture). The implementation
    /// validates `(base, len)` names a live shared mapping of the caller,
    /// tears down only that mapping's page-table entries, and drops the
    /// caller's reference to the region; the region's frames are zeroed and
    /// freed at its last reference. A `(base, len)` that does not name a live
    /// shared mapping fails closed with [`Errno::NotFound`].
    ///
    /// The default implementation fails closed with
    /// [`Errno::NotImplemented`]; the real handler is installed in
    /// `kernel/core`.
    fn shm_unmap(&self, _caller: &CallerContext<'_>, _base: u64, _len: usize) -> SyscallResult {
        Err(Errno::NotImplemented)
    }

    /// Create a caller-owned wait-set that multiplexes the readiness of
    /// several event sources, returning its kernel-minted handle
    /// (`plans/USB.md`).
    ///
    /// Needs no capability (the dispatcher gates nothing): the set observes
    /// only resources the caller already holds, each owner-checked when it is
    /// added. The implementation mints a fresh handle and records an empty set
    /// owned by the calling task.
    ///
    /// The default implementation fails closed with
    /// [`Errno::NotImplemented`]; the real handler is installed in
    /// `kernel/core`.
    fn waitset_create(&self, _caller: &CallerContext<'_>) -> SyscallResult {
        Err(Errno::NotImplemented)
    }

    /// Add or remove a member of a wait-set (`plans/USB.md`).
    ///
    /// `set` is the wait-set handle; `op` is a [`rustos_abi::WaitSetOp`];
    /// `kind` is a [`rustos_abi::WaitSourceKind`]; `id` names the resource (an
    /// IPC call-endpoint id or an [`rustos_abi::IrqHandle`] raw value); `token`
    /// is the caller's opaque tag. On `Add` the implementation **resolves and
    /// owner-checks the named resource against the calling task** before
    /// recording it (no ambient authority), and owner-checks the set itself; a
    /// resource the caller does not own, a handle that is not the caller's own
    /// wait-set, an unknown `op`/`kind`, or a duplicate/absent member fails
    /// closed.
    ///
    /// The default implementation fails closed with
    /// [`Errno::NotImplemented`]; the real handler is installed in
    /// `kernel/core`.
    fn waitset_ctl(
        &self,
        _caller: &CallerContext<'_>,
        _set: u64,
        _op: u32,
        _kind: u32,
        _id: u64,
        _token: u64,
    ) -> SyscallResult {
        Err(Errno::NotImplemented)
    }

    /// Block until any one member of a wait-set is ready, writing the ready
    /// member's caller-chosen token to `token_out` (`plans/USB.md`).
    ///
    /// `set` is the wait-set handle; `timeout_ns` is a relative timeout
    /// ([`u64::MAX`] = no timeout); `token_out` is the non-null user pointer the
    /// token is written to. The implementation owner-checks the set, parks the
    /// caller off the run queue between readiness checks (woken by an IPC post
    /// to a member endpoint, a member IRQ firing, or the timeout — never a busy
    /// spin), re-checks each member against the caller as it scans, and on a
    /// ready member writes its token and returns `0`. A timeout returns
    /// [`Errno::TimedOut`]; a handle that is not the caller's own wait-set, or a
    /// faulting `token_out`, fails closed.
    ///
    /// The default implementation fails closed with
    /// [`Errno::NotImplemented`]; the real handler is installed in
    /// `kernel/core`.
    fn waitset_wait(
        &self,
        _caller: &CallerContext<'_>,
        _set: u64,
        _timeout_ns: u64,
        _token_out: u64,
    ) -> SyscallResult {
        Err(Errno::NotImplemented)
    }

    /// Open the file or directory at the absolute path `path` (`path_len`
    /// bytes) with `flags`, returning a new per-process file descriptor
    /// (`PREREQUISITES.md` P-A).
    ///
    /// The dispatcher has already checked the caller holds
    /// [`CapabilityId::FS_ACCESS`] and that `path` is a non-null `UserPtr`.
    /// The implementation copies the path in through the validated
    /// `copy_from_user` boundary, resolves it against the mounted secured
    /// VFS under the caller's kernel-attested `Credentials`, allocates a
    /// descriptor in the caller's per-process table, and returns it. Every
    /// per-inode and mount-flag check stays kernel-side; any failure fails
    /// closed.
    ///
    /// The default implementation fails closed with
    /// [`Errno::NotImplemented`]: a kernel build with no filesystem service
    /// wired never fabricates a handle. The real handler is installed in
    /// `kernel/core`.
    fn fs_open(
        &self,
        _caller: &CallerContext<'_>,
        _path: u64,
        _path_len: usize,
        _flags: OpenFlags,
    ) -> SyscallResult {
        Err(Errno::NotImplemented)
    }

    /// Release the open file descriptor `fd` from the caller's per-process
    /// table (`PREREQUISITES.md` P-A).
    ///
    /// The default implementation fails closed with [`Errno::NotImplemented`].
    fn fs_close(&self, _caller: &CallerContext<'_>, _fd: u32) -> SyscallResult {
        Err(Errno::NotImplemented)
    }

    /// Read up to `len` bytes from open file `fd` at `offset` into the user
    /// buffer `buf`, returning the number read (`PREREQUISITES.md` P-A).
    ///
    /// The dispatcher has already validated `buf` is a non-null `UserPtr`.
    /// The implementation resolves `fd` against the caller's table,
    /// re-authorises the read through the secured VFS, and copies the bytes
    /// out through the validated `copy_to_user` boundary.
    ///
    /// The default implementation fails closed with [`Errno::NotImplemented`].
    fn fs_read(
        &self,
        _caller: &CallerContext<'_>,
        _fd: u32,
        _offset: u64,
        _buf: u64,
        _len: usize,
    ) -> SyscallResult {
        Err(Errno::NotImplemented)
    }

    /// Write up to `len` bytes from the user buffer `buf` to open file `fd`
    /// at `offset` (ignored for an append handle), returning the number
    /// written (`PREREQUISITES.md` P-A).
    ///
    /// The default implementation fails closed with [`Errno::NotImplemented`].
    fn fs_write(
        &self,
        _caller: &CallerContext<'_>,
        _fd: u32,
        _offset: u64,
        _buf: u64,
        _len: usize,
    ) -> SyscallResult {
        Err(Errno::NotImplemented)
    }

    /// List open directory `fd` into the user buffer `buf`, returning the
    /// number of bytes written as a packed [`rustos_abi::DirEntry`] stream
    /// (`PREREQUISITES.md` P-A).
    ///
    /// The default implementation fails closed with [`Errno::NotImplemented`].
    fn fs_readdir(
        &self,
        _caller: &CallerContext<'_>,
        _fd: u32,
        _buf: u64,
        _len: usize,
    ) -> SyscallResult {
        Err(Errno::NotImplemented)
    }

    /// Write the [`rustos_abi::FileStat`] of open handle `fd` to the user
    /// buffer `out`, returning the number of bytes written
    /// (`PREREQUISITES.md` P-A).
    ///
    /// The default implementation fails closed with [`Errno::NotImplemented`].
    fn fs_stat(
        &self,
        _caller: &CallerContext<'_>,
        _fd: u32,
        _out: u64,
        _out_len: usize,
    ) -> SyscallResult {
        Err(Errno::NotImplemented)
    }

    /// Set the length of open file `fd` to `size` (`PREREQUISITES.md` P-A).
    ///
    /// The default implementation fails closed with [`Errno::NotImplemented`].
    fn fs_truncate(&self, _caller: &CallerContext<'_>, _fd: u32, _size: u64) -> SyscallResult {
        Err(Errno::NotImplemented)
    }

    /// Flush the filesystem backing open handle `fd` to its store
    /// (`PREREQUISITES.md` P-A).
    ///
    /// The default implementation fails closed with [`Errno::NotImplemented`].
    fn fs_sync(&self, _caller: &CallerContext<'_>, _fd: u32) -> SyscallResult {
        Err(Errno::NotImplemented)
    }

    /// Create the directory at the absolute path `path` (`path_len` bytes)
    /// (`PREREQUISITES.md` P-A).
    ///
    /// The dispatcher has already checked the caller holds
    /// [`CapabilityId::FS_ACCESS`] and that `path` is a non-null `UserPtr`.
    ///
    /// The default implementation fails closed with [`Errno::NotImplemented`].
    fn fs_mkdir(&self, _caller: &CallerContext<'_>, _path: u64, _path_len: usize) -> SyscallResult {
        Err(Errno::NotImplemented)
    }

    /// Remove the file or empty directory at the absolute path `path`
    /// (`path_len` bytes) (`PREREQUISITES.md` P-A).
    ///
    /// The default implementation fails closed with [`Errno::NotImplemented`].
    fn fs_unlink(
        &self,
        _caller: &CallerContext<'_>,
        _path: u64,
        _path_len: usize,
    ) -> SyscallResult {
        Err(Errno::NotImplemented)
    }

    /// Move the file or directory at absolute `src` (`src_len` bytes) to
    /// absolute `dst` (`dst_len` bytes) (`PREREQUISITES.md` P-A rename).
    ///
    /// The dispatcher has already checked the caller holds
    /// [`CapabilityId::FS_ACCESS`] and that both `src` and `dst` are
    /// non-null `UserPtr`s.
    ///
    /// The default implementation fails closed with [`Errno::NotImplemented`].
    fn fs_rename(
        &self,
        _caller: &CallerContext<'_>,
        _src: u64,
        _src_len: usize,
        _dst: u64,
        _dst_len: usize,
    ) -> SyscallResult {
        Err(Errno::NotImplemented)
    }
}

/// Architecture-neutral syscall dispatcher.
///
/// Borrows its dependencies for the lifetime of one syscall:
///
/// * `handlers` — the [`SyscallHandlers`] implementation owned by
///   `kernel/core`.
/// * `audit`   — the [`Sink`] every security record flows through.
///
/// The struct holds no mutable state; one [`Dispatcher`] may be
/// constructed per syscall or shared across an entire CPU as the
/// kernel sees fit.
pub struct Dispatcher<'a, H: SyscallHandlers + ?Sized, S: Sink + ?Sized> {
    handlers: &'a H,
    audit: &'a S,
}

impl<'a, H: SyscallHandlers + ?Sized, S: Sink + ?Sized> Dispatcher<'a, H, S> {
    /// Construct a new dispatcher bound to `handlers` and `audit`.
    pub const fn new(handlers: &'a H, audit: &'a S) -> Self {
        Self { handlers, audit }
    }

    /// Run the full sequence for one syscall and return its
    /// result.
    ///
    /// # Errors
    ///
    /// * [`Errno::OutOfRange`] — `raw_number` is not a valid
    ///   [`SyscallNumber`] (above [`SyscallNumber::MAX`]).
    /// * [`Errno::NotFound`] — the number is in range but no entry of
    ///   [`rustos_abi::SYSCALLS`] is assigned at that index (no gaps in
    ///   `abi-v1` today).
    /// * [`Errno::PermissionDenied`] — the caller lacks the syscall's
    ///   `required_capability`.
    /// * [`Errno::LengthOutOfRange`] — the argument tuple carries data
    ///   in a slot past the syscall's declared `arg_count`.
    /// * [`Errno::BadAlignment`] — a `UserPtr` argument is null.
    /// * Any other [`Errno`] returned by the handler verbatim.
    pub fn dispatch(
        &self,
        caller: &CallerContext<'_>,
        raw_number: u16,
        args: RawArgs,
    ) -> SyscallResult {
        let Ok(number) = SyscallNumber::from_raw(raw_number) else {
            self.audit_unknown(caller, raw_number);
            return Err(Errno::OutOfRange);
        };
        let Some(spec) = spec_for(number) else {
            self.audit_unknown(caller, raw_number);
            return Err(Errno::NotFound);
        };

        // step 2: capability check.
        if let Some(required) = spec.required_capability {
            if !caller.caps.has(required) {
                self.audit_denied(caller, spec);
                return Err(Errno::PermissionDenied);
            }
        }

        // step 3: argument validation. Trailing slots must be
        // zero — a buggy trampoline that leaks register state past
        // `arg_count` is a security defect, not "harmless".
        for slot in &args.0[spec.arg_count as usize..] {
            if *slot != 0 {
                self.audit_bad_args(caller, spec);
                return Err(Errno::LengthOutOfRange);
            }
        }
        for i in 0..spec.arg_count as usize {
            if let Err(err) = validate_arg(spec.args[i], args.0[i]) {
                self.audit_bad_args(caller, spec);
                return Err(err);
            }
        }

        // step 4: dispatch.
        let outcome = self.invoke(caller, spec, &args);

        // step 5: audit emission for security-relevant calls.
        match &outcome {
            Ok(_) if spec.audit => self.audit_invoked(caller, spec),
            // `WouldBlock` is the `abi-v1` "nothing yet, retry" signal, not a
            // rejection: the capability and argument checks all passed and no
            // security decision was taken. Record it below the error level so
            // a caller that legitimately polls while pending (e.g. `login`
            // reading `users_db_read` while the encrypted root unlocks) cannot
            // flood the log with errors.
            Err(Errno::WouldBlock) if spec.audit => self.audit_would_block(caller, spec),
            Err(_) if spec.audit => self.audit_rejected(caller, spec, outcome.as_ref().err()),
            _ => {}
        }
        outcome
    }

    // This is the `abi-v1` dispatch table: a single flat `match` with exactly
    // one arm per syscall, so its length grows by one arm with every syscall
    // the frozen table gains. That is the intended shape — splitting it would
    // scatter the one-to-one number→handler mapping the ABI cross-check relies
    // on — so the `too_many_lines` heuristic does not apply here (the body is
    // trivially uniform, not complex).: justified allow.
    #[allow(clippy::too_many_lines)]
    fn invoke(
        &self,
        caller: &CallerContext<'_>,
        spec: &SyscallSpec,
        args: &RawArgs,
    ) -> SyscallResult {
        match spec.number {
            SyscallNumber::YIELD => self.handlers.yield_now(caller),
            SyscallNumber::EXIT => {
                // `validate_arg` guarantees the value is a
                // sign-extended `i32`. Recover the original `i32` by
                // truncating the low 32 bits — equivalent to `as i32`
                // but without the lint-flagged truncation cast.
                #[allow(clippy::cast_possible_wrap)]
                let code = (args.0[0] & 0xFFFF_FFFF) as i32;
                self.handlers.exit(caller, code)
            }
            SyscallNumber::IPC_SEND => {
                let len = decode_len(args.0[2])?;
                self.handlers.ipc_send(caller, args.0[0], args.0[1], len)
            }
            SyscallNumber::IPC_RECV => {
                let len = decode_len(args.0[2])?;
                self.handlers.ipc_recv(caller, args.0[0], args.0[1], len)
            }
            SyscallNumber::CAP_QUERY => {
                let cap = decode_capability(args.0[0])?;
                self.handlers.cap_query(caller, cap)
            }
            SyscallNumber::CAP_DELEGATE => self.handlers.cap_delegate(caller, args.0[0], args.0[1]),
            SyscallNumber::CAP_REVOKE => {
                let cap = decode_capability(args.0[1])?;
                self.handlers.cap_revoke(caller, args.0[0], cap)
            }
            SyscallNumber::CLOCK_GET => self.handlers.clock_get(caller),
            SyscallNumber::IRQ_BIND => self.handlers.irq_bind(caller, decode_u32(args.0[0])),
            SyscallNumber::IRQ_WAIT => {
                let handle = IrqHandle::from_raw(args.0[0]);
                self.handlers.irq_wait(caller, handle, args.0[1])
            }
            SyscallNumber::RANDOM_GET => {
                let len = decode_len(args.0[1])?;
                // `from_bits` rejects any reserved bit.
                let flags = RandomFlags::from_bits(decode_u32(args.0[2]))?;
                self.handlers.random_get(caller, args.0[0], len, flags)
            }
            SyscallNumber::STREAM_WRITE => {
                let len = decode_len(args.0[2])?;
                self.handlers
                    .stream_write(caller, decode_u32(args.0[0]), args.0[1], len)
            }
            SyscallNumber::SPAWN => {
                let len = decode_len(args.0[1])?;
                // args[2] is the console selector: the `CONSOLE_INHERIT`
                // sentinel or an installed console index, validated by
                // the handler against the live console list.
                self.handlers.spawn(caller, args.0[0], len, args.0[2])
            }
            SyscallNumber::STREAM_READ => {
                let len = decode_len(args.0[2])?;
                self.handlers
                    .stream_read(caller, decode_u32(args.0[0]), args.0[1], len)
            }
            SyscallNumber::MEM_MAP => {
                let len = decode_len(args.0[0])?;
                // `from_bits` rejects any reserved bit.
                let flags = MapFlags::from_bits(decode_u32(args.0[1]))?;
                self.handlers.mem_map(caller, len, flags, args.0[2])
            }
            SyscallNumber::MEM_UNMAP => {
                let len = decode_len(args.0[1])?;
                self.handlers.mem_unmap(caller, args.0[0], len)
            }
            SyscallNumber::WAIT => {
                // `validate_arg` guarantees args[0] is a sign-extended
                // `i32`; recover it by truncating the low 32 bits (the
                // same recovery `EXIT` uses), and args[1] is a non-null
                // `UserPtr`.
                #[allow(clippy::cast_possible_wrap)]
                let pid = (args.0[0] & 0xFFFF_FFFF) as i32;
                self.handlers.wait(caller, pid, args.0[1])
            }
            SyscallNumber::RLIMIT_GET => {
                self.handlers
                    .rlimit_get(caller, decode_u32(args.0[0]), args.0[1])
            }
            SyscallNumber::RLIMIT_SET => {
                self.handlers
                    .rlimit_set(caller, decode_u32(args.0[0]), args.0[1])
            }
            SyscallNumber::USERS_DB_READ => {
                // `validate_arg` guarantees args[0] is a non-null
                // `UserPtr`; args[1] is the buffer capacity.
                let len = decode_len(args.0[1])?;
                self.handlers.users_db_read(caller, args.0[0], len)
            }
            SyscallNumber::CONSOLE_COUNT => self.handlers.console_count(caller),
            SyscallNumber::STREAM_ECHO => {
                self.handlers
                    .stream_echo(caller, decode_u32(args.0[0]), decode_u32(args.0[1]))
            }
            SyscallNumber::KEY_INJECT => {
                // `validate_arg` guarantees args[0] is a non-null
                // `UserPtr`; args[1] is the record length.
                let len = decode_len(args.0[1])?;
                self.handlers.key_inject(caller, args.0[0], len)
            }
            SyscallNumber::DISPLAY_ACQUIRE => self.handlers.display_acquire(caller),
            SyscallNumber::DISPLAY_RELEASE => self.handlers.display_release(caller),
            SyscallNumber::KEYBOARD_READ => {
                // `validate_arg` guarantees args[0] is a non-null
                // `UserPtr`; args[1] is the buffer capacity.
                let len = decode_len(args.0[1])?;
                self.handlers.keyboard_read(caller, args.0[0], len)
            }
            // `validate_arg` accepts args[0] as an opaque `Handle` u64; the
            // handler resolves it against the calling task and the grant
            // table (forgery + ownership are checked there). args[1] is the byte offset of the sub-region within the
            // granted window and args[2] its length; the handler confirms the
            // sub-region lies wholly inside the grant.
            SyscallNumber::MMIO_MAP => {
                let len = decode_len(args.0[2])?;
                self.handlers.mmio_map(caller, args.0[0], args.0[1], len)
            }
            SyscallNumber::DMA_ALLOC => {
                // `validate_arg` accepts args[0] as an opaque `Handle` u64
                // (resolved against the calling task + grant table in the
                // handler); args[1] is the byte length and
                // args[2] is the non-null `device_out` `UserPtr` the handler
                // writes the device-visible base to.
                let len = decode_len(args.0[1])?;
                self.handlers.dma_alloc(caller, args.0[0], len, args.0[2])
            }
            SyscallNumber::DMA_FREE => {
                // `validate_arg` accepts args[0] as an opaque `Handle` u64
                // (resolved against the calling task + grant table in the
                // handler); args[1] is the CPU virtual base the carve
                // returned — a lookup key, never dereferenced by the kernel.
                self.handlers.dma_free(caller, args.0[0], args.0[1])
            }
            SyscallNumber::RESOURCE_GRANTS => {
                // args[0] is a non-null `UserPtr` (dispatcher-checked); args[1]
                // is the buffer capacity.
                let len = decode_len(args.0[1])?;
                self.handlers.resource_grants(caller, args.0[0], len)
            }
            SyscallNumber::HW_TREE_READ => {
                // args[0] is a non-null `UserPtr` (dispatcher-checked); args[1]
                // is the buffer capacity.
                let len = decode_len(args.0[1])?;
                self.handlers.hw_tree_read(caller, args.0[0], len)
            }
            SyscallNumber::HW_TREE_WAIT => {
                // args[0] is the last observed generation, args[1] the
                // timeout in nanoseconds (`u64::MAX` for unbounded).
                self.handlers.hw_tree_wait(caller, args.0[0], args.0[1])
            }
            SyscallNumber::USERS_DB_WAIT => {
                // args[0] is the timeout in nanoseconds (`u64::MAX` for an
                // effectively unbounded wait).
                self.handlers.users_db_wait(caller, args.0[0])
            }
            SyscallNumber::IPC_CALL => {
                // args[0] is the call-endpoint id; args[1]/args[3] are non-null
                // `UserPtr`s (dispatcher-checked); args[2]/args[4] are the
                // request length and reply-buffer capacity.
                let request_len = decode_len(args.0[2])?;
                let reply_cap = decode_len(args.0[4])?;
                self.handlers.ipc_call(
                    caller,
                    args.0[0],
                    args.0[1],
                    request_len,
                    args.0[3],
                    reply_cap,
                )
            }
            SyscallNumber::CALL_CREATE => {
                // args[0] is the endpoint id; args[1]/args[2] are non-null
                // `UserPtr`s to the send/recv `CapabilitySet` wire images
                // (dispatcher-checked); args[3..6] are the payload + capacity
                // bounds.
                let max_request = decode_len(args.0[3])?;
                let max_reply = decode_len(args.0[4])?;
                let capacity = decode_len(args.0[5])?;
                self.handlers.call_create(
                    caller,
                    args.0[0],
                    args.0[1],
                    args.0[2],
                    max_request,
                    max_reply,
                    capacity,
                )
            }
            SyscallNumber::CALL_RECV => {
                // args[0] is the endpoint id; args[1]/args[3] are non-null
                // `UserPtr`s (request buffer, ticket-out, dispatcher-checked);
                // args[2] is the request-buffer capacity.
                let buf_cap = decode_len(args.0[2])?;
                self.handlers
                    .call_recv(caller, args.0[0], args.0[1], buf_cap, args.0[3])
            }
            SyscallNumber::CALL_REPLY => {
                // args[0] is the endpoint id; args[1] the ticket; args[2] is a
                // non-null reply `UserPtr` (dispatcher-checked); args[3] the
                // reply length.
                let reply_len = decode_len(args.0[3])?;
                self.handlers
                    .call_reply(caller, args.0[0], args.0[1], args.0[2], reply_len)
            }
            SyscallNumber::CALL_PEER_ORIGIN => {
                // args[0] is the endpoint id; args[1] the in-service ticket;
                // args[2] is a non-null origin-out `UserPtr` (dispatcher-
                // checked); args[3] its capacity in bytes.
                let origin_cap = decode_len(args.0[3])?;
                self.handlers
                    .call_peer_origin(caller, args.0[0], args.0[1], args.0[2], origin_cap)
            }
            SyscallNumber::LOG_EMIT => {
                // args[0] is the non-null record `UserPtr` (dispatcher-
                // checked); args[1] is its byte length.
                let len = decode_len(args.0[1])?;
                self.handlers.log_emit(caller, args.0[0], len)
            }
            SyscallNumber::HW_EMIT_NODE => {
                // args[0] is the non-null encoded `HwNode` `UserPtr`
                // (dispatcher-checked); args[1] is its byte length.
                let len = decode_len(args.0[1])?;
                self.handlers.hw_emit_node(caller, args.0[0], len)
            }
            SyscallNumber::HW_REMOVE_NODE => {
                // args[0] is the `HwNode::id` to remove (a plain `u64`,
                // resolved against the live tree by the handler).
                self.handlers.hw_remove_node(caller, args.0[0])
            }
            SyscallNumber::MSI_ALLOC => {
                // args[0] is the non-null out `UserPtr` (dispatcher-checked);
                // args[1] is its capacity in bytes.
                let out_len = decode_len(args.0[1])?;
                self.handlers.msi_alloc(caller, args.0[0], out_len)
            }
            SyscallNumber::SHM_CREATE => {
                // args[0] is the region length in bytes; args[1] is the
                // non-null `id_out` `UserPtr` the handler writes the region
                // id to (dispatcher-checked).
                let len = decode_len(args.0[0])?;
                self.handlers.shm_create(caller, len, args.0[1])
            }
            SyscallNumber::SHM_MAP => {
                // args[0] is an opaque `Handle` u64; the handler resolves it
                // against the calling task and the grant table (forgery +
                // ownership are checked there).
                self.handlers.shm_map(caller, args.0[0])
            }
            SyscallNumber::SHM_UNMAP => {
                // args[0] is the base virtual address the map returned; args[1]
                // is its length in bytes.
                let len = decode_len(args.0[1])?;
                self.handlers.shm_unmap(caller, args.0[0], len)
            }
            SyscallNumber::WAITSET_CREATE => {
                // No arguments; the handler mints a handle for the caller.
                self.handlers.waitset_create(caller)
            }
            SyscallNumber::WAITSET_CTL => {
                // args[0] is the wait-set handle; args[1]/[2] are the op and
                // source kind (`u32` each, validated by the handler); args[3]
                // is the resource id; args[4] is the caller's token.
                let op = decode_u32(args.0[1]);
                let kind = decode_u32(args.0[2]);
                self.handlers
                    .waitset_ctl(caller, args.0[0], op, kind, args.0[3], args.0[4])
            }
            SyscallNumber::WAITSET_WAIT => {
                // args[0] is the wait-set handle; args[1] is the relative
                // timeout (`u64::MAX` = no timeout); args[2] is the non-null
                // `token_out` `UserPtr` (dispatcher-checked).
                self.handlers
                    .waitset_wait(caller, args.0[0], args.0[1], args.0[2])
            }
            SyscallNumber::FS_OPEN => {
                // args[0] is the non-null path `UserPtr` (dispatcher-checked);
                // args[1] is the path length; args[2] is the `OpenFlags`
                // bits, rejected for any reserved/illegal combination here.
                let path_len = decode_len(args.0[1])?;
                let flags = OpenFlags::from_bits(decode_u32(args.0[2]))?;
                self.handlers.fs_open(caller, args.0[0], path_len, flags)
            }
            SyscallNumber::FS_CLOSE => self.handlers.fs_close(caller, decode_u32(args.0[0])),
            SyscallNumber::FS_READ => {
                // args[0] fd; args[1] offset; args[2] non-null destination
                // `UserPtr` (dispatcher-checked); args[3] length.
                let len = decode_len(args.0[3])?;
                self.handlers
                    .fs_read(caller, decode_u32(args.0[0]), args.0[1], args.0[2], len)
            }
            SyscallNumber::FS_WRITE => {
                let len = decode_len(args.0[3])?;
                self.handlers
                    .fs_write(caller, decode_u32(args.0[0]), args.0[1], args.0[2], len)
            }
            SyscallNumber::FS_READDIR => {
                let len = decode_len(args.0[2])?;
                self.handlers
                    .fs_readdir(caller, decode_u32(args.0[0]), args.0[1], len)
            }
            SyscallNumber::FS_STAT => {
                let out_len = decode_len(args.0[2])?;
                self.handlers
                    .fs_stat(caller, decode_u32(args.0[0]), args.0[1], out_len)
            }
            SyscallNumber::FS_TRUNCATE => {
                self.handlers
                    .fs_truncate(caller, decode_u32(args.0[0]), args.0[1])
            }
            SyscallNumber::FS_SYNC => self.handlers.fs_sync(caller, decode_u32(args.0[0])),
            SyscallNumber::FS_MKDIR => {
                let path_len = decode_len(args.0[1])?;
                self.handlers.fs_mkdir(caller, args.0[0], path_len)
            }
            SyscallNumber::FS_UNLINK => {
                let path_len = decode_len(args.0[1])?;
                self.handlers.fs_unlink(caller, args.0[0], path_len)
            }
            SyscallNumber::FS_RENAME => {
                let src_len = decode_len(args.0[1])?;
                let dst_len = decode_len(args.0[3])?;
                self.handlers
                    .fs_rename(caller, args.0[0], src_len, args.0[2], dst_len)
            }
            SyscallNumber::WALL_TIME_GET => {
                // args[0] is the non-null out `UserPtr` (dispatcher-checked);
                // args[1] is its capacity in bytes.
                let out_cap = decode_len(args.0[1])?;
                self.handlers.wall_time_get(caller, args.0[0], out_cap)
            }
            SyscallNumber::WALL_TIME_SET => {
                // args[0] is the non-null `Time64` `UserPtr` (dispatcher-
                // checked); args[1] is its byte length; args[2] is the
                // `WallTimeState` discriminant (validated by the handler).
                let time_len = decode_len(args.0[1])?;
                let state = decode_u32(args.0[2]);
                self.handlers
                    .wall_time_set(caller, args.0[0], time_len, state)
            }
            SyscallNumber::BOOT_ID_GET => {
                // args[0] is the non-null out `UserPtr` (dispatcher-checked);
                // args[1] is its capacity in bytes.
                let out_cap = decode_len(args.0[1])?;
                self.handlers.boot_id_get(caller, args.0[0], out_cap)
            }
            _ => Err(Errno::NotFound),
        }
    }

    fn audit_unknown(&self, caller: &CallerContext<'_>, number: u16) {
        let mut t = [0u8; 16];
        let mut p = [0u8; PROC_ID_HEX_LEN];
        let mut n = [0u8; 12];
        // The number always fits in `u32` (it is a `u16`); `format_usize`
        // saturates above `i32::MAX` which never trips for a `u16`.
        record(
            self.audit,
            AuditEvent::SyscallUnknown,
            &[
                Field {
                    key: "task",
                    value: format_hex_u64(caller.task_id.0, &mut t),
                },
                Field {
                    key: "proc",
                    value: caller.caps.proc_id().write_hex(&mut p),
                },
                Field {
                    key: "no",
                    value: rustos_util::fmt::format_usize(usize::from(number), &mut n),
                },
            ],
        );
    }

    fn audit_denied(&self, caller: &CallerContext<'_>, spec: &SyscallSpec) {
        let mut t = [0u8; 16];
        let mut p = [0u8; PROC_ID_HEX_LEN];
        record(
            self.audit,
            AuditEvent::SyscallPermissionDenied,
            &[
                Field {
                    key: "task",
                    value: format_hex_u64(caller.task_id.0, &mut t),
                },
                Field {
                    key: "proc",
                    value: caller.caps.proc_id().write_hex(&mut p),
                },
                Field {
                    key: "sc",
                    value: spec.name,
                },
            ],
        );
    }

    fn audit_bad_args(&self, caller: &CallerContext<'_>, spec: &SyscallSpec) {
        let mut t = [0u8; 16];
        let mut p = [0u8; PROC_ID_HEX_LEN];
        record(
            self.audit,
            AuditEvent::SyscallBadArguments,
            &[
                Field {
                    key: "task",
                    value: format_hex_u64(caller.task_id.0, &mut t),
                },
                Field {
                    key: "proc",
                    value: caller.caps.proc_id().write_hex(&mut p),
                },
                Field {
                    key: "sc",
                    value: spec.name,
                },
            ],
        );
    }

    fn audit_invoked(&self, caller: &CallerContext<'_>, spec: &SyscallSpec) {
        let mut t = [0u8; 16];
        let mut p = [0u8; PROC_ID_HEX_LEN];
        record(
            self.audit,
            AuditEvent::SyscallInvoked,
            &[
                Field {
                    key: "task",
                    value: format_hex_u64(caller.task_id.0, &mut t),
                },
                Field {
                    key: "proc",
                    value: caller.caps.proc_id().write_hex(&mut p),
                },
                Field {
                    key: "sc",
                    value: spec.name,
                },
            ],
        );
    }

    fn audit_would_block(&self, caller: &CallerContext<'_>, spec: &SyscallSpec) {
        let mut t = [0u8; 16];
        let mut p = [0u8; PROC_ID_HEX_LEN];
        record(
            self.audit,
            AuditEvent::SyscallHandlerWouldBlock,
            &[
                Field {
                    key: "task",
                    value: format_hex_u64(caller.task_id.0, &mut t),
                },
                Field {
                    key: "proc",
                    value: caller.caps.proc_id().write_hex(&mut p),
                },
                Field {
                    key: "sc",
                    value: spec.name,
                },
            ],
        );
    }

    fn audit_rejected(&self, caller: &CallerContext<'_>, spec: &SyscallSpec, err: Option<&Errno>) {
        let mut t = [0u8; 16];
        let mut e = [0u8; 12];
        let err_field = match err {
            Some(e_ref) => format_i32(e_ref.as_i32(), &mut e),
            None => "?",
        };
        let mut p = [0u8; PROC_ID_HEX_LEN];
        record(
            self.audit,
            AuditEvent::SyscallHandlerRejected,
            &[
                Field {
                    key: "task",
                    value: format_hex_u64(caller.task_id.0, &mut t),
                },
                Field {
                    key: "proc",
                    value: caller.caps.proc_id().write_hex(&mut p),
                },
                Field {
                    key: "sc",
                    value: spec.name,
                },
                Field {
                    key: "err",
                    value: err_field,
                },
            ],
        );
    }
}

fn validate_arg(ty: AbiType, raw: u64) -> Result<(), Errno> {
    match ty {
        AbiType::Unit => {
            if raw == 0 {
                Ok(())
            } else {
                Err(Errno::LengthOutOfRange)
            }
        }
        AbiType::I32 => {
            // Upper 32 bits must equal the sign extension of the low
            // 32 bits — anything else is a malformed trampoline value.
            // Truncating to `i32` and zero-extending back to a `u64`
            // gives the canonical sign-extended representation; the
            // raw value must equal it exactly.
            #[allow(clippy::cast_possible_truncation, clippy::cast_possible_wrap)]
            let low = (raw & 0xFFFF_FFFF) as i32;
            #[allow(clippy::cast_sign_loss)]
            let extended = i64::from(low) as u64;
            if raw == extended {
                Ok(())
            } else {
                Err(Errno::OutOfRange)
            }
        }
        AbiType::U32 => {
            if raw >> 32 == 0 {
                Ok(())
            } else {
                Err(Errno::OutOfRange)
            }
        }
        AbiType::U64 | AbiType::Handle | AbiType::IpcEndpoint => Ok(()),
        AbiType::Cap => decode_capability(raw).map(|_| ()),
        AbiType::UserPtr => {
            if raw == 0 {
                Err(Errno::BadAlignment)
            } else {
                Ok(())
            }
        }
        AbiType::Len => decode_len(raw).map(|_| ()),
        AbiType::Errno => {
            // Errno is a return type; never appears as an input.
            Err(Errno::OutOfRange)
        }
    }
}

fn decode_capability(raw: u64) -> Result<CapabilityId, Errno> {
    if raw >> 16 != 0 {
        return Err(Errno::OutOfRange);
    }
    // The `>> 16 != 0` check above guarantees `raw` fits in `u16`.
    let narrowed = u16::try_from(raw).map_err(|_| Errno::OutOfRange)?;
    CapabilityId::from_raw(narrowed)
}

fn decode_len(raw: u64) -> Result<usize, Errno> {
    usize::try_from(raw).map_err(|_| Errno::LengthOutOfRange)
}

/// Narrow a `U32`-typed argument register to `u32`.
///
/// `validate_arg` has already rejected any value whose upper 32 bits are
/// non-zero (the `AbiType::U32` rule), so the low-32 truncation is
/// lossless; the mask makes that explicit and keeps the lint allow in one
/// place rather than at every call site.
#[allow(clippy::cast_possible_truncation)]
const fn decode_u32(raw: u64) -> u32 {
    (raw & 0xFFFF_FFFF) as u32
}

#[cfg(test)]
mod tests {
    use super::*;
    use alloc::vec::Vec;
    use core::cell::RefCell;
    use rustos_abi::{CapabilityId, SyscallNumber};
    use rustos_caps::CapabilitySet;
    use rustos_kernel_sec::{TaskCapabilities, TaskId, UserId};
    use rustos_log::{set_max_level, Event, Level};

    /// Single-threaded sink that records every event identifier.
    struct RecordingSink {
        events: RefCell<Vec<u32>>,
    }
    impl RecordingSink {
        fn new() -> Self {
            set_max_level(Level::Trace);
            Self {
                events: RefCell::new(Vec::new()),
            }
        }
        fn ids(&self) -> Vec<u32> {
            self.events.borrow().clone()
        }
    }
    impl Sink for RecordingSink {
        fn write_event(&self, event: &Event<'_>) {
            self.events.borrow_mut().push(event.id.0);
        }
    }

    fn caps_of(items: &[CapabilityId]) -> CapabilitySet {
        let mut s = CapabilitySet::empty();
        for c in items {
            s.insert(*c);
        }
        s
    }

    fn build_caps(items: &[CapabilityId], sink: &RecordingSink) -> TaskCapabilities {
        let set = caps_of(items);
        let t = TaskCapabilities::derive(TaskId(0xA), UserId(1000), set, set, sink);
        // Drop the derivation event so tests can assert against dispatcher
        // events alone.
        sink.events.borrow_mut().clear();
        t
    }

    /// Handler that records each invocation and returns canned results.
    #[derive(Default)]
    struct MockHandlers {
        calls: RefCell<Vec<&'static str>>,
        force_err: Option<Errno>,
    }
    impl MockHandlers {
        fn record(&self, name: &'static str) {
            self.calls.borrow_mut().push(name);
        }
        fn last(&self) -> Option<&'static str> {
            self.calls.borrow().last().copied()
        }
    }
    impl SyscallHandlers for MockHandlers {
        fn yield_now(&self, _c: &CallerContext<'_>) -> SyscallResult {
            self.record("yield");
            Ok(0)
        }
        fn exit(&self, _c: &CallerContext<'_>, code: i32) -> SyscallResult {
            self.record("exit");
            #[allow(clippy::cast_sign_loss)]
            let bits = code as u32;
            Ok(u64::from(bits))
        }
        fn ipc_send(&self, _c: &CallerContext<'_>, _e: u64, _p: u64, len: usize) -> SyscallResult {
            self.record("ipc_send");
            if let Some(err) = self.force_err {
                Err(err)
            } else {
                Ok(len as u64)
            }
        }
        fn ipc_recv(&self, _c: &CallerContext<'_>, _e: u64, _p: u64, len: usize) -> SyscallResult {
            self.record("ipc_recv");
            Ok(len as u64)
        }
        fn cap_query(&self, c: &CallerContext<'_>, cap: CapabilityId) -> SyscallResult {
            self.record("cap_query");
            Ok(u64::from(c.caps.has(cap)))
        }
        fn cap_delegate(&self, _c: &CallerContext<'_>, _t: u64, _p: u64) -> SyscallResult {
            self.record("cap_delegate");
            Ok(0)
        }
        fn cap_revoke(&self, _c: &CallerContext<'_>, _t: u64, _cap: CapabilityId) -> SyscallResult {
            self.record("cap_revoke");
            Ok(0)
        }
        fn clock_get(&self, _c: &CallerContext<'_>) -> SyscallResult {
            self.record("clock_get");
            Ok(42)
        }
        fn irq_bind(&self, _c: &CallerContext<'_>, line: u32) -> SyscallResult {
            self.record("irq_bind");
            // Echo the line back as a fabricated handle so the test
            // can assert the dispatcher decoded the argument
            // correctly without inventing a real IRQ allocator.
            Ok(u64::from(line) | 0xF000_0000_0000_0000)
        }
        fn irq_wait(
            &self,
            _c: &CallerContext<'_>,
            _h: IrqHandle,
            _timeout_ns: u64,
        ) -> SyscallResult {
            self.record("irq_wait");
            Ok(0)
        }
        fn random_get(
            &self,
            _c: &CallerContext<'_>,
            _buf: u64,
            len: usize,
            _flags: RandomFlags,
        ) -> SyscallResult {
            self.record("random_get");
            // Echo the requested length back as the byte count so the
            // reachability test can assert the dispatcher decoded the
            // arguments without inventing a real reserve here.
            Ok(len as u64)
        }
        fn stream_write(
            &self,
            _c: &CallerContext<'_>,
            _fd: u32,
            _buf: u64,
            len: usize,
        ) -> SyscallResult {
            self.record("stream_write");
            // Echo the requested length back as the byte count so the
            // reachability test can assert the dispatcher decoded the
            // arguments without wiring a real console here.
            Ok(len as u64)
        }
        fn spawn(
            &self,
            _c: &CallerContext<'_>,
            _path: u64,
            path_len: usize,
            _console: u64,
        ) -> SyscallResult {
            self.record("spawn");
            // Echo the path length back so the reachability test can
            // assert the dispatcher decoded the `(path, path_len,
            // console)` arguments without wiring a real spawn service
            // here.
            Ok(path_len as u64)
        }
        fn stream_read(
            &self,
            _c: &CallerContext<'_>,
            _fd: u32,
            _buf: u64,
            len: usize,
        ) -> SyscallResult {
            self.record("stream_read");
            // Echo the requested length back as the byte count so the
            // reachability test can assert the dispatcher decoded the
            // arguments without wiring a real console here.
            Ok(len as u64)
        }
        fn mem_map(
            &self,
            _c: &CallerContext<'_>,
            len: usize,
            _flags: MapFlags,
            _addr_hint: u64,
        ) -> SyscallResult {
            self.record("mem_map");
            // Echo the requested length back as a fabricated base so the
            // reachability test can assert the dispatcher decoded the
            // `(len, flags, addr_hint)` arguments without wiring a real
            // memory service here.
            Ok(len as u64)
        }
        fn mem_unmap(&self, _c: &CallerContext<'_>, _base: u64, _len: usize) -> SyscallResult {
            self.record("mem_unmap");
            Ok(0)
        }
        fn wait(&self, _c: &CallerContext<'_>, pid: i32, _status: u64) -> SyscallResult {
            self.record("wait");
            // Echo the requested pid back as a fabricated reaped PID so the
            // reachability test can assert the dispatcher decoded the
            // `(pid, status)` arguments without wiring a real wait service
            // here. The reachability test passes pid 0 (a valid I32).
            #[allow(clippy::cast_sign_loss)]
            Ok(u64::from(pid as u32))
        }
        fn rlimit_get(&self, _c: &CallerContext<'_>, kind: u32, _out: u64) -> SyscallResult {
            self.record("rlimit_get");
            // Echo the kind back so the reachability test can assert the
            // dispatcher decoded `(kind, out)` without wiring a real
            // resource-limit service here.
            Ok(u64::from(kind))
        }
        fn rlimit_set(&self, _c: &CallerContext<'_>, kind: u32, _value: u64) -> SyscallResult {
            self.record("rlimit_set");
            Ok(u64::from(kind))
        }
        fn users_db_read(&self, _c: &CallerContext<'_>, _buf: u64, len: usize) -> SyscallResult {
            self.record("users_db_read");
            // Echo the capacity back so the reachability test can assert
            // the dispatcher decoded `(buf, len)` without wiring a real
            // users-database service here.
            Ok(len as u64)
        }
        fn console_count(&self, _c: &CallerContext<'_>) -> SyscallResult {
            self.record("console_count");
            // A fabricated single-console topology so the reachability
            // test can assert the dispatcher routed the call without
            // wiring a real console list here.
            Ok(1)
        }
        fn stream_echo(&self, _c: &CallerContext<'_>, _fd: u32, _enabled: u32) -> SyscallResult {
            self.record("stream_echo");
            // Success so the reachability test can assert the dispatcher
            // decoded `(fd, enabled)` without wiring a real console here.
            Ok(0)
        }
        fn key_inject(&self, _c: &CallerContext<'_>, _buf: u64, len: usize) -> SyscallResult {
            self.record("key_inject");
            // Echo the length back so the reachability test can assert the
            // dispatcher decoded `(buf, len)` without wiring a real
            // input-focus arbiter here.
            Ok(len as u64)
        }
        fn display_acquire(&self, _c: &CallerContext<'_>) -> SyscallResult {
            self.record("display_acquire");
            Ok(0)
        }
        fn display_release(&self, _c: &CallerContext<'_>) -> SyscallResult {
            self.record("display_release");
            Ok(0)
        }
        fn keyboard_read(&self, _c: &CallerContext<'_>, _buf: u64, len: usize) -> SyscallResult {
            self.record("keyboard_read");
            // Echo the length back so the reachability test can assert the
            // dispatcher decoded `(buf, len)` without wiring a real
            // keyboard channel here.
            Ok(len as u64)
        }

        fn mmio_map(
            &self,
            _c: &CallerContext<'_>,
            handle: u64,
            offset: u64,
            len: usize,
        ) -> SyscallResult {
            self.record("mmio_map");
            // Echo `handle + offset + len` back so the reachability test can
            // assert the dispatcher decoded all three arguments (handle,
            // sub-region offset, length) without wiring a real grant table /
            // map facility here.
            Ok(handle + offset + len as u64)
        }

        fn dma_alloc(
            &self,
            _c: &CallerContext<'_>,
            handle: u64,
            _len: usize,
            _device_out: u64,
        ) -> SyscallResult {
            self.record("dma_alloc");
            // Echo the handle back so the reachability test can assert the
            // dispatcher decoded the arguments without wiring a real grant
            // table / DMA facility here.
            Ok(handle)
        }

        fn dma_free(&self, _c: &CallerContext<'_>, handle: u64, cpu_va: u64) -> SyscallResult {
            self.record("dma_free");
            // Echo `handle + cpu_va` back so the reachability test can assert
            // the dispatcher decoded both arguments (grant handle, CPU base)
            // without wiring a real grant table / DMA facility here.
            Ok(handle + cpu_va)
        }

        fn resource_grants(
            &self,
            _caller: &CallerContext<'_>,
            _buf: u64,
            len: usize,
        ) -> SyscallResult {
            self.record("resource_grants");
            // Echo the buffer length back so the reachability test can assert
            // the dispatcher decoded the arguments without wiring a real grant
            // table here.
            Ok(len as u64)
        }

        fn hw_tree_read(&self, _c: &CallerContext<'_>, _buf: u64, len: usize) -> SyscallResult {
            self.record("hw_tree_read");
            // Echo the buffer length back so the reachability test can assert
            // the dispatcher decoded `(buf, len)` without wiring a real
            // hardware-tree store here.
            Ok(len as u64)
        }

        fn hw_tree_wait(
            &self,
            _c: &CallerContext<'_>,
            last_generation: u64,
            _timeout_ns: u64,
        ) -> SyscallResult {
            self.record("hw_tree_wait");
            // Echo the generation back so the reachability test can assert the
            // dispatcher decoded `(last_generation, timeout_ns)` without
            // wiring a real store / scheduler here.
            Ok(last_generation)
        }

        fn users_db_wait(&self, _c: &CallerContext<'_>, timeout_ns: u64) -> SyscallResult {
            self.record("users_db_wait");
            // Echo the timeout back so the reachability test can assert the
            // dispatcher decoded the single `timeout_ns` argument without
            // wiring a real users-database source / scheduler here.
            Ok(timeout_ns)
        }

        fn ipc_call(
            &self,
            _c: &CallerContext<'_>,
            _endpoint: u64,
            _request: u64,
            request_len: usize,
            _reply: u64,
            _reply_cap: usize,
        ) -> SyscallResult {
            self.record("ipc_call");
            // Echo the request length back so the reachability test can assert
            // the dispatcher decoded the five arguments without wiring a real
            // call-endpoint registry / scheduler here.
            Ok(request_len as u64)
        }

        #[allow(clippy::too_many_arguments)] // Matches the trait declaration's justified count.
        fn call_create(
            &self,
            _c: &CallerContext<'_>,
            _endpoint: u64,
            _send_caps: u64,
            _recv_caps: u64,
            _max_request: usize,
            _max_reply: usize,
            _capacity: usize,
        ) -> SyscallResult {
            self.record("call_create");
            Ok(0)
        }

        fn call_recv(
            &self,
            _c: &CallerContext<'_>,
            _endpoint: u64,
            _buf: u64,
            buf_cap: usize,
            _ticket_out: u64,
        ) -> SyscallResult {
            self.record("call_recv");
            // Echo the buffer capacity so the reachability test can assert the
            // dispatcher decoded the four arguments without wiring a real
            // endpoint / scheduler here.
            Ok(buf_cap as u64)
        }

        fn call_reply(
            &self,
            _c: &CallerContext<'_>,
            _endpoint: u64,
            _ticket: u64,
            _reply: u64,
            _reply_len: usize,
        ) -> SyscallResult {
            self.record("call_reply");
            Ok(0)
        }

        fn call_peer_origin(
            &self,
            _c: &CallerContext<'_>,
            _endpoint: u64,
            _ticket: u64,
            _origin: u64,
            origin_cap: usize,
        ) -> SyscallResult {
            self.record("call_peer_origin");
            // Echo the buffer capacity so the reachability test can assert the
            // dispatcher decoded the four arguments without wiring a real
            // endpoint / in-service call here.
            Ok(origin_cap as u64)
        }

        fn wall_time_get(
            &self,
            _c: &CallerContext<'_>,
            _out: u64,
            out_cap: usize,
        ) -> SyscallResult {
            self.record("wall_time_get");
            // Echo the capacity so the reachability test can assert the
            // dispatcher decoded both arguments without wiring a real clock.
            Ok(out_cap as u64)
        }

        fn wall_time_set(
            &self,
            _c: &CallerContext<'_>,
            _time: u64,
            time_len: usize,
            _state: u32,
        ) -> SyscallResult {
            self.record("wall_time_set");
            // Echo the length so the reachability test can assert the
            // dispatcher decoded the arguments without wiring a real clock.
            Ok(time_len as u64)
        }

        fn boot_id_get(&self, _c: &CallerContext<'_>, _out: u64, out_cap: usize) -> SyscallResult {
            self.record("boot_id_get");
            // Echo the capacity so the reachability test can assert the
            // dispatcher decoded both arguments without wiring a real boot id.
            Ok(out_cap as u64)
        }

        fn log_emit(&self, _c: &CallerContext<'_>, _record: u64, _len: usize) -> SyscallResult {
            self.record("log_emit");
            Ok(0)
        }

        fn hw_emit_node(&self, _c: &CallerContext<'_>, _node: u64, _len: usize) -> SyscallResult {
            self.record("hw_emit_node");
            Ok(0)
        }

        fn hw_remove_node(&self, _c: &CallerContext<'_>, _node_id: u64) -> SyscallResult {
            self.record("hw_remove_node");
            Ok(0)
        }

        fn msi_alloc(&self, _c: &CallerContext<'_>, _out: u64, out_len: usize) -> SyscallResult {
            self.record("msi_alloc");
            // Echo the buffer length so the reachability test can assert the
            // dispatcher decoded both arguments without wiring a real MSI
            // controller / device-resource grant here.
            Ok(out_len as u64)
        }

        fn shm_create(&self, _c: &CallerContext<'_>, len: usize, _id_out: u64) -> SyscallResult {
            self.record("shm_create");
            // Echo the length so the reachability test can assert the
            // dispatcher decoded the argument without wiring a real region.
            Ok(len as u64)
        }

        fn shm_map(&self, _c: &CallerContext<'_>, handle: u64) -> SyscallResult {
            self.record("shm_map");
            Ok(handle)
        }

        fn shm_unmap(&self, _c: &CallerContext<'_>, _base: u64, _len: usize) -> SyscallResult {
            self.record("shm_unmap");
            Ok(0)
        }

        fn waitset_create(&self, _c: &CallerContext<'_>) -> SyscallResult {
            self.record("waitset_create");
            // Echo a handle so the reachability test sees a non-error result.
            Ok(1)
        }

        fn waitset_ctl(
            &self,
            _c: &CallerContext<'_>,
            set: u64,
            _op: u32,
            _kind: u32,
            _id: u64,
            _token: u64,
        ) -> SyscallResult {
            self.record("waitset_ctl");
            // Echo the set handle so the test can assert the dispatcher decoded
            // the arguments without wiring a real wait-set.
            Ok(set)
        }

        fn waitset_wait(
            &self,
            _c: &CallerContext<'_>,
            _set: u64,
            _timeout_ns: u64,
            _token_out: u64,
        ) -> SyscallResult {
            self.record("waitset_wait");
            Ok(0)
        }

        fn fs_open(
            &self,
            _c: &CallerContext<'_>,
            _path: u64,
            _path_len: usize,
            _flags: OpenFlags,
        ) -> SyscallResult {
            self.record("fs_open");
            Ok(4)
        }

        fn fs_close(&self, _c: &CallerContext<'_>, _fd: u32) -> SyscallResult {
            self.record("fs_close");
            Ok(0)
        }

        fn fs_read(
            &self,
            _c: &CallerContext<'_>,
            _fd: u32,
            _offset: u64,
            _buf: u64,
            _len: usize,
        ) -> SyscallResult {
            self.record("fs_read");
            Ok(0)
        }

        fn fs_write(
            &self,
            _c: &CallerContext<'_>,
            _fd: u32,
            _offset: u64,
            _buf: u64,
            _len: usize,
        ) -> SyscallResult {
            self.record("fs_write");
            Ok(0)
        }

        fn fs_readdir(
            &self,
            _c: &CallerContext<'_>,
            _fd: u32,
            _buf: u64,
            _len: usize,
        ) -> SyscallResult {
            self.record("fs_readdir");
            Ok(0)
        }

        fn fs_stat(
            &self,
            _c: &CallerContext<'_>,
            _fd: u32,
            _out: u64,
            _out_len: usize,
        ) -> SyscallResult {
            self.record("fs_stat");
            Ok(0)
        }

        fn fs_truncate(&self, _c: &CallerContext<'_>, _fd: u32, _size: u64) -> SyscallResult {
            self.record("fs_truncate");
            Ok(0)
        }

        fn fs_sync(&self, _c: &CallerContext<'_>, _fd: u32) -> SyscallResult {
            self.record("fs_sync");
            Ok(0)
        }

        fn fs_mkdir(&self, _c: &CallerContext<'_>, _path: u64, _path_len: usize) -> SyscallResult {
            self.record("fs_mkdir");
            Ok(0)
        }

        fn fs_rename(
            &self,
            _c: &CallerContext<'_>,
            _src: u64,
            _src_len: usize,
            _dst: u64,
            _dst_len: usize,
        ) -> SyscallResult {
            self.record("fs_rename");
            Ok(0)
        }

        fn fs_unlink(&self, _c: &CallerContext<'_>, _path: u64, _path_len: usize) -> SyscallResult {
            self.record("fs_unlink");
            Ok(0)
        }
    }

    #[test]
    fn hash_matches_lib_abi() {
        assert_eq!(verify_table_hash(), Ok(()));
        assert_eq!(sha256(&ENCODED_TABLE), SYSCALL_TABLE_HASH);
    }

    #[test]
    fn every_syscall_is_reachable_with_required_capability() {
        let sink = RecordingSink::new();
        // Hold every capability the abi-v1 table requires.
        let caps = build_caps(
            &[
                CapabilityId::USER_ADMIN,
                CapabilityId::IRQ_BIND,
                CapabilityId::CONSOLE_WRITE,
                CapabilityId::PROC_SPAWN,
                CapabilityId::CONSOLE_READ,
                CapabilityId::USERS_READ,
                CapabilityId::INPUT_INJECT,
                CapabilityId::DISPLAY,
                CapabilityId::INPUT_READ,
                CapabilityId::MMIO_MAP,
                CapabilityId::MEM_DMA,
                CapabilityId::SYSINFO_HW,
                CapabilityId::LOG_EMIT,
                CapabilityId::HW_EMIT,
                CapabilityId::SHM,
                CapabilityId::FS_ACCESS,
                CapabilityId::TIME_SET,
            ],
            &sink,
        );
        let ctx = CallerContext {
            task_id: TaskId(7),
            caps: &caps,
        };
        let h = MockHandlers::default();
        let d = Dispatcher::new(&h, &sink);

        for spec in rustos_abi::SYSCALLS {
            let mut args = RawArgs::ZERO;
            populate_valid_args(spec, &mut args);
            let r = d.dispatch(&ctx, spec.number.as_u16(), args);
            assert!(r.is_ok(), "{} returned {r:?}", spec.name);
        }
    }

    fn populate_valid_args(spec: &SyscallSpec, args: &mut RawArgs) {
        for i in 0..spec.arg_count as usize {
            args.0[i] = match spec.args[i] {
                AbiType::U32 | AbiType::U64 | AbiType::Handle | AbiType::IpcEndpoint => 1,
                AbiType::Cap => u64::from(CapabilityId::FS_MOUNT.as_u16()),
                AbiType::UserPtr => 0x1000,
                AbiType::Len => 64,
                AbiType::I32 | AbiType::Unit | AbiType::Errno => 0,
            };
        }
    }

    #[test]
    fn missing_capability_is_refused_and_audited() {
        let sink = RecordingSink::new();
        let caps = build_caps(&[], &sink); // empty effective set
        let ctx = CallerContext {
            task_id: TaskId(7),
            caps: &caps,
        };
        let h = MockHandlers::default();
        let d = Dispatcher::new(&h, &sink);

        let mut args = RawArgs::ZERO;
        args.0[0] = 1; // handle
        args.0[1] = u64::from(CapabilityId::FS_MOUNT.as_u16());
        let r = d.dispatch(&ctx, SyscallNumber::CAP_REVOKE.as_u16(), args);
        assert_eq!(r, Err(Errno::PermissionDenied));
        // Handler must NOT have been invoked.
        assert_eq!(h.last(), None);
        // Exactly one denied event.
        assert_eq!(sink.ids(), [AuditEvent::SyscallPermissionDenied.id().0]);
    }

    #[test]
    fn audit_records_carry_the_callers_attested_proc_id() {
        use rustos_abi::{ProcId, PROC_ID_HEX_LEN};

        /// Sink that captures the value of the `proc` field of each event.
        struct ProcFieldSink {
            seen: RefCell<Vec<alloc::string::String>>,
        }
        impl Sink for ProcFieldSink {
            fn write_event(&self, event: &Event<'_>) {
                for f in event.fields {
                    if f.key == "proc" {
                        self.seen.borrow_mut().push(f.value.into());
                    }
                }
            }
        }
        set_max_level(Level::Trace);
        let sink = ProcFieldSink {
            seen: RefCell::new(Vec::new()),
        };

        // The attested identity lives on the capability record, minted
        // kernel-side; it is unrelated to the numeric task id.
        let minted = ProcId::from_raw([0xAB; 16]);
        let caps = TaskCapabilities::derive(
            TaskId(7),
            UserId(1000),
            CapabilitySet::empty(),
            CapabilitySet::empty(),
            &sink,
        )
        .with_proc_id(minted);
        let ctx = CallerContext {
            task_id: TaskId(7),
            caps: &caps,
        };
        let h = MockHandlers::default();
        let d = Dispatcher::new(&h, &sink);

        // A capability-gated syscall the empty set cannot satisfy → denied,
        // which emits an audited record carrying the `proc` field.
        let mut args = RawArgs::ZERO;
        args.0[0] = 1;
        args.0[1] = u64::from(CapabilityId::FS_MOUNT.as_u16());
        assert_eq!(
            d.dispatch(&ctx, SyscallNumber::CAP_REVOKE.as_u16(), args),
            Err(Errno::PermissionDenied)
        );

        let mut expected = [0u8; PROC_ID_HEX_LEN];
        let expected = minted.write_hex(&mut expected);
        let seen = sink.seen.borrow();
        assert_eq!(seen.len(), 1, "exactly the one denied record");
        assert_eq!(seen[0], expected);
        // The attestation comes from the caps record, not the task id: a
        // record whose numeric task id is 7 still carries the minted id, not
        // a value derived from `7`.
        assert_ne!(seen[0], "0000000000000007");
    }

    #[test]
    fn unknown_syscall_is_rejected_with_audit() {
        let sink = RecordingSink::new();
        let caps = build_caps(&[], &sink);
        let ctx = CallerContext {
            task_id: TaskId(7),
            caps: &caps,
        };
        let h = MockHandlers::default();
        let d = Dispatcher::new(&h, &sink);

        // Past MAX = OutOfRange.
        assert_eq!(
            d.dispatch(&ctx, SyscallNumber::MAX + 1, RawArgs::ZERO),
            Err(Errno::OutOfRange)
        );
        // In range but unassigned = NotFound.
        let unassigned = u16::try_from(rustos_abi::SYSCALLS.len()).unwrap();
        assert_eq!(
            d.dispatch(&ctx, unassigned, RawArgs::ZERO),
            Err(Errno::NotFound)
        );
        assert_eq!(h.last(), None);
        // Two SyscallUnknown audit records.
        let ids = sink.ids();
        assert_eq!(ids.len(), 2);
        for id in ids {
            assert_eq!(id, AuditEvent::SyscallUnknown.id().0);
        }
    }

    #[test]
    fn trailing_argument_must_be_zero() {
        let sink = RecordingSink::new();
        let caps = build_caps(&[], &sink);
        let ctx = CallerContext {
            task_id: TaskId(7),
            caps: &caps,
        };
        let h = MockHandlers::default();
        let d = Dispatcher::new(&h, &sink);

        // YIELD declares zero arguments; smuggling 1 in slot 1 must fail.
        let mut args = RawArgs::ZERO;
        args.0[1] = 1;
        assert_eq!(
            d.dispatch(&ctx, SyscallNumber::YIELD.as_u16(), args),
            Err(Errno::LengthOutOfRange)
        );
        assert_eq!(h.last(), None);
        assert_eq!(sink.ids(), [AuditEvent::SyscallBadArguments.id().0]);
    }

    #[test]
    fn user_ptr_null_is_rejected() {
        let sink = RecordingSink::new();
        let caps = build_caps(&[], &sink);
        let ctx = CallerContext {
            task_id: TaskId(7),
            caps: &caps,
        };
        let h = MockHandlers::default();
        let d = Dispatcher::new(&h, &sink);

        let mut args = RawArgs::ZERO;
        args.0[0] = 1; // endpoint
        args.0[1] = 0; // user ptr — null, must be refused
        args.0[2] = 8; // len
        assert_eq!(
            d.dispatch(&ctx, SyscallNumber::IPC_SEND.as_u16(), args),
            Err(Errno::BadAlignment)
        );
        assert_eq!(h.last(), None);
    }

    #[test]
    fn u32_argument_high_bits_must_be_zero() {
        let sink = RecordingSink::new();
        let caps = build_caps(&[], &sink);
        let ctx = CallerContext {
            task_id: TaskId(7),
            caps: &caps,
        };
        let h = MockHandlers::default();
        let d = Dispatcher::new(&h, &sink);

        // No abi-v1 syscall takes a U32 today; exercise the validator
        // through cap_query's Cap argument, which forbids the high
        // bits set (raw >> 16 != 0).
        let mut args = RawArgs::ZERO;
        args.0[0] = 1u64 << 20; // out of cap-id range
        assert_eq!(
            d.dispatch(&ctx, SyscallNumber::CAP_QUERY.as_u16(), args),
            Err(Errno::OutOfRange)
        );
        assert_eq!(h.last(), None);
    }

    #[test]
    fn i32_argument_must_be_sign_extended() {
        let sink = RecordingSink::new();
        let caps = build_caps(&[], &sink);
        let ctx = CallerContext {
            task_id: TaskId(7),
            caps: &caps,
        };
        let h = MockHandlers::default();
        let d = Dispatcher::new(&h, &sink);

        // -1 properly sign-extended.
        let mut args = RawArgs::ZERO;
        args.0[0] = u64::MAX;
        assert!(d.dispatch(&ctx, SyscallNumber::EXIT.as_u16(), args).is_ok());

        // High bits set without negative low — invalid.
        let mut bad = RawArgs::ZERO;
        bad.0[0] = 0x1_0000_0000;
        assert_eq!(
            d.dispatch(&ctx, SyscallNumber::EXIT.as_u16(), bad),
            Err(Errno::OutOfRange)
        );
    }

    #[test]
    fn handler_error_is_audited_for_security_relevant_calls() {
        let sink = RecordingSink::new();
        let caps = build_caps(&[], &sink);
        let ctx = CallerContext {
            task_id: TaskId(7),
            caps: &caps,
        };
        let h = MockHandlers {
            force_err: Some(Errno::NotFound),
            ..Default::default()
        };
        let d = Dispatcher::new(&h, &sink);
        let mut args = RawArgs::ZERO;
        args.0[0] = 1;
        args.0[1] = 0x2000;
        args.0[2] = 4;
        assert_eq!(
            d.dispatch(&ctx, SyscallNumber::IPC_SEND.as_u16(), args),
            Err(Errno::NotFound)
        );
        assert_eq!(sink.ids(), [AuditEvent::SyscallHandlerRejected.id().0]);
    }

    #[test]
    fn a_would_block_outcome_is_audited_as_pending_not_rejected() {
        // `WouldBlock` is the `abi-v1` "nothing yet, retry" signal, not a
        // rejection: an audited syscall returning it must emit the benign,
        // below-error `SyscallHandlerWouldBlock` (id 5005) rather than the
        // ERROR-level `SyscallHandlerRejected` (id 5004) a genuine refusal
        // gets — otherwise a caller that legitimately polls while pending
        // (e.g. `login` reading `users_db_read` while the encrypted root
        // unlocks) floods the boot log with errors.
        let sink = RecordingSink::new();
        let caps = build_caps(&[], &sink);
        let ctx = CallerContext {
            task_id: TaskId(7),
            caps: &caps,
        };
        let h = MockHandlers {
            force_err: Some(Errno::WouldBlock),
            ..Default::default()
        };
        let d = Dispatcher::new(&h, &sink);
        let mut args = RawArgs::ZERO;
        args.0[0] = 1;
        args.0[1] = 0x2000;
        args.0[2] = 4;
        assert_eq!(
            d.dispatch(&ctx, SyscallNumber::IPC_SEND.as_u16(), args),
            Err(Errno::WouldBlock)
        );
        assert_eq!(sink.ids(), [AuditEvent::SyscallHandlerWouldBlock.id().0]);
    }

    #[test]
    fn observers_do_not_emit_invoked_audit_records() {
        let sink = RecordingSink::new();
        let caps = build_caps(&[], &sink);
        let ctx = CallerContext {
            task_id: TaskId(7),
            caps: &caps,
        };
        let h = MockHandlers::default();
        let d = Dispatcher::new(&h, &sink);

        for &n in &[
            SyscallNumber::YIELD,
            SyscallNumber::CAP_QUERY,
            SyscallNumber::CLOCK_GET,
        ] {
            let mut args = RawArgs::ZERO;
            if n == SyscallNumber::CAP_QUERY {
                args.0[0] = u64::from(CapabilityId::FS_MOUNT.as_u16());
            }
            assert!(d.dispatch(&ctx, n.as_u16(), args).is_ok());
        }
        // No security-relevant audit traffic.
        assert!(sink.ids().is_empty());
    }

    #[test]
    fn audited_call_emits_invoked_on_success() {
        let sink = RecordingSink::new();
        let caps = build_caps(&[], &sink);
        let ctx = CallerContext {
            task_id: TaskId(7),
            caps: &caps,
        };
        let h = MockHandlers::default();
        let d = Dispatcher::new(&h, &sink);
        let mut args = RawArgs::ZERO;
        args.0[0] = 0xDEAD_BEEF;
        assert!(d
            .dispatch(&ctx, SyscallNumber::EXIT.as_u16(), {
                let mut a = RawArgs::ZERO;
                a.0[0] = 0u64.wrapping_sub(2); // -2 sign-extended
                a
            })
            .is_ok());
        let _ = args;
        assert_eq!(sink.ids(), [AuditEvent::SyscallInvoked.id().0]);
    }

    #[test]
    fn irq_bind_without_capability_is_refused_and_audited() {
        // Without `CAP_IRQ_BIND` the dispatcher must short-circuit on
        // the capability check (step 2), refuse with
        // PermissionDenied, never call the handler, and emit exactly
        // one denied audit record.
        let sink = RecordingSink::new();
        let caps = build_caps(&[], &sink);
        let ctx = CallerContext {
            task_id: TaskId(7),
            caps: &caps,
        };
        let h = MockHandlers::default();
        let d = Dispatcher::new(&h, &sink);

        let mut args = RawArgs::ZERO;
        args.0[0] = 42; // line — well-typed
        assert_eq!(
            d.dispatch(&ctx, SyscallNumber::IRQ_BIND.as_u16(), args),
            Err(Errno::PermissionDenied)
        );
        assert_eq!(h.last(), None);
        assert_eq!(sink.ids(), [AuditEvent::SyscallPermissionDenied.id().0]);
    }

    #[test]
    fn irq_bind_with_capability_reaches_handler_and_audits_invocation() {
        // With `CAP_IRQ_BIND` granted the dispatcher decodes the `u32`
        // line argument, calls `irq_bind`, and emits a single
        // SyscallInvoked record (the spec row sets `audit: true`).
        // The Mock impl fabricates a handle of `line | top-nibble`;
        // assert the dispatcher returned that verbatim so we know the
        // narrowing decode is correct.
        let sink = RecordingSink::new();
        let caps = build_caps(&[CapabilityId::IRQ_BIND], &sink);
        let ctx = CallerContext {
            task_id: TaskId(7),
            caps: &caps,
        };
        let h = MockHandlers::default();
        let d = Dispatcher::new(&h, &sink);

        let mut args = RawArgs::ZERO;
        args.0[0] = 17;
        let r = d.dispatch(&ctx, SyscallNumber::IRQ_BIND.as_u16(), args);
        assert_eq!(r, Ok(0x11 | 0xF000_0000_0000_0000));
        assert_eq!(h.last(), Some("irq_bind"));
        assert_eq!(sink.ids(), [AuditEvent::SyscallInvoked.id().0]);
    }

    #[test]
    fn irq_bind_rejects_line_argument_with_high_bits_set() {
        // The `irq_bind` spec row declares its single argument as
        // `U32`; the dispatcher's per-arg validator must refuse a
        // value whose upper 32 bits are non-zero before the handler
        // is reached, with `Errno::OutOfRange` and a bad-arguments
        // audit record.
        let sink = RecordingSink::new();
        let caps = build_caps(&[CapabilityId::IRQ_BIND], &sink);
        let ctx = CallerContext {
            task_id: TaskId(7),
            caps: &caps,
        };
        let h = MockHandlers::default();
        let d = Dispatcher::new(&h, &sink);

        let mut args = RawArgs::ZERO;
        args.0[0] = 1u64 << 40; // high bits set — invalid `U32`
        assert_eq!(
            d.dispatch(&ctx, SyscallNumber::IRQ_BIND.as_u16(), args),
            Err(Errno::OutOfRange)
        );
        assert_eq!(h.last(), None);
        assert_eq!(sink.ids(), [AuditEvent::SyscallBadArguments.id().0]);
    }

    #[test]
    fn irq_wait_passes_handle_and_timeout_verbatim() {
        // `irq_wait` carries `(Handle, U64)`; both slots accept any
        // 64-bit value verbatim. The dispatcher must forward both
        // arguments to the handler unchanged and emit no
        // `SyscallInvoked` record (the spec row sets `audit: false`).
        let sink = RecordingSink::new();
        let caps = build_caps(&[CapabilityId::IRQ_BIND], &sink);
        let ctx = CallerContext {
            task_id: TaskId(7),
            caps: &caps,
        };
        let h = MockHandlers::default();
        let d = Dispatcher::new(&h, &sink);

        let mut args = RawArgs::ZERO;
        args.0[0] = 0xCAFE_F00D_DEAD_BEEF; // handle
        args.0[1] = 1_000_000_000; // timeout_ns
        assert!(d
            .dispatch(&ctx, SyscallNumber::IRQ_WAIT.as_u16(), args)
            .is_ok());
        assert_eq!(h.last(), Some("irq_wait"));
        assert!(sink.ids().is_empty(), "irq_wait must not audit on success");
    }

    #[test]
    fn mem_map_decodes_len_flags_and_addr_hint_and_is_unaudited() {
        // `mem_map` is ungated and unaudited. With a
        // well-typed `(len, flags, addr_hint)` tuple the dispatcher decodes
        // each argument, reaches the handler, and emits no audit record.
        let sink = RecordingSink::new();
        let caps = build_caps(&[], &sink); // no capability needed
        let ctx = CallerContext {
            task_id: TaskId(7),
            caps: &caps,
        };
        let h = MockHandlers::default();
        let d = Dispatcher::new(&h, &sink);

        let mut args = RawArgs::ZERO;
        args.0[0] = 0x2000; // len
        args.0[1] = u64::from(MapFlags::FIXED.bits()); // flags
        args.0[2] = 0x10_0000; // addr_hint
        let r = d.dispatch(&ctx, SyscallNumber::MEM_MAP.as_u16(), args);
        // The Mock echoes `len` back as the fabricated base.
        assert_eq!(r, Ok(0x2000));
        assert_eq!(h.last(), Some("mem_map"));
        assert!(sink.ids().is_empty(), "mem_map must not audit");
    }

    #[test]
    fn mem_map_rejects_a_reserved_flag_bit_before_the_handler() {
        // `flags` is declared `U32`; the per-arg validator accepts the
        // 32-bit value, but `MapFlags::from_bits` must reject a reserved
        // bit with `Errno::OutOfRange` before the handler is reached.
        let sink = RecordingSink::new();
        let caps = build_caps(&[], &sink);
        let ctx = CallerContext {
            task_id: TaskId(7),
            caps: &caps,
        };
        let h = MockHandlers::default();
        let d = Dispatcher::new(&h, &sink);

        let mut args = RawArgs::ZERO;
        args.0[0] = 0x1000; // len
        args.0[1] = 1 << 1; // reserved flag bit
        assert_eq!(
            d.dispatch(&ctx, SyscallNumber::MEM_MAP.as_u16(), args),
            Err(Errno::OutOfRange)
        );
        assert_eq!(h.last(), None);
    }

    #[test]
    fn mem_unmap_forwards_base_and_len_unaudited() {
        let sink = RecordingSink::new();
        let caps = build_caps(&[], &sink);
        let ctx = CallerContext {
            task_id: TaskId(7),
            caps: &caps,
        };
        let h = MockHandlers::default();
        let d = Dispatcher::new(&h, &sink);

        let mut args = RawArgs::ZERO;
        args.0[0] = 0x4000; // base
        args.0[1] = 0x2000; // len
        assert!(d
            .dispatch(&ctx, SyscallNumber::MEM_UNMAP.as_u16(), args)
            .is_ok());
        assert_eq!(h.last(), Some("mem_unmap"));
        assert!(sink.ids().is_empty(), "mem_unmap must not audit");
    }

    #[test]
    fn wait_decodes_pid_and_status_and_is_audited() {
        // `wait` is ungated (a process reaps its own children, no
        // capability) but audited — reaping a child is a process-lifecycle
        // state change. With a well-typed
        // `(pid, status)` tuple the dispatcher recovers the `i32` pid,
        // forwards the `status` pointer verbatim, reaches the handler, and
        // emits exactly one `SyscallInvoked` record on success.
        let sink = RecordingSink::new();
        let caps = build_caps(&[], &sink); // no capability needed
        let ctx = CallerContext {
            task_id: TaskId(7),
            caps: &caps,
        };
        let h = MockHandlers::default();
        let d = Dispatcher::new(&h, &sink);

        let mut args = RawArgs::ZERO;
        // pid 5 as a sign-extended `i32` (low 32 bits only).
        args.0[0] = 5;
        args.0[1] = 0x1000; // status — a non-null user pointer
        let r = d.dispatch(&ctx, SyscallNumber::WAIT.as_u16(), args);
        // The Mock echoes the decoded pid back as the fabricated reaped PID.
        assert_eq!(r, Ok(5));
        assert_eq!(h.last(), Some("wait"));
        assert_eq!(sink.ids(), [AuditEvent::SyscallInvoked.id().0]);
    }

    #[test]
    fn wait_recovers_a_negative_pid_and_rejects_a_null_status() {
        let sink = RecordingSink::new();
        let caps = build_caps(&[], &sink);
        let ctx = CallerContext {
            task_id: TaskId(7),
            caps: &caps,
        };
        let h = MockHandlers::default();
        let d = Dispatcher::new(&h, &sink);

        // `WAIT_PID_ANY` (-1) is sign-extended through all 64 bits; the
        // dispatcher must recover it as `i32::-1` and forward it. The Mock
        // echoes the pid back reinterpreted as `u32`, i.e. `u32::MAX`.
        let mut args = RawArgs::ZERO;
        #[allow(clippy::cast_sign_loss)]
        let extended = i64::from(rustos_abi::WAIT_PID_ANY) as u64;
        args.0[0] = extended;
        args.0[1] = 0x1000; // status
        assert_eq!(
            d.dispatch(&ctx, SyscallNumber::WAIT.as_u16(), args),
            Ok(u64::from(u32::MAX))
        );

        // A null `status` pointer is rejected by the per-arg `UserPtr`
        // validator before the handler is reached.
        let h2 = MockHandlers::default();
        let d2 = Dispatcher::new(&h2, &sink);
        let mut bad = RawArgs::ZERO;
        bad.0[0] = 1; // pid
        bad.0[1] = 0; // null status
        assert_eq!(
            d2.dispatch(&ctx, SyscallNumber::WAIT.as_u16(), bad),
            Err(Errno::BadAlignment)
        );
        assert_eq!(h2.last(), None);
    }

    #[test]
    fn rlimit_get_decodes_kind_and_pointer_unaudited() {
        // `rlimit_get` reads the caller's own effective limit: ungated and
        // not audited per call. With a well-typed
        // `(kind, out)` tuple the dispatcher narrows the `u32` kind, forwards
        // the `out` pointer, reaches the handler, and emits no audit record.
        let sink = RecordingSink::new();
        let caps = build_caps(&[], &sink); // no capability needed
        let ctx = CallerContext {
            task_id: TaskId(7),
            caps: &caps,
        };
        let h = MockHandlers::default();
        let d = Dispatcher::new(&h, &sink);

        let mut args = RawArgs::ZERO;
        args.0[0] = 2; // kind
        args.0[1] = 0x1000; // out — a non-null user pointer
        let r = d.dispatch(&ctx, SyscallNumber::RLIMIT_GET.as_u16(), args);
        // The Mock echoes the decoded kind back.
        assert_eq!(r, Ok(2));
        assert_eq!(h.last(), Some("rlimit_get"));
        assert!(sink.ids().is_empty(), "rlimit_get must not audit");
    }

    #[test]
    fn rlimit_set_decodes_kind_and_pointer_and_is_audited() {
        // `rlimit_set` is ungated at the dispatcher (lowering a bound needs
        // no capability; the `CAP_RLIMIT_RAISE` check is fine-grained in the
        // handler) but IS audited — it changes enforced policy.
        let sink = RecordingSink::new();
        let caps = build_caps(&[], &sink);
        let ctx = CallerContext {
            task_id: TaskId(7),
            caps: &caps,
        };
        let h = MockHandlers::default();
        let d = Dispatcher::new(&h, &sink);

        let mut args = RawArgs::ZERO;
        args.0[0] = 3; // kind
        args.0[1] = 0x1000; // value — a non-null user pointer
        let r = d.dispatch(&ctx, SyscallNumber::RLIMIT_SET.as_u16(), args);
        assert_eq!(r, Ok(3));
        assert_eq!(h.last(), Some("rlimit_set"));
        assert_eq!(sink.ids(), [AuditEvent::SyscallInvoked.id().0]);

        // A null pointer is rejected by the per-arg `UserPtr` validator
        // before the handler is reached.
        let h2 = MockHandlers::default();
        let d2 = Dispatcher::new(&h2, &sink);
        let mut bad = RawArgs::ZERO;
        bad.0[0] = 0; // kind
        bad.0[1] = 0; // null pointer
        assert_eq!(
            d2.dispatch(&ctx, SyscallNumber::RLIMIT_SET.as_u16(), bad),
            Err(Errno::BadAlignment)
        );
        assert_eq!(h2.last(), None);
    }

    #[test]
    fn users_db_read_without_capability_is_refused_and_audited() {
        // The credential database is privileged: without `CAP_USERS_READ`
        // the dispatcher refuses before the handler is reached (capability check before state).
        let sink = RecordingSink::new();
        let caps = build_caps(&[], &sink);
        let ctx = CallerContext {
            task_id: TaskId(7),
            caps: &caps,
        };
        let h = MockHandlers::default();
        let d = Dispatcher::new(&h, &sink);

        let mut args = RawArgs::ZERO;
        args.0[0] = 0x2000; // buf
        args.0[1] = 4096; // len
        assert_eq!(
            d.dispatch(&ctx, SyscallNumber::USERS_DB_READ.as_u16(), args),
            Err(Errno::PermissionDenied)
        );
        assert_eq!(h.last(), None);
        assert_eq!(sink.ids(), [AuditEvent::SyscallPermissionDenied.id().0]);
    }

    #[test]
    fn users_db_read_decodes_buf_and_len_and_is_audited() {
        let sink = RecordingSink::new();
        let caps = build_caps(&[CapabilityId::USERS_READ], &sink);
        let ctx = CallerContext {
            task_id: TaskId(7),
            caps: &caps,
        };
        let h = MockHandlers::default();
        let d = Dispatcher::new(&h, &sink);

        let mut args = RawArgs::ZERO;
        args.0[0] = 0x2000; // buf — a non-null user pointer
        args.0[1] = 4096; // len
        let r = d.dispatch(&ctx, SyscallNumber::USERS_DB_READ.as_u16(), args);
        // The Mock echoes the decoded capacity back.
        assert_eq!(r, Ok(4096));
        assert_eq!(h.last(), Some("users_db_read"));
        assert_eq!(sink.ids(), [AuditEvent::SyscallInvoked.id().0]);

        // A null `buf` pointer is rejected by the per-arg `UserPtr`
        // validator before the handler is reached.
        let h2 = MockHandlers::default();
        let d2 = Dispatcher::new(&h2, &sink);
        let mut bad = RawArgs::ZERO;
        bad.0[0] = 0; // null buf
        bad.0[1] = 4096;
        assert_eq!(
            d2.dispatch(&ctx, SyscallNumber::USERS_DB_READ.as_u16(), bad),
            Err(Errno::BadAlignment)
        );
        assert_eq!(h2.last(), None);
    }
}
