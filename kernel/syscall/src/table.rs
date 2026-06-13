//! Generated `abi-v1` dispatch table.
//!
//! Every syscall entering the kernel — from any architecture port —
//! lands in [`Dispatcher::dispatch`]. The dispatcher performs the
//! five steps mandated by `AGENTS.md` §5.4 and forwards the call to
//! the owning subsystem via the [`SyscallHandlers`] trait. The trait
//! is implemented in `kernel/core`'s wiring layer so this crate stays
//! decoupled from `kernel/ipc`, `kernel/sched`, and friends
//! (`AGENTS.md` §2.3 — no bloat).

use rustos_abi::{
    spec_for, AbiType, CapabilityId, Errno, IrqHandle, MapFlags, RandomFlags, SyscallNumber,
    SyscallSpec, ENCODED_TABLE, SYSCALL_MAX_ARGS,
};
use rustos_crypto::{sha256, Sha256Digest};
use rustos_kernel_sec::{TaskCapabilities, TaskId};
use rustos_log::{Field, Sink};
use rustos_util::fmt::{format_hex_u64, format_i32};

use crate::audit::{record, AuditEvent};

/// SHA-256 fingerprint of [`rustos_abi::ENCODED_TABLE`].
///
/// The value is **derived at build time** by this crate's `build.rs`
/// from `rustos_abi::ENCODED_TABLE` — the single source of truth
/// (`AGENTS.md` §9, §2.2) — and `include!`d here. There is no
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
// at the call site rather than silently desync the ABI. `AGENTS.md`
// §2.4 (no interface creep) — this is a compile-time invariant
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
    /// the calling task so the handle cannot be forged
    /// (`AGENTS.md` §5.2, §5.4).
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
    /// secure random bytes drawn from the kernel output reserve
    /// (`AGENTS.md` §22), returning the number of bytes written.
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
    /// written (`AGENTS.md` §20).
    ///
    /// The dispatcher has already checked the caller holds
    /// [`CapabilityId::CONSOLE_WRITE`], that `buf` is non-null, that `fd`
    /// fits in `u32`, and that `len` fits in `usize`. The implementation
    /// resolves `fd` against the caller's per-process descriptor table
    /// (`rustos_abi::DescriptorTable`): an `fd` that is not a writable
    /// inherited stream fails closed (`AGENTS.md` §5.4 / §20 — the
    /// descriptor, not an ambient device, is the authority). It then
    /// copies the buffer through the validated `copy_from_user` boundary
    /// (`AGENTS.md` §5.4) and emits it to that descriptor's kernel stream
    /// backing — in the bootstrap session the discovered console (the
    /// detected framebuffer when present, else the first discovered UART,
    /// `plans/PI.md` P6). A build with no backing wired must fail closed
    /// with [`Errno::NotImplemented`] rather than silently discarding the
    /// bytes (`AGENTS.md` §2.9).
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
    /// through the validated `copy_from_user` boundary (`AGENTS.md`
    /// §5.4), looks it up in the kernel's embedded-program registry,
    /// builds a fresh hardware-isolated address space for it (§4),
    /// registers it as a runnable process, and returns its PID; the
    /// caller keeps running (`plans/SPAWN.md` SP3 — a true concurrent
    /// spawn, not an `exec`-style hand-off). `console` selects the
    /// child's standard-stream attachment (`AGENTS.md` §20):
    /// [`rustos_abi::CONSOLE_INHERIT`] attaches the child to the
    /// caller's own descriptor table, any other value names an
    /// installed console index and the implementation must fail closed
    /// with [`Errno::NotFound`] when no console is installed at it. A
    /// build with no spawn service wired must fail closed with
    /// [`Errno::NotImplemented`], and a path naming no registered
    /// program with [`Errno::NotFound`], rather than silently doing
    /// nothing (`AGENTS.md` §2.9).
    fn spawn(
        &self,
        caller: &CallerContext<'_>,
        path: u64,
        path_len: usize,
        console: u64,
    ) -> SyscallResult;
    /// Read up to `len` bytes from the calling process's standard stream
    /// `fd` into the user buffer at `buf`, returning the number of bytes
    /// read (`AGENTS.md` §20).
    ///
    /// The dispatcher has already checked the caller holds
    /// [`CapabilityId::CONSOLE_READ`], that `buf` is non-null, that `fd`
    /// fits in `u32`, and that `len` fits in `usize`. The implementation
    /// resolves `fd` against the caller's per-process descriptor table
    /// (`rustos_abi::DescriptorTable`): an `fd` that is not a readable
    /// inherited stream fails closed (`AGENTS.md` §5.4 / §20). It then
    /// reads from that descriptor's kernel stream backing — in the
    /// bootstrap session the first discovered keyboard/UART input source
    /// (`plans/PI.md` P6) — into a bounded kernel staging buffer and
    /// copies it out through the validated `copy_to_user` boundary
    /// (`AGENTS.md` §5.4). A short read (fewer bytes than `len`, possibly
    /// zero when no input is pending) is valid, so the caller loops. A
    /// build with no backing wired must fail closed with
    /// [`Errno::NotImplemented`] rather than fabricating input
    /// (`AGENTS.md` §2.9).
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
    /// caller's **own** hardware-isolated address space (`AGENTS.md` §4 —
    /// no global user heap, no cross-process mapping), zeroes it before it
    /// is visible, and never makes it executable (`AGENTS.md` §19.2 — W^X).
    /// A frame- or page-table-allocation failure must return
    /// [`Errno::OutOfMemory`] rather than panicking (`AGENTS.md` §4 / §2.9);
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
    /// frames it reclaims (`AGENTS.md` §4 — secret hygiene) and fails closed
    /// when `(base, len)` does not name a region the caller mapped
    /// (`AGENTS.md` §5.4). A build with no memory service wired must fail
    /// closed with [`Errno::NotImplemented`]; a zero `len` is rejected with
    /// [`Errno::LengthOutOfRange`]. Returns `Ok(0)` on success.
    fn mem_unmap(&self, caller: &CallerContext<'_>, base: u64, len: usize) -> SyscallResult;
    /// Wait for a child of the calling process to exit, reaping it and
    /// writing its exit code to the user `status` pointer; returns the
    /// reaped child's PID (`plans/SPAWN.md` SP6).
    ///
    /// The dispatcher has already validated that `pid` is a sign-extended
    /// `i32` and that `status` is a non-null `UserPtr`. `pid` is either a
    /// specific child's PID or [`rustos_abi::WAIT_ANY`] (wait for any
    /// child). The implementation validates the parent/child relationship —
    /// a process may only reap its **own** children (`AGENTS.md` §4 / §5.4)
    /// — blocks the caller until a child is reapable, and copies the exit
    /// code out through the validated `copy_to_user` boundary. A `pid` that
    /// is not a child of the caller must fail closed with
    /// [`Errno::NotFound`]; a build with no process-wait service wired must
    /// fail closed with [`Errno::NotImplemented`] rather than fabricating a
    /// reaped child (`AGENTS.md` §2.9).
    fn wait(&self, caller: &CallerContext<'_>, pid: i32, status: u64) -> SyscallResult;

    /// Read the calling task's effective limit for resource `kind`, writing
    /// the encoded [`rustos_abi::ResourceLimit`] to the user `out` pointer
    /// (`AGENTS.md` §24.3).
    ///
    /// The dispatcher has already validated that `kind` fits in a `u32`
    /// (upper bits zero) and that `out` is a non-null `UserPtr`. The
    /// implementation validates `kind` against [`rustos_abi::LimitKind`] and
    /// fails closed on an unassigned value (`AGENTS.md` §5.4 — validate every
    /// input). Returns `Ok(0)` on success.
    ///
    /// The default implementation fails closed with [`Errno::NotImplemented`]
    /// (`AGENTS.md` §2.9): a kernel build with no resource-limit service
    /// wired never fabricates a limit. The enforcement is installed in
    /// `kernel/core`.
    fn rlimit_get(&self, _caller: &CallerContext<'_>, _kind: u32, _out: u64) -> SyscallResult {
        Err(Errno::NotImplemented)
    }

    /// Install the calling task's limit for resource `kind` from the encoded
    /// [`rustos_abi::ResourceLimit`] at the user `value` pointer (`AGENTS.md`
    /// §24.3).
    ///
    /// The dispatcher has already validated that `kind` fits in a `u32` and
    /// that `value` is a non-null `UserPtr`. The implementation copies the
    /// limit in through the validated `copy_from_user` boundary, validates
    /// `kind` and the soft/hard pair, and — when the request would *raise* a
    /// hard bound above the inherited ceiling — refuses with
    /// [`Errno::PermissionDenied`] unless the caller holds
    /// [`rustos_abi::CapabilityId::RLIMIT_RAISE`] (§24.3). Returns `Ok(0)` on
    /// success.
    ///
    /// The default implementation fails closed with [`Errno::NotImplemented`]
    /// (`AGENTS.md` §2.9); the enforcement is installed in `kernel/core`.
    fn rlimit_set(&self, _caller: &CallerContext<'_>, _kind: u32, _value: u64) -> SyscallResult {
        Err(Errno::NotImplemented)
    }

    /// Copy the system user database (`/System/Security/Users`) the kernel
    /// loaded at boot out to the user buffer at `buf` (`AGENTS.md` §5.1,
    /// `plans/PI.md` P11).
    ///
    /// The dispatcher has already checked
    /// [`rustos_abi::CapabilityId::USERS_READ`] and that `buf` is a
    /// non-null `UserPtr`. The implementation bounds `len`, copies the
    /// database's exact `users-v1` text through the validated
    /// `copy_to_user` boundary (`AGENTS.md` §5.4), and returns the byte
    /// count. A buffer smaller than the database must fail closed with
    /// [`Errno::BufferTooSmall`] — a credential database is never
    /// truncated (`AGENTS.md` §2.9); a kernel holding no database must
    /// fail closed with [`Errno::NotFound`].
    ///
    /// The default implementation fails closed with
    /// [`Errno::NotImplemented`] (`AGENTS.md` §2.9): a kernel build with
    /// no users-database service wired never fabricates accounts. The
    /// service is installed in `kernel/core`.
    fn users_db_read(&self, _caller: &CallerContext<'_>, _buf: u64, _len: usize) -> SyscallResult {
        Err(Errno::NotImplemented)
    }

    /// Report how many system text consoles are installed (`AGENTS.md`
    /// §20, `plans/PI.md` P11).
    ///
    /// The dispatcher has already checked the caller holds
    /// [`CapabilityId::CONSOLE_WRITE`]. The implementation returns the
    /// length of the boot-installed console list — the index space the
    /// `spawn` syscall's `console` argument selects from. PID 1 `init`
    /// uses it to start one login session per discovered console.
    ///
    /// The default implementation fails closed with
    /// [`Errno::NotImplemented`] (`AGENTS.md` §2.9): a kernel build with
    /// no console list wired never fabricates a console topology. The
    /// real count is installed in `kernel/core`.
    fn console_count(&self, _caller: &CallerContext<'_>) -> SyscallResult {
        Err(Errno::NotImplemented)
    }

    /// Set whether one of the calling process's inherited input streams
    /// echoes the bytes it reads back to its console (`AGENTS.md` §20,
    /// `plans/PI.md` P11 — terminal local echo).
    ///
    /// The dispatcher has already checked the caller holds
    /// [`CapabilityId::CONSOLE_READ`]. `fd` must be a readable inherited
    /// stream and `enabled` is the ABI's `0`-disables/non-zero-enables
    /// flag. The implementation toggles the resolved console's echo flag;
    /// login disables echo around a password read so the secret is never
    /// rendered, then restores it (`AGENTS.md` §5.4 — never echo a
    /// credential).
    ///
    /// The default implementation fails closed with
    /// [`Errno::NotImplemented`] (`AGENTS.md` §2.9): a kernel build with
    /// no console list wired has no echo to toggle. The real handler is
    /// installed in `kernel/core`.
    fn stream_echo(&self, _caller: &CallerContext<'_>, _fd: u32, _enabled: u32) -> SyscallResult {
        Err(Errno::NotImplemented)
    }

    /// Inject one decoded keyboard *key edge* into the kernel input-focus
    /// arbiter (`AGENTS.md` §20, `plans/PI.md` P11 — input follows the
    /// surface owner).
    ///
    /// The dispatcher has already checked the caller holds
    /// [`CapabilityId::INPUT_INJECT`] and that `buf` is a non-null
    /// `UserPtr`. The implementation copies up to `len` bytes in through
    /// the validated `copy_from_user` boundary (`AGENTS.md` §5.4), decodes
    /// one [`rustos_abi::input::KeyInput`] record fail-closed, and hands it
    /// to the arbiter, which decides the encoding and destination by who
    /// holds focus: with the text console foreground it encodes the press
    /// to console (tty) bytes and enqueues them on the focused console's
    /// input queue; with the desktop foreground it routes the record to the
    /// kernel keyboard channel. The driver no longer chooses the encoding
    /// or destination (`AGENTS.md` §17.4). Returns the number of bytes
    /// consumed from the record.
    ///
    /// The default implementation fails closed with
    /// [`Errno::NotImplemented`] (`AGENTS.md` §2.9): a kernel build with no
    /// input-focus arbiter wired has nowhere to route the edge. The real
    /// handler is installed in `kernel/core`.
    fn key_inject(&self, _caller: &CallerContext<'_>, _buf: u64, _len: usize) -> SyscallResult {
        Err(Errno::NotImplemented)
    }

    /// Acquire ownership of the display and claim keyboard input focus
    /// (`AGENTS.md` §10, §17.3; `plans/PI.md` P11 — input follows the
    /// surface owner).
    ///
    /// The dispatcher has already checked the caller holds
    /// [`CapabilityId::DISPLAY`]. The implementation switches the
    /// input-focus arbiter's foreground to the desktop keyboard channel, so
    /// subsequently injected key edges ([`Self::key_inject`]) are delivered
    /// as records the display owner drains with [`Self::keyboard_read`].
    ///
    /// The default implementation fails closed with
    /// [`Errno::NotImplemented`] (`AGENTS.md` §2.9): a build with no
    /// arbiter wired owns no display to acquire. The real handler is
    /// installed in `kernel/core`.
    fn display_acquire(&self, _caller: &CallerContext<'_>) -> SyscallResult {
        Err(Errno::NotImplemented)
    }

    /// Release the display and return keyboard input focus to the text
    /// console (`AGENTS.md` §10, §17.3; `plans/PI.md` P11).
    ///
    /// The inverse of [`Self::display_acquire`]; the dispatcher has already
    /// checked the caller holds [`CapabilityId::DISPLAY`]. The default
    /// implementation fails closed with [`Errno::NotImplemented`].
    fn display_release(&self, _caller: &CallerContext<'_>) -> SyscallResult {
        Err(Errno::NotImplemented)
    }

    /// Read one decoded keyboard event from the kernel keyboard channel
    /// (`AGENTS.md` §10; `plans/PI.md` P11 — keyboard input for the
    /// desktop).
    ///
    /// The dispatcher has already checked the caller holds
    /// [`CapabilityId::INPUT_READ`] and that `buf` is a non-null `UserPtr`.
    /// The implementation drains one [`rustos_abi::input::KeyInput`] record
    /// the arbiter routed to the channel into `buf` (at least
    /// [`rustos_abi::input::KeyInput::WIRE_LEN`] bytes), copies it out
    /// through the validated boundary (`AGENTS.md` §5.4), and returns the
    /// number of bytes written — or `0` when the channel is drained.
    ///
    /// The default implementation fails closed with
    /// [`Errno::NotImplemented`] (`AGENTS.md` §2.9): a build with no
    /// arbiter wired has no channel to drain. The real handler is installed
    /// in `kernel/core`.
    fn keyboard_read(&self, _caller: &CallerContext<'_>, _buf: u64, _len: usize) -> SyscallResult {
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

    /// Run the full §5.4 sequence for one syscall and return its
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

        // §5.4 step 2: capability check.
        if let Some(required) = spec.required_capability {
            if !caller.caps.has(required) {
                self.audit_denied(caller, spec);
                return Err(Errno::PermissionDenied);
            }
        }

        // §5.4 step 3: argument validation. Trailing slots must be
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

        // §5.4 step 4: dispatch.
        let outcome = self.invoke(caller, spec, &args);

        // §5.4 step 5: audit emission for security-relevant calls.
        match &outcome {
            Ok(_) if spec.audit => self.audit_invoked(caller, spec),
            Err(_) if spec.audit => self.audit_rejected(caller, spec, outcome.as_ref().err()),
            _ => {}
        }
        outcome
    }

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
            _ => Err(Errno::NotFound),
        }
    }

    fn audit_unknown(&self, caller: &CallerContext<'_>, number: u16) {
        let mut t = [0u8; 16];
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
                    key: "no",
                    value: rustos_util::fmt::format_usize(usize::from(number), &mut n),
                },
            ],
        );
    }

    fn audit_denied(&self, caller: &CallerContext<'_>, spec: &SyscallSpec) {
        let mut t = [0u8; 16];
        record(
            self.audit,
            AuditEvent::SyscallPermissionDenied,
            &[
                Field {
                    key: "task",
                    value: format_hex_u64(caller.task_id.0, &mut t),
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
        record(
            self.audit,
            AuditEvent::SyscallBadArguments,
            &[
                Field {
                    key: "task",
                    value: format_hex_u64(caller.task_id.0, &mut t),
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
        record(
            self.audit,
            AuditEvent::SyscallInvoked,
            &[
                Field {
                    key: "task",
                    value: format_hex_u64(caller.task_id.0, &mut t),
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
        record(
            self.audit,
            AuditEvent::SyscallHandlerRejected,
            &[
                Field {
                    key: "task",
                    value: format_hex_u64(caller.task_id.0, &mut t),
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
/// place rather than at every call site (`AGENTS.md` §2.2).
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
        // the capability check (AGENTS.md §5.4 step 2), refuse with
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
        // `mem_map` is ungated and unaudited (`AGENTS.md` §16.6). With a
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
        // state change (`AGENTS.md` §5.4.4). With a well-typed
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

        // `WAIT_ANY` (-1) is sign-extended through all 64 bits; the
        // dispatcher must recover it as `i32::-1` and forward it. The Mock
        // echoes the pid back reinterpreted as `u32`, i.e. `u32::MAX`.
        let mut args = RawArgs::ZERO;
        #[allow(clippy::cast_sign_loss)]
        let extended = i64::from(rustos_abi::WAIT_ANY) as u64;
        args.0[0] = extended;
        args.0[1] = 0x1000; // status
        assert_eq!(
            d.dispatch(&ctx, SyscallNumber::WAIT.as_u16(), args),
            Ok(u64::from(u32::MAX))
        );

        // A null `status` pointer is rejected by the per-arg `UserPtr`
        // validator before the handler is reached (`AGENTS.md` §5.4).
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
        // not audited per call (`AGENTS.md` §24.3). With a well-typed
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
        // handler) but IS audited — it changes enforced policy (§24.3).
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
        // before the handler is reached (`AGENTS.md` §5.4).
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
        // the dispatcher refuses before the handler is reached (`AGENTS.md`
        // §5.4 — capability check before state).
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
        // validator before the handler is reached (`AGENTS.md` §5.4).
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
