//! Generated `abi-v1` dispatch table.
//!
//! Every syscall entering the kernel — from any architecture port —
//! lands in [`Dispatcher::dispatch`]. The dispatcher performs the
//! five steps mandated by and forwards the call to
//! the owning subsystem via the [`SyscallHandlers`] trait. The trait
//! is implemented in `kernel/core`'s wiring layer so this crate stays
//! decoupled from `kernel/ipc`, `kernel/sched`, and friends
//! (no bloat).

use tairix_abi::seat::ReleaseSurface;
use tairix_abi::{
    spec_for, AbiType, CallRecvFlags, CapabilityId, Errno, IrqHandle, LinkFlags, MapFlags,
    OpenFlags, PowerAction, RandomFlags, RealpathMode, SchedPriority, Signal, SignalIntakeOp,
    SyscallNumber, SyscallSpec, UnlinkFlags, WaitFlags, ENCODED_TABLE, FS_ATTR_KEY_MAX,
    FS_ATTR_VALUE_MAX, FS_MODE_MASK, PROC_ID_HEX_LEN, SYSCALL_MAX_ARGS,
};
use tairix_crypto::{sha256, Sha256Digest};
use tairix_kernel_sec::{ProcessId, TaskCapabilities, TaskId};
use tairix_log::{Field, Sink};
use tairix_util::fmt::{format_hex_u64, format_i32};

use crate::audit::{record, AuditEvent};

/// SHA-256 fingerprint of [`tairix_abi::ENCODED_TABLE`].
///
/// The value is **derived at build time** by this crate's `build.rs`
/// from `tairix_abi::ENCODED_TABLE` — the single source of truth — and `include!`d here. There is no
/// hand-maintained literal to edit or to let drift out of sync with the
/// table it fingerprints: changing the syscall table re-derives this
/// value on the next build. The kernel still re-checks it via
/// [`verify_table_hash`] at the syscall-registration phase of
/// `kernel_main`, and `cargo xtask abi-check` cross-checks the linked
/// value against a freshly computed digest; refusal to boot beats
/// silently dispatching against an ABI the user space never agreed to.
pub const SYSCALL_TABLE_HASH: Sha256Digest =
    include!(concat!(env!("OUT_DIR"), "/syscall_table_hash.rs"));

/// Re-compute the SHA-256 of [`tairix_abi::ENCODED_TABLE`] and compare it
/// to [`SYSCALL_TABLE_HASH`].
///
/// # Errors
///
/// Returns [`Errno::AbiVersionUnsupported`] when the two diverge, which
/// can only happen if the dependency graph contains a `tairix-abi`
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
    /// The calling **thread**, and the identifier carried in audit records.
    ///
    /// This is the schedulable entity, so it is what a park, unpark, or wake
    /// must name. For per-process state use [`Self::process`] instead.
    pub task_id: TaskId,
    /// Effective capability set, already intersected with the user
    /// grant and manifest request (see `kernel/sec`).
    pub caps: &'a TaskCapabilities,
}

impl CallerContext<'_> {
    /// The **process** (thread group) the calling thread belongs to — the key
    /// every piece of per-process state is held under.
    ///
    /// Read straight off the capability snapshot the dispatcher already took,
    /// so it costs no lookup and no lock: the record *is* the process's.
    #[must_use]
    pub fn process(&self) -> ProcessId {
        self.caps.process()
    }
}

/// Whether a **parser sandbox** task (`SPAWN_FLAG_SANDBOX`,
/// `docs/src/security/sandbox.md`) may issue `number` at all.
///
/// The list is closed and deliberately short: only the self-scoped and
/// descriptor-scoped operations a sandboxed worker needs to run, talk over
/// the descriptors its parent explicitly wired, and manage its own heap.
/// Everything that names an object *outside* the task — a path, an IPC
/// endpoint, a resource reference, a process, a device, system state — is
/// refused before its handler runs, so a compromised parser cannot even
/// probe those surfaces. In particular there is no `fs_open` (no path-based
/// authority; `fs_read`/`fs_write`/`fs_close` act only on descriptors the
/// parent handed over), no `spawn`/`signal`/`wait` (a sandbox spawns
/// nothing), no `ipc_*`/`pipe_create`/`resource_open` (no new channels),
/// and no `cap_*` (a sandbox holds nothing and may never be handed
/// anything). Widening this list is a security decision reviewed under the
/// charter's capability-minimalism bar, never a convenience.
#[must_use]
pub fn sandbox_allows(number: SyscallNumber) -> bool {
    matches!(
        number,
        SyscallNumber::YIELD
            | SyscallNumber::EXIT
            | SyscallNumber::STREAM_READ
            | SyscallNumber::STREAM_WRITE
            | SyscallNumber::FS_READ
            | SyscallNumber::FS_WRITE
            | SyscallNumber::FS_CLOSE
            | SyscallNumber::MEM_MAP
            | SyscallNumber::MEM_UNMAP
            // Threads and the futex are self-scoped: a thread runs in the
            // sandbox's *own* address space under its *own* (empty) capability
            // record, and a futex key names a word inside that space. Neither
            // reaches anything the sandbox could not already reach, and a
            // sandboxed parser that can block on a word rather than spin is
            // strictly better behaved.
            | SyscallNumber::THREAD_CREATE
            | SyscallNumber::THREAD_EXIT
            | SyscallNumber::FUTEX_WAIT
            | SyscallNumber::FUTEX_WAKE
    )
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
/// caller — except the two benign non-rejections, which get their own
/// below-error records: [`Errno::WouldBlock`] ("nothing yet, retry",
/// [`AuditEvent::SyscallHandlerWouldBlock`]) and [`Errno::NotFound`]
/// ("no such object", [`AuditEvent::SyscallHandlerNotFound`]).
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
    /// Receive a message from an IPC message port the caller owns.
    ///
    /// `sender_out` names a caller buffer of exactly
    /// `tairix_abi::ORIGIN_WIRE_LEN` bytes; the implementation writes the
    /// sending task's kernel-attested `Origin` (snapshotted at send time)
    /// through it alongside the payload copy, so the receiver can
    /// authenticate each message's principal fail-closed.
    fn ipc_recv(
        &self,
        caller: &CallerContext<'_>,
        endpoint: u64,
        ptr: u64,
        len: usize,
        sender_out: u64,
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
    /// [`tairix_abi::RANDOM_REQUEST_MAX_BYTES`] with
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
    /// (`tairix_abi::DescriptorTable`): an `fd` that is not a writable
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
    /// The dispatcher has already checked that `path` is non-null and that
    /// `path_len` fits in `usize`, but attaches **no** capability gate: which
    /// authority a spawn needs depends on the attach block only this
    /// implementation decodes. It must therefore refuse a caller holding
    /// neither [`CapabilityId::PROC_SPAWN`] nor
    /// [`CapabilityId::SANDBOX_SPAWN`] before staging anything, and then,
    /// once the block is parsed, admit a canonical parser-sandbox spawn on
    /// either capability and every other spawn on
    /// [`CapabilityId::PROC_SPAWN`] alone. The implementation copies the path in
    /// through the validated `copy_from_user` boundary, looks it up in the kernel's embedded-program registry,
    /// builds a fresh hardware-isolated address space for it,
    /// registers it as a runnable process, and returns its PID; the
    /// caller keeps running (`plans/SPAWN.md` SP3 — a true concurrent
    /// spawn, not an `exec`-style hand-off). A
    /// build with no spawn service wired must fail closed with
    /// [`Errno::NotImplemented`], and a path naming no registered
    /// program with [`Errno::NotFound`], rather than silently doing
    /// nothing.
    ///
    /// `(attach, attach_len)` optionally carry the child's *attach block*
    /// (`plans/SPAWN.md` SP10): a non-zero `attach` names an encoded
    /// [`tairix_abi::SpawnAttach`] block in the caller's address space.
    /// The implementation stages exactly
    /// [`tairix_abi::SPAWN_ATTACH_LEN`] bytes through the validated
    /// `copy_from_user` boundary and parses the block fail-closed; a
    /// malformed block is rejected whole, never partially applied. The
    /// block selects the child's credential
    /// ([`tairix_abi::SPAWN_UID_INHERIT`] keeps the caller's own; a
    /// concrete uid asks the kernel to resolve that user and switch the
    /// child into it, which requires [`CapabilityId::SPAWN_AS_USER`] and
    /// must fail closed with [`Errno::PermissionDenied`] otherwise — a
    /// running process can never change its *own* identity, there is no
    /// setuid-self), the console its base descriptor table comes from
    /// ([`tairix_abi::CONSOLE_INHERIT`] = the caller's own table; any
    /// other value names an installed console index, failing closed with
    /// [`Errno::NotFound`] when none is installed there), and one
    /// [`tairix_abi::FdWire`] per standard descriptor — wiring the
    /// child's fd 0/1/2/3 onto pre-opened descriptors of the **caller's
    /// own** open table (files, resources, pipe ends), each resolved
    /// owner-checked against the kernel-attested caller so a forged or
    /// foreign number refuses the spawn with [`Errno::NotFound`]. A zero
    /// `attach` means full inherit: the caller's own credential and
    /// descriptor table, untouched.
    ///
    /// `(strings, strings_len)` optionally carry the child's startup
    /// strings: a non-zero `strings` names an encoded
    /// `tairix_abi::process` startup-vector block (the `PSV1` format) in
    /// the caller's address space holding the argument vector and
    /// environment the caller chose for the child. The implementation
    /// bounds `strings_len` against
    /// [`tairix_abi::PROCESS_START_MAX_TOTAL_LEN`], stages the block
    /// through the validated `copy_from_user` boundary, and parses it
    /// fail-closed; a malformed block is rejected with the decoder's
    /// stable [`Errno`], never partially applied. The strings
    /// are data — they grant nothing, and the kernel mints the child's
    /// stack canary itself, ignoring the block's. A zero `strings` means
    /// the child receives the program's registered default arguments and
    /// an empty environment.
    // The signature mirrors the syscall's six ABI registers plus the
    // kernel-attested caller; folding registers into a carrier struct would
    // only re-spell the ABI's argument order without removing an argument.
    #[allow(clippy::too_many_arguments)]
    fn spawn(
        &self,
        caller: &CallerContext<'_>,
        path: u64,
        path_len: usize,
        attach: u64,
        attach_len: usize,
        strings: u64,
        strings_len: usize,
    ) -> SyscallResult;
    /// Create a pipe — a bounded, kernel-buffered unidirectional byte
    /// stream — writing its two new descriptors (the read end first, then
    /// the write end, two `u32`s) through the user pointer `out`
    /// (`plans/SPAWN.md` SP10). Returns `Ok(0)` on success.
    ///
    /// The dispatcher has already checked that `out` is non-null. Both
    /// descriptors land in the **caller's own** open table (the same
    /// allocator [`SyscallHandlers::fs_open`] draws from) and are served
    /// by [`SyscallHandlers::fs_read`] / [`SyscallHandlers::fs_write`] /
    /// [`SyscallHandlers::fs_close`]. The default body fails closed with
    /// [`Errno::NotImplemented`] so a build without the pipe facility
    /// announces the inert interface rather than minting descriptors it
    /// cannot serve.
    fn pipe_create(&self, caller: &CallerContext<'_>, out: u64) -> SyscallResult {
        let _ = (caller, out);
        Err(Errno::NotImplemented)
    }
    /// Create a pseudo-terminal — a kernel object joining a master end and a
    /// slave end whose slave carries a console-class line discipline —
    /// writing its two new descriptors (the master end first, then the slave
    /// end, two `u32`s) through the user pointer `out`, at the initial
    /// geometry `rows`×`cols` (`plans/PTY.md`). Returns `Ok(0)` on success.
    ///
    /// The dispatcher has already checked that `out` is non-null. Each
    /// dimension must be non-zero and fit a `u16`, else the call fails
    /// closed with [`Errno::OutOfRange`] before any state is touched. Both
    /// descriptors land in the **caller's own** open table (the same
    /// allocator [`SyscallHandlers::pipe_create`] draws from) and are served
    /// by [`SyscallHandlers::fs_read`] / [`SyscallHandlers::fs_write`] /
    /// [`SyscallHandlers::fs_close`]. The default body fails closed with
    /// [`Errno::NotImplemented`] so a build without the pty facility
    /// announces the inert interface rather than minting descriptors it
    /// cannot serve.
    fn pty_create(
        &self,
        caller: &CallerContext<'_>,
        out: u64,
        rows: u32,
        cols: u32,
    ) -> SyscallResult {
        let _ = (caller, out, rows, cols);
        Err(Errno::NotImplemented)
    }
    /// Set the character-cell geometry of the pseudo-terminal the caller's
    /// descriptor `fd` is the **master** end of, to `rows`×`cols`
    /// (`plans/PTY.md`). Returns `Ok(0)` on success. The graphical
    /// terminal's window-resize path — the tty `TIOCSWINSZ` analogue.
    ///
    /// Each dimension must be non-zero and fit a `u16`, else the call fails
    /// closed with [`Errno::OutOfRange`] before any state is touched. `fd`
    /// must be a pty **master** descriptor of the caller; anything else
    /// fails closed with [`Errno::NotFound`], never leaking which case
    /// occurred. The default body fails closed with [`Errno::NotImplemented`]
    /// so a build without the pty facility announces the inert interface.
    fn pty_set_size(
        &self,
        caller: &CallerContext<'_>,
        fd: u32,
        rows: u32,
        cols: u32,
    ) -> SyscallResult {
        let _ = (caller, fd, rows, cols);
        Err(Errno::NotImplemented)
    }
    /// Read up to `len` bytes from the calling process's standard stream
    /// `fd` into the user buffer at `buf`, returning the number of bytes
    /// read.
    ///
    /// The dispatcher has already checked the caller holds
    /// [`CapabilityId::CONSOLE_READ`], that `buf` is non-null, that `fd`
    /// fits in `u32`, and that `len` fits in `usize`. The implementation
    /// resolves `fd` against the caller's per-process descriptor table
    /// (`tairix_abi::DescriptorTable`): an `fd` that is not a readable
    /// inherited stream fails closed. It then
    /// reads from that descriptor's kernel stream backing — in the
    /// bootstrap session the first discovered keyboard/UART input source
    /// (`plans/PI.md` P6) — into a bounded kernel staging buffer and
    /// copies it out through the validated `copy_to_user` boundary. A short read (fewer bytes than `len`, possibly
    /// zero when no input is pending) is valid, so the caller loops. A
    /// build with no backing wired must fail closed with
    /// [`Errno::NotImplemented`] rather than fabricating input.
    ///
    /// `timeout_ns` bounds how long a read with no pending input may park:
    /// `0` waits indefinitely (the interactive default), and a non-zero
    /// bound fails with [`Errno::TimedOut`] once it elapses with no input,
    /// so a full-screen program can refresh a clock or status figure
    /// without a busy poll.
    fn stream_read(
        &self,
        caller: &CallerContext<'_>,
        fd: u32,
        buf: u64,
        len: usize,
        timeout_ns: u64,
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
    /// `i32`, that `status` is a non-null `UserPtr`, and that `flags` carries
    /// no reserved bit. `pid` is either a specific child's PID or
    /// [`tairix_abi::WAIT_PID_ANY`] (wait for any child). The implementation
    /// validates the parent/child relationship — a process may only reap its
    /// **own** children — and copies the exit code out through the validated
    /// `copy_to_user` boundary. A `pid` that is not a child of the caller
    /// must fail closed with [`Errno::NotFound`]; a build with no
    /// process-wait service wired must fail closed with
    /// [`Errno::NotImplemented`] rather than fabricating a reaped child.
    ///
    /// With [`WaitFlags::NONBLOCK`] clear the call blocks the caller until a
    /// child is reapable (never busy-polls). With it set the call polls: it
    /// reaps an already-exited child if one exists, otherwise — when a
    /// matching child is still running — returns [`Errno::WouldBlock`]
    /// without parking the caller and leaves `status` untouched. With
    /// [`WaitFlags::STOPPED`] set the call also reports a child freshly
    /// stopped by [`Signal::Stop`] without reaping it. `status` receives
    /// the typed [`tairix_abi::WaitStatusRecord`], not a bare exit code.
    fn wait(
        &self,
        caller: &CallerContext<'_>,
        pid: i32,
        status: u64,
        flags: WaitFlags,
    ) -> SyscallResult;

    /// Deliver control signal `signal` to a child of the calling process
    /// (`plans/SPAWN.md` SP7).
    ///
    /// The dispatcher has already validated that `pid` is a sign-extended
    /// `i32` and that `signal` is a defined [`Signal`] (a value outside the
    /// closed set is rejected before dispatch with [`Errno::OutOfRange`]).
    /// The implementation identifies the sender from the kernel-provided
    /// caller identity (never a caller-supplied one), validates the
    /// parent/child relationship — a process may signal only its **own**
    /// children — and delivers the signal. A `pid` that is not a child of
    /// the caller must fail closed with [`Errno::NotFound`]. Returns
    /// `Ok(0)` on success.
    ///
    /// The default implementation fails closed with [`Errno::NotImplemented`]:
    /// a kernel build with no process-signal service wired never pretends the
    /// signal landed. The producer is installed in `kernel/core`.
    fn signal(&self, _caller: &CallerContext<'_>, _pid: i32, _signal: Signal) -> SyscallResult {
        Err(Errno::NotImplemented)
    }

    /// Change the calling process's working directory to the path at
    /// `(path, path_len)` (`plans/SHELL.md` P2).
    ///
    /// The dispatcher has already checked [`CapabilityId::FS_ACCESS`] and that
    /// `path` is a non-null `UserPtr`. The implementation copies the path in
    /// through the validated `copy_from_user` boundary, resolves it (relative
    /// to the caller's current working directory when it is not absolute) with
    /// the shared path parser, and re-authorises it as a searchable directory
    /// through the secured VFS under the caller's real credentials before it
    /// becomes the new working directory. A path that is not a searchable
    /// directory fails closed and leaves the working directory unchanged.
    /// Returns `Ok(0)` on success.
    ///
    /// The default implementation fails closed with [`Errno::NotImplemented`]:
    /// a kernel build with no filesystem service wired never pretends the
    /// working directory changed. The service is installed in `kernel/core`.
    fn fs_chdir(&self, _caller: &CallerContext<'_>, _path: u64, _path_len: usize) -> SyscallResult {
        Err(Errno::NotImplemented)
    }

    /// Write the calling process's working directory — a normalised absolute
    /// path — to the user buffer at `buf` (`plans/SHELL.md` P2).
    ///
    /// The dispatcher has already validated that `buf` is a non-null
    /// `UserPtr`. The implementation copies the working directory out through
    /// the validated `copy_to_user` boundary and returns its byte length. A
    /// buffer smaller than the path must fail closed with
    /// [`Errno::BufferTooSmall`] — the path is never truncated. Reading one's
    /// own working directory grants no authority, so no capability is
    /// required.
    ///
    /// The default implementation fails closed with [`Errno::NotImplemented`];
    /// the service is installed in `kernel/core`.
    fn fs_getcwd(&self, _caller: &CallerContext<'_>, _buf: u64, _buf_cap: usize) -> SyscallResult {
        Err(Errno::NotImplemented)
    }

    /// Resolve the resource reference at `(reference, reference_len)` and open
    /// it to a new descriptor with `flags` (`plans/SHELL.md` P5).
    ///
    /// The dispatcher has already validated that `reference` is a non-null
    /// `UserPtr` and rejected any illegal [`OpenFlags`]. The implementation
    /// copies the reference in through the validated `copy_from_user`
    /// boundary, parses it with the single shared reference parser, and
    /// resolves it through the capability-checked namespace resolver under the
    /// caller's kernel-attested identity — authorisation is per namespace and
    /// selector, so there is no blanket dispatcher gate. On success it records
    /// a resource-backed descriptor in the caller's per-process table (the
    /// same number space as [`Self::fs_open`], so numbers never collide) and
    /// returns it; a malformed, unknown, unwired, or unauthorised reference
    /// fails closed without minting a descriptor.
    ///
    /// The default implementation fails closed with [`Errno::NotImplemented`]:
    /// a kernel build with no resource resolver wired never fabricates a
    /// handle. The real handler is installed in `kernel/core`.
    fn resource_open(
        &self,
        _caller: &CallerContext<'_>,
        _reference: u64,
        _reference_len: usize,
        _flags: OpenFlags,
    ) -> SyscallResult {
        Err(Errno::NotImplemented)
    }

    /// Read the calling task's effective limit for resource `kind`, writing
    /// the encoded [`tairix_abi::ResourceLimit`] to the user `out` pointer.
    ///
    /// The dispatcher has already validated that `kind` fits in a `u32`
    /// (upper bits zero) and that `out` is a non-null `UserPtr`. The
    /// implementation validates `kind` against [`tairix_abi::LimitKind`] and
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
    /// [`tairix_abi::ResourceLimit`] at the user `value` pointer.
    ///
    /// The dispatcher has already validated that `kind` fits in a `u32` and
    /// that `value` is a non-null `UserPtr`. The implementation copies the
    /// limit in through the validated `copy_from_user` boundary, validates
    /// `kind` and the soft/hard pair, and — when the request would *raise* a
    /// hard bound above the inherited ceiling — refuses with
    /// [`Errno::PermissionDenied`] unless the caller holds
    /// [`tairix_abi::CapabilityId::RLIMIT_RAISE`]. Returns `Ok(0)` on
    /// success.
    ///
    /// The default implementation fails closed with [`Errno::NotImplemented`]; the enforcement is installed in `kernel/core`.
    fn rlimit_set(&self, _caller: &CallerContext<'_>, _kind: u32, _value: u64) -> SyscallResult {
        Err(Errno::NotImplemented)
    }

    /// Mark the calling process's entire anonymous memory — current and
    /// future — as pinned: ineligible for the compressed `ramzip` tier
    /// and any future lower swap tier (`plans/STRESSTEST.md` ST2).
    ///
    /// The dispatcher has already checked
    /// [`tairix_abi::CapabilityId::MEM_PIN`] and emitted the audit
    /// record. The implementation bounds the pin by the caller's
    /// effective `PinnedMemoryBytes` soft limit — a footprint already
    /// past the bound fails closed with [`Errno::OutOfRange`] — and
    /// stores the mark against the kernel-trusted caller id. Already
    /// pinned is success (the process is in the requested state).
    ///
    /// The default implementation fails closed with
    /// [`Errno::NotImplemented`]: a kernel build with no pin bookkeeping
    /// wired never pretends a pin took effect. The enforcement is
    /// installed in `kernel/core`.
    fn mem_pin(&self, _caller: &CallerContext<'_>) -> SyscallResult {
        Err(Errno::NotImplemented)
    }

    /// Clear the calling process's `mem_pin` mark, restoring its
    /// anonymous memory's eligibility for the swap tiers
    /// (`plans/STRESSTEST.md` ST2).
    ///
    /// Ungated (releasing the caller's own exemption grants nothing) but
    /// audited by the dispatcher like `mem_pin`, so the trail carries
    /// both edges of every pin window. Already unpinned is success.
    ///
    /// The default implementation fails closed with
    /// [`Errno::NotImplemented`]; the bookkeeping is installed in
    /// `kernel/core`.
    fn mem_unpin(&self, _caller: &CallerContext<'_>) -> SyscallResult {
        Err(Errno::NotImplemented)
    }

    /// Operate on the calling process's own signal intake — the fail-closed
    /// signal-observation opt-in (`plans/STRESSTEST.md` ST3).
    ///
    /// The dispatcher has already validated `op` against the closed
    /// [`SignalIntakeOp`] set and emitted the audit record. The
    /// implementation acts only on the caller's own intake, keyed by the
    /// kernel-trusted caller id: `Enable` opts the caller's
    /// `Interrupt`/`Terminate` disposition into observable delivery
    /// (idempotent), `Disable` restores the default (idempotent; refused
    /// [`Errno::WouldBlock`] while an observed signal is pending
    /// undrained), and `Take` drains the one pending observed signal,
    /// returning its wire discriminant ([`Errno::WouldBlock`] when nothing
    /// is pending, [`Errno::NotFound`] when never enabled).
    ///
    /// The default implementation fails closed with
    /// [`Errno::NotImplemented`]: a kernel build with no intake bookkeeping
    /// wired never pretends a disposition changed. The bookkeeping is
    /// installed in `kernel/core`.
    fn signal_intake(&self, _caller: &CallerContext<'_>, _op: SignalIntakeOp) -> SyscallResult {
        Err(Errno::NotImplemented)
    }

    /// Set the calling task's scheduling class — enter (`realtime` true) or
    /// leave (false) the strict-priority real-time band (`plans/USB.md`).
    ///
    /// The dispatcher has already checked the caller holds
    /// [`CapabilityId::SCHED_REALTIME`] — the whole syscall is gated in both
    /// directions, since a task's scheduling class is per-task state and the
    /// capability is static, so only a holder is ever real-time and only a
    /// holder ever leaves the class. This handler acts solely on the
    /// caller's own task, keyed by the kernel-trusted caller id (never a
    /// caller-supplied target). Setting the class the task already holds is
    /// success.
    ///
    /// The default implementation fails closed with
    /// [`Errno::NotImplemented`]: a kernel build with no scheduler class
    /// control wired never pretends a task's priority changed. The real
    /// handler is installed in `kernel/core`.
    fn sched_set_realtime(&self, _caller: &CallerContext<'_>, _realtime: bool) -> SyscallResult {
        Err(Errno::NotImplemented)
    }

    /// Change a target process's time-shared scheduling service level
    /// (`SyscallNumber::SCHED_SET_PRIORITY`).
    ///
    /// The default refuses with [`Errno::NotImplemented`] (fail closed) so
    /// a kernel build with no scheduler control wired never pretends a
    /// task's priority changed. The real handler is installed in
    /// `kernel/core`; it owns the own-child / same-principal /
    /// `CAP_PROC_CONTROL` target rule and the raise gate.
    fn sched_set_priority(
        &self,
        _caller: &CallerContext<'_>,
        _pid: i32,
        _priority: SchedPriority,
    ) -> SyscallResult {
        Err(Errno::NotImplemented)
    }

    /// Copy the system user database (`/System/Security/Users`) the kernel
    /// loaded at boot out to the user buffer at `buf` (
    /// `plans/PI.md` P11).
    ///
    /// The dispatcher has already checked
    /// [`tairix_abi::CapabilityId::USERS_READ`] and that `buf` is a
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

    /// Apply one typed user/group administration request
    /// (`plans/CAPABILITY_USE.md` CU4).
    ///
    /// The dispatcher has already checked the caller holds
    /// [`CapabilityId::USER_ADMIN`] and that `req` and `out` are non-null
    /// `UserPtr`s. `req`/`req_len` carry one versioned
    /// `tairix_abi::users_admin::UsersAdminRequest` record;
    /// `out`/`out_cap` receive a list operation's response (mutating
    /// operations write nothing and answer `0`). The implementation
    /// decodes fail-closed, enforces the never-widen and
    /// last-administrator rules under the caller's kernel-attested
    /// identity, persists before going live, and audits every outcome.
    ///
    /// The default implementation fails closed with
    /// [`Errno::NotImplemented`]: a kernel build with no
    /// account-administration engine wired refuses every edit.
    fn users_admin(
        &self,
        _caller: &CallerContext<'_>,
        _req: u64,
        _req_len: usize,
        _out: u64,
        _out_cap: usize,
    ) -> SyscallResult {
        Err(Errno::NotImplemented)
    }

    /// Switch a seat's foreground session — retarget which text console an
    /// unowned seat's input drains to (`plans/DISPLAY.md` D3).
    ///
    /// The dispatcher has already checked the caller holds
    /// [`CapabilityId::SEAT_ADMIN`]. `seat_id` names the seat and
    /// `console` the installed text console that becomes
    /// its foreground. The implementation validates both against the live
    /// seat registry and console list — an unknown seat or console fails
    /// closed with [`Errno::NotFound`] before any state changes — then
    /// retargets the foreground and audits the switch. A held seat keeps
    /// routing to its owner until the lease ends.
    ///
    /// The default implementation fails closed with
    /// [`Errno::NotImplemented`]: a build with no seat registry wired has
    /// no foreground to switch. The real handler is installed in
    /// `kernel/core`.
    fn seat_switch(
        &self,
        _caller: &CallerContext<'_>,
        _seat_id: u64,
        _console: u32,
    ) -> SyscallResult {
        Err(Errno::NotImplemented)
    }

    /// Forcibly revoke a seat's current lease — evict a wedged or
    /// switched-away owner (`plans/DISPLAY.md` D3).
    ///
    /// The dispatcher has already checked the caller holds
    /// [`CapabilityId::SEAT_ADMIN`]. `seat_id` names the seat;
    /// an unknown seat fails closed with
    /// [`Errno::NotFound`] and an unowned seat refuses with
    /// [`Errno::SeatNotOwner`]. On success the seat becomes acquirable,
    /// input returns to the text foreground, the evicted owner's next
    /// owner-gated call sees the distinct [`Errno::SeatRevoked`], and the
    /// eviction is audited with the evicted owner's task id.
    ///
    /// The default implementation fails closed with
    /// [`Errno::NotImplemented`]: a build with no seat registry wired has
    /// no lease to revoke. The real handler is installed in `kernel/core`.
    fn seat_revoke(&self, _caller: &CallerContext<'_>, _seat_id: u64) -> SyscallResult {
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

    /// Set the console read line discipline of one of the calling
    /// process's inherited input streams (`plans/PI.md` P11 — the
    /// [`tairix_abi::InputMode`]: cooked, secret, or raw).
    ///
    /// The dispatcher has already checked the caller holds
    /// [`CapabilityId::CONSOLE_READ`]. `fd` must be a readable inherited
    /// stream and `mode` an [`tairix_abi::InputMode`] discriminant (the
    /// reserved `0` and every unknown value fail closed). The
    /// implementation selects the resolved console's discipline: cooked
    /// echoes, secret suppresses echo and shows the activity indicator (a
    /// password read never renders the secret), raw suppresses echo and
    /// draws nothing (a full-screen program paints its own display).
    ///
    /// The default implementation fails closed with
    /// [`Errno::NotImplemented`]: a kernel build with
    /// no console list wired has no discipline to select. The real handler
    /// is installed in `kernel/core`.
    fn stream_input_mode(
        &self,
        _caller: &CallerContext<'_>,
        _fd: u32,
        _mode: u32,
    ) -> SyscallResult {
        Err(Errno::NotImplemented)
    }

    /// Mark (or clear) the foreground job of the console behind readable
    /// descriptor `fd`, the task its cooked-mode line discipline delivers
    /// `^C`/`^Z` to (`plans/SPAWN.md` SP9 — the `tcsetpgrp` analogue).
    ///
    /// The dispatcher has already checked [`CapabilityId::CONSOLE_READ`]
    /// (the same fd-scoped terminal-control gate `stream_input_mode`
    /// carries) and that `pid` is a sign-extended `i32`. The implementation
    /// resolves `fd` against the caller's own descriptor table (a
    /// non-readable or unbacked descriptor fails closed with
    /// [`Errno::NotFound`]), and for a non-zero `pid` authorises it as a
    /// **live child of the caller** through the same parent/child
    /// bookkeeping `wait`/`signal` use — never a caller-supplied claim. A
    /// `pid` of `0` clears the slot. Returns `Ok(0)` on success.
    ///
    /// The default implementation fails closed with
    /// [`Errno::NotImplemented`]: a kernel build with no console list wired
    /// has no foreground slot to set. The real handler is installed in
    /// `kernel/core`.
    fn console_foreground(
        &self,
        _caller: &CallerContext<'_>,
        _fd: u32,
        _pid: i32,
    ) -> SyscallResult {
        Err(Errno::NotImplemented)
    }

    /// Discard everything a finished session left on the terminal behind
    /// readable descriptor `fd`, so none of it reaches whoever uses that
    /// terminal next (`terminal_purge`): the retained display (both cell grids
    /// of a framebuffer console, both rings of a pty, a byte-stream device's
    /// remote display and scrollback), the input queued but never read, and
    /// the read line discipline, which returns to cooked.
    ///
    /// The dispatcher has already checked [`CapabilityId::CONSOLE_WRITE`]. The
    /// implementation additionally requires
    /// [`CapabilityId::CONSOLE_READ`] — the purge discards queued input as well
    /// as retained output — before it touches any state, resolves `fd` against
    /// the caller's own descriptor table (a non-readable or unbacked
    /// descriptor fails closed with [`Errno::NotFound`]), and admits only the
    /// terminal's controlling owner, exactly as `stream_input_mode` does.
    /// Returns `Ok(0)`.
    ///
    /// The default implementation fails closed with
    /// [`Errno::NotImplemented`]: a kernel build with no console list wired
    /// has no terminal to purge. The real handler is installed in
    /// `kernel/core`.
    fn terminal_purge(&self, _caller: &CallerContext<'_>, _fd: u32) -> SyscallResult {
        Err(Errno::NotImplemented)
    }

    /// Inject one decoded keyboard *key edge* into the kernel input-focus
    /// arbiter (`plans/PI.md` P11 — input follows the
    /// surface owner).
    ///
    /// The dispatcher has already checked the caller holds
    /// [`CapabilityId::INPUT_INJECT`] and that `buf` is a non-null
    /// `UserPtr`. `seat` names the seat the edge belongs to (the seat whose
    /// keyboard produced it); an unknown seat id fails closed with
    /// [`Errno::NotFound`]. The implementation copies up to `len` bytes in
    /// through the validated `copy_from_user` boundary, decodes
    /// one [`tairix_abi::input::KeyInput`] record fail-closed, and hands it
    /// to the arbiter, which decides the encoding and destination by who
    /// holds that seat: with the text console foreground it encodes the press
    /// to console (tty) bytes and enqueues them on the focused console's
    /// input queue; with the desktop foreground it routes the record to the
    /// seat's keyboard channel. The driver no longer chooses the encoding
    /// or destination. Returns the number of bytes
    /// consumed from the record.
    ///
    /// The default implementation fails closed with
    /// [`Errno::NotImplemented`]: a kernel build with no
    /// input-focus arbiter wired has nowhere to route the edge. The real
    /// handler is installed in `kernel/core`.
    fn key_inject(
        &self,
        _caller: &CallerContext<'_>,
        _seat: u64,
        _buf: u64,
        _len: usize,
    ) -> SyscallResult {
        Err(Errno::NotImplemented)
    }

    /// Acquire ownership of the seat `seat` — one display with its keyboard
    /// — as an exclusive, owner-tracked lease (`plans/DISPLAY.md`;
    /// `plans/PI.md` P11 — input follows the surface owner).
    ///
    /// The dispatcher has already checked the caller holds
    /// [`CapabilityId::DISPLAY`]. An unknown seat id fails closed with
    /// [`Errno::NotFound`]. The implementation records the
    /// kernel-attested caller as the seat owner, so subsequently injected
    /// key edges ([`Self::key_inject`]) are delivered as records the owner
    /// drains with [`Self::keyboard_read`]. A seat held by another task
    /// refuses the claim with [`Errno::SeatBusy`] (ownership is never
    /// displaced); a repeat acquire by the holder is refused with
    /// [`Errno::AlreadyExists`].
    ///
    /// The default implementation fails closed with
    /// [`Errno::NotImplemented`]: a build with no
    /// seat registry wired owns no seat to acquire. The real handler is
    /// installed in `kernel/core`.
    fn display_acquire(&self, _caller: &CallerContext<'_>, _seat: u64) -> SyscallResult {
        Err(Errno::NotImplemented)
    }

    /// Release the seat `seat` and return its keyboard input to the text
    /// console (`plans/DISPLAY.md`; `plans/PI.md` P11).
    ///
    /// The inverse of [`Self::display_acquire`]; the dispatcher has already
    /// checked the caller holds [`CapabilityId::DISPLAY`]. The
    /// implementation is owner-checked: a caller that does not hold the
    /// seat is refused with [`Errno::SeatNotOwner`] (or
    /// [`Errno::SeatRevoked`] once, after an administrative eviction),
    /// never a global "flip it back" switch; an unknown seat id fails
    /// closed with [`Errno::NotFound`]. `next` says what becomes of the
    /// seat's screen — the text console takes it back, or it is held
    /// cleared for the graphical presenter taking over — and a value
    /// outside that closed set is refused with [`Errno::OutOfRange`]. The
    /// default implementation fails closed with [`Errno::NotImplemented`].
    fn display_release(
        &self,
        _caller: &CallerContext<'_>,
        _seat: u64,
        _next: ReleaseSurface,
    ) -> SyscallResult {
        Err(Errno::NotImplemented)
    }

    /// Read one decoded keyboard event from the kernel keyboard channel
    /// (`plans/PI.md` P11 — keyboard input for the
    /// desktop).
    ///
    /// The dispatcher has already checked the caller holds
    /// [`CapabilityId::INPUT_READ`] and that `buf` is a non-null `UserPtr`.
    /// `seat` names the seat whose channel is drained; an unknown seat id
    /// fails closed with [`Errno::NotFound`].
    /// The implementation owner-gates the drain against the seat's live
    /// lease — a caller that does not hold the seat is refused with
    /// [`Errno::SeatNotOwner`] / [`Errno::SeatRevoked`] — then drains one
    /// [`tairix_abi::input::KeyInput`] record
    /// the seat registry routed to the channel into `buf` (at least
    /// [`tairix_abi::input::KeyInput::WIRE_LEN`] bytes), copies it out
    /// through the validated boundary, and returns the
    /// number of bytes written — or `0` when the channel is drained.
    ///
    /// The default implementation fails closed with
    /// [`Errno::NotImplemented`]: a build with no
    /// seat registry wired has no channel to drain. The real handler is
    /// installed in `kernel/core`.
    fn keyboard_read(
        &self,
        _caller: &CallerContext<'_>,
        _seat: u64,
        _buf: u64,
        _len: usize,
    ) -> SyscallResult {
        Err(Errno::NotImplemented)
    }

    /// Inject one decoded pointer event into the kernel seat registry
    /// (`plans/PI.md` P11 — the pointer analogue of [`Self::key_inject`]).
    ///
    /// The dispatcher has already checked the caller holds
    /// [`CapabilityId::INPUT_INJECT`] and that `buf` is a non-null
    /// `UserPtr`. `seat` names the seat the event belongs to (the seat
    /// whose pointing device produced it); an unknown seat id fails closed
    /// with [`Errno::NotFound`]. The implementation copies up to `len`
    /// bytes in through the validated `copy_from_user` boundary, decodes
    /// one [`tairix_abi::input::PointerInput`] record fail-closed, and
    /// hands it to the seat registry, which routes by who holds that seat:
    /// a held seat's record goes to its pointer channel (drained by
    /// [`Self::pointer_read`]); an unowned seat's record is consumed and
    /// discarded — the text console has no pointer consumer. Returns the
    /// number of bytes consumed from the record.
    ///
    /// The default implementation fails closed with
    /// [`Errno::NotImplemented`]: a kernel build with no seat registry
    /// wired has nowhere to route the event. The real handler is installed
    /// in `kernel/core`.
    fn pointer_inject(
        &self,
        _caller: &CallerContext<'_>,
        _seat: u64,
        _buf: u64,
        _len: usize,
    ) -> SyscallResult {
        Err(Errno::NotImplemented)
    }

    /// Read one decoded pointer event from a seat's pointer channel
    /// (`plans/PI.md` P11 — the pointer analogue of
    /// [`Self::keyboard_read`]).
    ///
    /// The dispatcher has already checked the caller holds
    /// [`CapabilityId::INPUT_READ`] and that `buf` is a non-null `UserPtr`.
    /// `seat` names the seat whose channel is drained; an unknown seat id
    /// fails closed with [`Errno::NotFound`]. The implementation
    /// owner-gates the drain against the seat's live lease — a caller that
    /// does not hold the seat is refused with [`Errno::SeatNotOwner`] /
    /// [`Errno::SeatRevoked`] — then drains one
    /// [`tairix_abi::input::PointerInput`] record the seat registry routed
    /// to the channel into `buf` (at least
    /// [`tairix_abi::input::PointerInput::WIRE_LEN`] bytes), copies it out
    /// through the validated boundary, and returns the number of bytes
    /// written — or `0` when the channel is drained.
    ///
    /// The default implementation fails closed with
    /// [`Errno::NotImplemented`]: a build with no seat registry wired has
    /// no channel to drain. The real handler is installed in `kernel/core`.
    fn pointer_read(
        &self,
        _caller: &CallerContext<'_>,
        _seat: u64,
        _buf: u64,
        _len: usize,
    ) -> SyscallResult {
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
    /// set as consecutive [`tairix_abi::hwtree::GrantedResource`] records,
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
    /// [`tairix_abi::hwtree::HwTreeHeader`] (generation + node count)
    /// followed by that many [`tairix_abi::hwtree::HwNode`] records, copies
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
    /// `UserPtr`s and validated `flags` against the defined
    /// [`CallRecvFlags`] bits. The implementation resolves `endpoint`,
    /// enforces the endpoint's required **receive** capability against the
    /// caller before touching state, and either copies one request out
    /// (returning its byte length and writing its ticket to `ticket_out`) or
    /// blocks cooperatively until one is posted (never busy-spinning) —
    /// unless `flags` carries [`CallRecvFlags::NON_BLOCKING`], in which case
    /// an empty queue fails closed with [`Errno::WouldBlock`] instead of
    /// parking (the wait-set event-loop mode: a queued call the wait-set
    /// reported may legitimately have been cancelled by its poster's exit).
    /// A request larger than `buf_cap` fails closed with
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
        _flags: CallRecvFlags,
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

    /// Read the kernel-attested [`tairix_abi::Origin`] of the caller whose
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
    /// than [`tairix_abi::ORIGIN_WIRE_LEN`] fails closed; the origin is never
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

    /// Post a request to a call endpoint without blocking, arming a
    /// per-request deadline, and write the correlating ticket to
    /// `ticket_out` (`plans/FIX-IO.md` IO1 — the asynchronous half of
    /// [`Self::ipc_call`]).
    ///
    /// The dispatcher has already checked `request` and `ticket_out` are
    /// non-null `UserPtr`s. The implementation runs the same endpoint
    /// resolution, per-endpoint grant, capability, and size checks as
    /// [`Self::ipc_call`], copies the request in, posts it with the given
    /// absolute deadline (`u64::MAX` = none — the handler converts the
    /// caller's relative `deadline_ns` against the monotonic clock), wakes
    /// the bound server, and writes the minted ticket out. Returns `Ok(0)`.
    ///
    /// The default implementation fails closed with
    /// [`Errno::NotImplemented`]; the real handler is installed in
    /// `kernel/core`.
    fn call_post(
        &self,
        _caller: &CallerContext<'_>,
        _endpoint: u64,
        _request: u64,
        _request_len: usize,
        _ticket_out: u64,
        _deadline_ns: u64,
    ) -> SyscallResult {
        Err(Errno::NotImplemented)
    }

    /// Reap the reply to a request posted with [`Self::call_post`], without
    /// blocking (`plans/FIX-IO.md` IO1).
    ///
    /// The dispatcher has already checked `reply` is a non-null `UserPtr`.
    /// The implementation resolves `endpoint`, claims the reply for `ticket`
    /// on behalf of the calling task (claimant-checked), and either copies
    /// the reply out (returning its byte length) or fails closed with
    /// [`Errno::WouldBlock`] (still pending), [`Errno::TimedOut`] (deadline
    /// elapsed — the ticket is retired), or [`Errno::NotFound`] (cancelled,
    /// torn down, or not this caller's ticket). A reply larger than
    /// `reply_cap` fails closed with [`Errno::BufferTooSmall`].
    ///
    /// The default implementation fails closed with
    /// [`Errno::NotImplemented`]; the real handler is installed in
    /// `kernel/core`.
    fn call_reap(
        &self,
        _caller: &CallerContext<'_>,
        _endpoint: u64,
        _ticket: u64,
        _reply: u64,
        _reply_cap: usize,
    ) -> SyscallResult {
        Err(Errno::NotImplemented)
    }

    /// Withdraw one outstanding request posted with [`Self::call_post`]
    /// (`plans/FIX-IO.md` IO1).
    ///
    /// The implementation resolves `endpoint` and cancels the caller's own
    /// `ticket`, returning `Ok(0)` if a call was withdrawn or
    /// [`Errno::NotFound`] for a foreign, unknown, or already-completed
    /// ticket (no existence oracle).
    ///
    /// The default implementation fails closed with
    /// [`Errno::NotImplemented`]; the real handler is installed in
    /// `kernel/core`.
    fn call_cancel(
        &self,
        _caller: &CallerContext<'_>,
        _endpoint: u64,
        _ticket: u64,
    ) -> SyscallResult {
        Err(Errno::NotImplemented)
    }

    /// Report whether the in-flight caller of a served call endpoint holds a
    /// seat's live lease, returning the lease's generation
    /// (`plans/DISPLAY.md` D7a — the display service's per-present check).
    ///
    /// The implementation resolves `endpoint`, enforces the endpoint's
    /// required **receive** capability against the caller and confirms it is
    /// the owning task — both before touching state, exactly as
    /// [`Self::call_peer_origin`] — then looks up the attested identity
    /// captured for `ticket` and reads seat `seat`'s **live** lease for that
    /// peer, answering with its generation (`>= 1`). A foreign endpoint or
    /// an unknown/not-in-service ticket fails closed; a peer that does not
    /// hold the seat is the typed [`Errno::SeatNotOwner`] /
    /// [`Errno::SeatRevoked`] refusal.
    ///
    /// The default implementation fails closed with
    /// [`Errno::NotImplemented`]; the real handler is installed in
    /// `kernel/core`.
    fn call_peer_seat(
        &self,
        _caller: &CallerContext<'_>,
        _endpoint: u64,
        _ticket: u64,
        _seat: u64,
    ) -> SyscallResult {
        Err(Errno::NotImplemented)
    }

    /// Read the kernel wall-clock time and its provenance state (P-D).
    ///
    /// The dispatcher has already checked `out` is a non-null `UserPtr`; the
    /// call is unprivileged (like `clock_get`). The implementation reads the
    /// monotonic clock on the issuing CPU, projects the stored wall instant
    /// forward by the elapsed monotonic time, and copies the
    /// [`tairix_abi::WallClockReading`] (a [`tairix_abi::Time64`] plus a
    /// [`tairix_abi::WallTimeState`] byte) out, returning its byte length. A
    /// buffer shorter than [`tairix_abi::WallClockReading::WIRE_LEN`] fails
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
    /// [`tairix_abi::WallTimeState`] (rejecting `Unset` and any undefined
    /// discriminant), copies in a [`tairix_abi::Time64`] through the
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
    /// [`tairix_abi::BootId`] minted for this boot out to `out` and returns
    /// its byte length. A buffer shorter than [`tairix_abi::BOOT_ID_LEN`]
    /// fails closed, as does a boot whose random subsystem could not be
    /// seeded in time (the kernel reports `EntropyNotReady` rather than the
    /// all-zero [`tairix_abi::BootId::UNSET`] sentinel).
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

    /// Read the kernel's boot-static machine summary
    /// ([`tairix_abi::BootFacts`]).
    ///
    /// The dispatcher has already checked `out` is a non-null `UserPtr`; the
    /// call is unprivileged (the facts are the machine's public shape —
    /// arch, core count, installed memory — minted once at boot, never live
    /// state and never a secret). The implementation copies the
    /// [`tairix_abi::BOOT_FACTS_WIRE_LEN`]-byte encoding out to `out` and
    /// returns its byte length. A buffer shorter than the wire length fails
    /// closed with [`Errno::BufferTooSmall`].
    ///
    /// The default implementation fails closed with
    /// [`Errno::NotImplemented`]; the real handler is installed in
    /// `kernel/core`.
    fn boot_facts_get(
        &self,
        _caller: &CallerContext<'_>,
        _out: u64,
        _out_cap: usize,
    ) -> SyscallResult {
        Err(Errno::NotImplemented)
    }

    /// Read the calling task's own kernel-attested [`tairix_abi::Origin`].
    ///
    /// The dispatcher has already checked `out` is a non-null `UserPtr`; the
    /// call is unprivileged (a task may always learn its own identity). The
    /// implementation builds the caller's attested origin from its own
    /// kernel-held task record — never a caller-supplied value — and copies
    /// its wire image out, returning its byte length. A buffer shorter than
    /// [`tairix_abi::ORIGIN_WIRE_LEN`] fails closed. This is the self-directed
    /// twin of [`Self::call_peer_origin`]: the origin is read from kernel
    /// state, so a task can neither forge another principal's identity nor
    /// inflate its own.
    ///
    /// The default implementation fails closed with
    /// [`Errno::NotImplemented`]; the real handler is installed in
    /// `kernel/core`.
    fn self_origin(
        &self,
        _caller: &CallerContext<'_>,
        _out: u64,
        _out_cap: usize,
    ) -> SyscallResult {
        Err(Errno::NotImplemented)
    }

    /// Read the **unfiltered, global** kernel introspection view (P-C).
    ///
    /// The dispatcher has already checked the caller holds
    /// [`CapabilityId::SYSINFO_INTROSPECT`] and that `out` is a non-null
    /// `UserPtr`. `domain` is the [`tairix_abi::IntrospectDomain`]
    /// discriminant the handler validates; `arg` is a domain-specific
    /// selector (a record offset for the paged domains, or unused). The
    /// implementation writes the requested records to `out` little-endian
    /// through the validated boundary and returns the byte count.
    ///
    /// The kernel primitive **never narrows by principal**: it always
    /// answers with the whole system's state and leaves per-client scoping
    /// to the `sysinfod` broker (the sole holder of the capability). Every
    /// field is validated and the call fails closed — a bad domain, a short
    /// buffer, or (for the per-task-limits domain) an unresolvable target
    /// [`tairix_abi::ProcId`] all deny.
    ///
    /// The default implementation fails closed with
    /// [`Errno::NotImplemented`]; the real handler is installed in
    /// `kernel/core`.
    fn sysinfo_introspect(
        &self,
        _caller: &CallerContext<'_>,
        _domain: u32,
        _arg: u64,
        _out: u64,
        _out_cap: usize,
    ) -> SyscallResult {
        Err(Errno::NotImplemented)
    }

    /// Read the character-cell geometry of the text console backing a
    /// standard stream (P-C — the `top` terminal UI).
    ///
    /// The dispatcher has already checked `out` is a non-null `UserPtr`; the
    /// call is unprivileged (a program may always ask how big its own
    /// terminal is). The implementation resolves `fd` against the caller's
    /// descriptor table to the backing console, and — only for a console
    /// whose geometry the kernel actually knows (a framebuffer text console)
    /// — writes its [`tairix_abi::TerminalSize`] out little-endian through the
    /// validated boundary and returns its byte length. A byte-stream console
    /// (a UART), whose remote terminal size is unknowable to the kernel,
    /// fails closed with [`Errno::NotImplemented`] so the client applies the
    /// conventional fallback — the kernel never fabricates a size. An `fd`
    /// that is not an open stream, or a buffer shorter than
    /// [`tairix_abi::TerminalSize::WIRE_LEN`], also fails closed.
    ///
    /// The default implementation fails closed with
    /// [`Errno::NotImplemented`]; the real handler is installed in
    /// `kernel/core`.
    fn terminal_size(
        &self,
        _caller: &CallerContext<'_>,
        _fd: u32,
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
    /// The implementation copies in at most [`tairix_abi::LOG_RECORD_MAX`]
    /// bytes through the validated boundary, fully validates the record with
    /// [`tairix_abi::decode_log_record`], and emits it
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
    /// implementation copies in at most [`tairix_abi::hwtree::HwNode::WIRE_LEN`]
    /// bytes through the validated boundary, fully decodes the node with the
    /// fail-closed [`tairix_abi::HwNode::from_bytes`] parser, and admits it **only** when every
    /// [`tairix_abi::hwtree::HwResource`] it requests is wholly contained
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
    /// `flags` is the [`tairix_abi::hwtree::HwRemoveFlags`] word. An empty
    /// set is a surprise removal (the device physically vanished) and always
    /// proceeds; the `ORDERLY` bit is the stop-if-idle posture that refuses
    /// with [`Errno::Busy`], removing nothing, while a volume is still
    /// attached on a block-service endpoint the node declares. The handler
    /// decodes and validates `flags` before touching any state, rejecting a
    /// reserved bit with [`Errno::OutOfRange`] (validate every input, fail
    /// closed).
    ///
    /// The default implementation fails closed with
    /// [`Errno::NotImplemented`]; the real handler is installed in
    /// `kernel/core`.
    fn hw_remove_node(
        &self,
        _caller: &CallerContext<'_>,
        _node_id: u64,
        _flags: u64,
    ) -> SyscallResult {
        Err(Errno::NotImplemented)
    }

    /// Publish the fault-domain health of the interior node the calling
    /// driver owns into the live hardware tree (`plans/FIX-IO.md` IO4,
    /// cross-process fault-domain propagation).
    ///
    /// The dispatcher has already checked the caller holds
    /// [`CapabilityId::HW_EMIT`] (the same privilege as
    /// [`Self::hw_emit_node`]). The implementation validates `health` as a
    /// [`tairix_abi::blkio::FaultDomainState`] discriminant, resolves the
    /// caller's *own* matched node (never a caller-supplied id), records the
    /// node's health, and bumps the hardware-tree generation so the device
    /// manager's reactive watch reacts to the coherent recovery episode. The
    /// node stays present — a *distinct* signal from [`Self::hw_remove_node`]
    /// so a merely-recovering subtree is never torn down. An out-of-range
    /// health, a caller with no loaded node, or an absent node fails closed.
    /// Returns `Ok(0)` once recorded.
    ///
    /// The default implementation fails closed with
    /// [`Errno::NotImplemented`]; the real handler is installed in
    /// `kernel/core`.
    fn hw_node_health(&self, _caller: &CallerContext<'_>, _health: u64) -> SyscallResult {
        Err(Errno::NotImplemented)
    }

    /// Report the hardware-tree node id the calling driver was autoloaded for
    /// (`plans/FIX-IO.md` IO4, leaf-side fault-domain attribution).
    ///
    /// The call needs no capability (a driver learning its *own* node id is
    /// the unprivileged self-identity baseline, like reading one's own pid).
    /// The implementation resolves the caller's own matched node from its task
    /// id (never a caller-supplied id — no ambient authority, no window onto
    /// the global tree) and returns that node id. A caller with no matched
    /// node (not an autoloaded driver) fails closed with [`Errno::NotFound`].
    ///
    /// The default implementation fails closed with
    /// [`Errno::NotImplemented`]; the real handler is installed in
    /// `kernel/core`.
    fn hw_self_node(&self, _caller: &CallerContext<'_>) -> SyscallResult {
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
    /// [`tairix_abi::hwtree::HwResource::irq`] onto a child node), and writes
    /// the encoded [`tairix_abi::MsiAllocation`] into the caller's `out`
    /// buffer through the validated boundary, returning the number of bytes
    /// written. A platform with no MSI controller fails closed with
    /// [`Errno::NotImplemented`]; an exhausted vector space fails closed with
    /// [`Errno::OutOfRange`]; a buffer shorter than
    /// [`tairix_abi::MsiAllocation::WIRE_LEN`] fails closed.
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
    /// base user virtual address, writing the region's byte length — the
    /// kernel's own record, never the granting client's claim — to the
    /// caller-supplied `len_out` pointer. An unknown or non-owned handle, a
    /// grant of the wrong kind, or a torn-down region fails closed.
    ///
    /// The default implementation fails closed with
    /// [`Errno::NotImplemented`]; the real handler is installed in
    /// `kernel/core`.
    fn shm_map(&self, _caller: &CallerContext<'_>, _handle: u64, _len_out: u64) -> SyscallResult {
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

    /// Grant the serving task of a call endpoint the right to map a shared
    /// memory region the caller owns, returning the minted grant handle
    /// (`plans/DISPLAY.md` D7a — the display client hands its frame buffer
    /// to the display service).
    ///
    /// The dispatcher has already checked the caller holds
    /// [`CapabilityId::SHM`] and audited the call. The implementation
    /// confirms the caller itself holds a `Shared` grant covering `region`
    /// (delegation never widens authority), resolves the live serving task
    /// of `endpoint` at grant time — never a caller-supplied (recyclable)
    /// PID — and mints that task its own unforgeable handle for the region.
    /// An unknown region, a region the caller cannot map, or an unknown
    /// endpoint fails closed with [`Errno::NotFound`].
    ///
    /// The default implementation fails closed with
    /// [`Errno::NotImplemented`]; the real handler is installed in
    /// `kernel/core`.
    fn shm_grant(
        &self,
        _caller: &CallerContext<'_>,
        _region: u64,
        _endpoint: u64,
    ) -> SyscallResult {
        Err(Errno::NotImplemented)
    }

    /// Grant the serving task of one call endpoint the right to *call*
    /// another call endpoint the caller already holds, returning the minted
    /// grant handle (`plans/FIX-IO.md` `IO6b` — the endpoint sibling of
    /// [`Self::shm_grant`], so a composing service can drive the several
    /// member devices an array is made of).
    ///
    /// The dispatcher has already checked the caller holds
    /// [`CapabilityId::IPC_ENDPOINT`] and audited the call. The
    /// implementation confirms the caller itself holds an `Endpoint` grant
    /// covering `endpoint` **before** reading any endpoint state (delegation
    /// never widens authority), resolves the live serving task of
    /// `recipient` at grant time — never a caller-supplied (recyclable) PID
    /// — and mints that task its own unforgeable handle for `endpoint`. A
    /// grant the caller does not hold and an unknown recipient endpoint are
    /// the same [`Errno::NotFound`] with nothing minted, so the reply
    /// confirms nothing about foreign endpoints.
    ///
    /// The default implementation fails closed with
    /// [`Errno::NotImplemented`]; the real handler is installed in
    /// `kernel/core`.
    fn call_grant(
        &self,
        _caller: &CallerContext<'_>,
        _endpoint: u64,
        _recipient: u64,
    ) -> SyscallResult {
        Err(Errno::NotImplemented)
    }

    /// Delegate one of the caller's own open filesystem descriptors to
    /// another live task as a one-shot grant bounded above by
    /// `write_ceiling` bytes, returning the minted grant handle
    /// (`plans/CAPABILITY_USE.md` CU6 — the file picker's user-mediated
    /// hand-off; `plans/APPDATA.md` §3.8 — the app-data service's blob
    /// descriptor).
    ///
    /// The dispatcher has already checked the caller holds
    /// [`CapabilityId::FS_ACCESS`] and audited the call. The implementation
    /// resolves `fd` against the **caller's own** open table (a foreign or
    /// unopened descriptor fails closed with [`Errno::NotFound`]), refuses
    /// any backing that is not a plain filesystem path (a pipe, pty,
    /// resource, or already-delegated descriptor answers
    /// [`Errno::OutOfRange`], so delegation never chains) and any directory,
    /// confirms the recipient task `pid` is live (task ids are never reused,
    /// so the grant can never land on a recycled identity), captures the
    /// caller's uid and effective capability set beside the descriptor's
    /// path, and mints the recipient an unforgeable handle that resolves
    /// only when the recipient itself presents it to [`Self::fd_redeem`].
    ///
    /// The delegation carries the descriptor's **own** read/write access and
    /// nothing more, so it never widens what the grantor opened.
    /// `write_ceiling` is the highest file length the holder may write or
    /// truncate to; it must be zero for a read-only descriptor and non-zero
    /// for a writable one, so an unbounded writable delegation is not a
    /// representable request.
    ///
    /// The default implementation fails closed with
    /// [`Errno::NotImplemented`]; the real handler is installed in
    /// `kernel/core`.
    fn fd_grant(
        &self,
        _caller: &CallerContext<'_>,
        _fd: u32,
        _pid: u64,
        _write_ceiling: u64,
    ) -> SyscallResult {
        Err(Errno::NotImplemented)
    }

    /// Redeem an [`Self::fd_grant`] handle minted to the calling task,
    /// installing the delegated file into the caller's own open table and
    /// returning the fresh descriptor number.
    ///
    /// Needs no capability (the dispatcher gates nothing): receiving
    /// user-mediated, already-checked authority is the point of the
    /// delegation, and every later operation on the descriptor is still
    /// VFS-checked under the grantor's captured identity. The
    /// implementation resolves `handle` against the grants minted **to the
    /// calling task** (a foreign or unknown handle fails closed with
    /// [`Errno::NotFound`], indistinguishable from one that never existed)
    /// and consumes the grant only once the descriptor allocation
    /// succeeds, so redemption is one-shot and a refused redemption leaves
    /// the grant intact.
    ///
    /// The default implementation fails closed with
    /// [`Errno::NotImplemented`]; the real handler is installed in
    /// `kernel/core`.
    fn fd_redeem(&self, _caller: &CallerContext<'_>, _handle: u64) -> SyscallResult {
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
    /// `set` is the wait-set handle; `op` is a [`tairix_abi::WaitSetOp`];
    /// `kind` is a [`tairix_abi::WaitSourceKind`]; `id` names the resource
    /// per the kind's own docs (an IPC call-endpoint id, an
    /// [`tairix_abi::IrqHandle`] raw value, a child PID, a seat id, a message
    /// port id, or a pipe-read descriptor of the caller's own open table);
    /// `token` is the caller's opaque tag. On `Add` the implementation **resolves and
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
    /// number of bytes written as a packed [`tairix_abi::DirEntry`] stream
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

    /// Write the [`tairix_abi::FileStat`] of open handle `fd` to the user
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
    /// The dispatcher has already checked the caller holds
    /// [`CapabilityId::FS_ACCESS`], that `path` is a non-null `UserPtr`, and
    /// rejected any reserved [`UnlinkFlags`] bit. With
    /// [`UnlinkFlags::DIRECTORY`] the removal succeeds only when the name is
    /// an (empty) directory — the atomic `rmdir` posture, decided by the
    /// filesystem under its own lock.
    ///
    /// The default implementation fails closed with [`Errno::NotImplemented`].
    fn fs_unlink(
        &self,
        _caller: &CallerContext<'_>,
        _path: u64,
        _path_len: usize,
        _flags: UnlinkFlags,
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

    /// Create a symbolic link at absolute `link` (`link_len` bytes) whose
    /// stored target is the `target_len` bytes at `target`.
    ///
    /// The dispatcher has already checked the caller holds
    /// [`CapabilityId::FS_ACCESS`] and that both pointers are non-null
    /// `UserPtr`s. The target is stored verbatim — it is data, not a path
    /// the kernel walks here — so authority is decided at each later *use*
    /// of the link, per component, never at creation.
    ///
    /// The default implementation fails closed with [`Errno::NotImplemented`].
    fn fs_symlink(
        &self,
        _caller: &CallerContext<'_>,
        _target: u64,
        _target_len: usize,
        _link: u64,
        _link_len: usize,
    ) -> SyscallResult {
        Err(Errno::NotImplemented)
    }

    /// Read the target of the symbolic link at absolute `path` (`path_len`
    /// bytes) into the caller's buffer at `out` (`out_len` bytes),
    /// returning the target's length.
    ///
    /// The dispatcher has already checked the caller holds
    /// [`CapabilityId::FS_ACCESS`] and that both pointers are non-null
    /// `UserPtr`s. The final component is never followed; a path whose
    /// final component is not a link fails closed.
    ///
    /// The default implementation fails closed with [`Errno::NotImplemented`].
    fn fs_readlink(
        &self,
        _caller: &CallerContext<'_>,
        _path: u64,
        _path_len: usize,
        _out: u64,
        _out_len: usize,
    ) -> SyscallResult {
        Err(Errno::NotImplemented)
    }

    /// Add the absolute path `link` (`link_len` bytes) as a second name for
    /// the node the absolute path `existing` (`existing_len` bytes) already
    /// names — a hard link.
    ///
    /// The dispatcher has already checked the caller holds
    /// [`CapabilityId::FS_ACCESS`], that both pointers are non-null
    /// `UserPtr`s, and rejected any reserved `flags` bit. An empty `flags`
    /// follows neither final component, so the node that gains a name is the
    /// one the caller spelled; [`LinkFlags::FOLLOW`] resolves the existing
    /// name's final link instead. The new name is authorised as a create in
    /// its own parent and confers no authority the caller did not already
    /// hold.
    ///
    /// The default implementation fails closed with [`Errno::NotImplemented`].
    fn fs_link(
        &self,
        _caller: &CallerContext<'_>,
        _existing: u64,
        _existing_len: usize,
        _link: u64,
        _link_len: usize,
        _flags: LinkFlags,
    ) -> SyscallResult {
        Err(Errno::NotImplemented)
    }

    /// Canonicalise the absolute path `path` (`path_len` bytes) into the
    /// caller's buffer at `out` (`out_len` bytes), returning the canonical
    /// path's length.
    ///
    /// The dispatcher has already checked the caller holds
    /// [`CapabilityId::FS_ACCESS`], that both pointers are non-null
    /// `UserPtr`s, and refused an undefined `mode`. Every component is
    /// resolved under the caller's attested identity — links followed, `..`
    /// applied to the nodes really traversed — so the answer is the one path
    /// the kernel's own resolution reaches; `mode` decides only how much of
    /// the path must exist.
    ///
    /// The default implementation fails closed with [`Errno::NotImplemented`].
    fn fs_realpath(
        &self,
        _caller: &CallerContext<'_>,
        _path: u64,
        _path_len: usize,
        _out: u64,
        _out_len: usize,
        _mode: RealpathMode,
    ) -> SyscallResult {
        Err(Errno::NotImplemented)
    }

    /// Set the permission bits of the file or directory at the absolute
    /// path `path` (`path_len` bytes) to `mode` (the `chmod(2)` shape).
    ///
    /// The dispatcher has already checked the caller holds
    /// [`CapabilityId::FS_ACCESS`], that `path` is a non-null `UserPtr`,
    /// and rejected any `mode` bit above [`FS_MODE_MASK`]. The per-inode
    /// rule — only the inode's owner may change its mode — is the secured
    /// VFS's, applied in the handler's filesystem service.
    ///
    /// The default implementation fails closed with [`Errno::NotImplemented`].
    fn fs_set_mode(
        &self,
        _caller: &CallerContext<'_>,
        _path: u64,
        _path_len: usize,
        _mode: u32,
    ) -> SyscallResult {
        Err(Errno::NotImplemented)
    }

    /// Set the owning user and/or group of the file or directory at the
    /// absolute path `path` (`path_len` bytes) to `uid` / `gid` (the
    /// `chown(2)` / `chgrp(2)` shape). Either field may be
    /// [`tairix_abi::fs::FS_OWNER_UNCHANGED`] to leave it unchanged.
    ///
    /// The dispatcher has already checked the caller holds
    /// [`CapabilityId::FS_ACCESS`] and that `path` is a non-null `UserPtr`.
    /// The per-inode rule — reassigning the uid, or setting a gid the caller
    /// is not a member of, requires [`CapabilityId::FS_CHOWN`]; otherwise
    /// only the owner may change the group, and only to a group they belong
    /// to; any change strips the set-*id* bits — is the secured VFS's,
    /// applied in the handler's filesystem service.
    ///
    /// The default implementation fails closed with [`Errno::NotImplemented`].
    fn fs_set_owner(
        &self,
        _caller: &CallerContext<'_>,
        _path: u64,
        _path_len: usize,
        _uid: u32,
        _gid: u32,
    ) -> SyscallResult {
        Err(Errno::NotImplemented)
    }

    /// Read one extended attribute of the file or directory at the
    /// absolute path `path` (`path_len` bytes) into the caller's
    /// `value_out` buffer, returning the value's byte count (the
    /// `getxattr(2)` shape).
    ///
    /// The dispatcher has already checked the caller holds
    /// [`CapabilityId::FS_ACCESS`], that the pointers are non-null
    /// `UserPtr`s, and bounded `key_len` to `1..=FS_ATTR_KEY_MAX`. The key
    /// grammar and the per-inode read-permission rule are the secured
    /// VFS's, applied in the handler's filesystem service.
    ///
    /// The default implementation fails closed with [`Errno::NotImplemented`].
    // The seam mirrors the six syscall argument registers one-to-one (three
    // pointer/length pairs), exactly as the dispatcher hands them over;
    // folding them into a struct would give this one syscall a different
    // shape from every sibling without removing any register.
    #[allow(clippy::too_many_arguments)]
    fn fs_attr_get(
        &self,
        _caller: &CallerContext<'_>,
        _path: u64,
        _path_len: usize,
        _key: u64,
        _key_len: usize,
        _value_out: u64,
        _value_out_len: usize,
    ) -> SyscallResult {
        Err(Errno::NotImplemented)
    }

    /// Set one extended attribute of the file or directory at the absolute
    /// path `path` to the `value_len` bytes at `value` (the `setxattr(2)`
    /// shape).
    ///
    /// The dispatcher has already checked [`CapabilityId::FS_ACCESS`],
    /// the pointers, `key_len` (`1..=FS_ATTR_KEY_MAX`), and `value_len`
    /// (at most [`FS_ATTR_VALUE_MAX`]). Write permission and the key
    /// grammar are the secured VFS's.
    ///
    /// The default implementation fails closed with [`Errno::NotImplemented`].
    // The seam mirrors the six syscall argument registers one-to-one (three
    // pointer/length pairs), exactly as the dispatcher hands them over;
    // folding them into a struct would give this one syscall a different
    // shape from every sibling without removing any register.
    #[allow(clippy::too_many_arguments)]
    fn fs_attr_set(
        &self,
        _caller: &CallerContext<'_>,
        _path: u64,
        _path_len: usize,
        _key: u64,
        _key_len: usize,
        _value: u64,
        _value_len: usize,
    ) -> SyscallResult {
        Err(Errno::NotImplemented)
    }

    /// Yield the `index`-th visible extended-attribute key of the file or
    /// directory at the absolute path `path` into the caller's `key_out`
    /// buffer, returning the key's byte count or `0` past the end.
    ///
    /// The dispatcher has already checked [`CapabilityId::FS_ACCESS`] and
    /// the pointers. Read permission and the visibility filtering are the
    /// secured VFS's.
    ///
    /// The default implementation fails closed with [`Errno::NotImplemented`].
    fn fs_attr_list(
        &self,
        _caller: &CallerContext<'_>,
        _path: u64,
        _path_len: usize,
        _index: u64,
        _key_out: u64,
        _key_out_len: usize,
    ) -> SyscallResult {
        Err(Errno::NotImplemented)
    }

    /// Remove one extended attribute of the file or directory at the
    /// absolute path `path` (the `removexattr(2)` shape).
    ///
    /// The dispatcher has already checked [`CapabilityId::FS_ACCESS`],
    /// the pointers, and `key_len` (`1..=FS_ATTR_KEY_MAX`). Write
    /// permission and the key grammar are the secured VFS's.
    ///
    /// The default implementation fails closed with [`Errno::NotImplemented`].
    fn fs_attr_remove(
        &self,
        _caller: &CallerContext<'_>,
        _path: u64,
        _path_len: usize,
        _key: u64,
        _key_len: usize,
    ) -> SyscallResult {
        Err(Errno::NotImplemented)
    }

    /// Resolve a published port name to its live IPC endpoint id.
    ///
    /// The dispatcher has already checked that `name` is a non-null
    /// `UserPtr` and decoded `name_len`. The implementation copies the
    /// name bytes in through the validated `copy_from_user` boundary,
    /// validates them against the `tairix_abi::PortName` grammar (fail
    /// closed — malformed bytes are refused before the registry is
    /// consulted), and looks the name up in the named-port registry,
    /// returning the bound endpoint id or [`Errno::NotFound`] when no
    /// port is currently published under that name. Resolution grants
    /// nothing: every send is still capability-checked at the port.
    ///
    /// The default implementation fails closed with [`Errno::NotImplemented`].
    fn port_resolve(
        &self,
        _caller: &CallerContext<'_>,
        _name: u64,
        _name_len: usize,
    ) -> SyscallResult {
        Err(Errno::NotImplemented)
    }

    /// Bind an asynchronous IPC message port owned by the calling task
    /// (the receive half of `ipc_send`/`ipc_recv`).
    ///
    /// The implementation bounds `max_payload` and `capacity` against the
    /// ABI message ceilings, requires `CAP_IPC_BIND_PRIVILEGED` for a
    /// reserved well-known id (squat protection, exactly as `call_create`),
    /// refuses an id that is already bound (`Errno::AlreadyExists`),
    /// records the kernel-trusted caller as the port's owner — the only
    /// task that may receive from it or observe it through a wait-set —
    /// and tears the port down when that owner exits.
    ///
    /// The default implementation fails closed with [`Errno::NotImplemented`].
    fn port_bind(
        &self,
        _caller: &CallerContext<'_>,
        _endpoint: u64,
        _max_payload: usize,
        _capacity: usize,
    ) -> SyscallResult {
        Err(Errno::NotImplemented)
    }

    /// Map `len` bytes of open file `fd`, starting at the page-aligned
    /// file byte `offset`, into the caller's own address space as a
    /// demand-paged, read-only private mapping, returning its base
    /// address (the `mmap(2)` shape).
    ///
    /// The dispatcher has already checked the caller holds
    /// [`CapabilityId::FS_ACCESS`]. The implementation resolves `fd`
    /// against the caller's table (open for reading, path-backed),
    /// validates the alignment and extent, reserves the region, and
    /// records the mapping-time identity the fault path pages under.
    ///
    /// The default implementation fails closed with [`Errno::NotImplemented`].
    fn file_map(
        &self,
        _caller: &CallerContext<'_>,
        _fd: u32,
        _offset: u64,
        _len: u64,
    ) -> SyscallResult {
        Err(Errno::NotImplemented)
    }

    /// Release the whole file mapping based at `base` (`len` bytes, as
    /// requested at map time) from the caller's own address space.
    ///
    /// The default implementation fails closed with [`Errno::NotImplemented`].
    fn file_unmap(&self, _caller: &CallerContext<'_>, _base: u64, _len: u64) -> SyscallResult {
        Err(Errno::NotImplemented)
    }

    /// Attach a filesystem driver to a runtime block source and publish
    /// the volume's root (`plans/DEVICES.md` D3b).
    ///
    /// The dispatcher has already checked the caller holds
    /// [`CapabilityId::FS_MOUNT`]. `request`/`request_len` name the
    /// caller's encoded `VolumeAttachRequest`; the implementation copies
    /// it in, decodes it fail-closed, and re-validates every field
    /// against live state before mounting anything.
    ///
    /// The default implementation fails closed with [`Errno::NotImplemented`].
    fn volume_attach(
        &self,
        _caller: &CallerContext<'_>,
        _request: u64,
        _request_len: usize,
    ) -> SyscallResult {
        Err(Errno::NotImplemented)
    }

    /// Detach a runtime-attached volume: flush it, retract its mount, and
    /// unpublish its root (`plans/DEVICES.md` D3b).
    ///
    /// The dispatcher has already checked the caller holds
    /// [`CapabilityId::FS_MOUNT`]. `request`/`request_len` name the
    /// caller's encoded `VolumeDetachRequest` (the volume's stable
    /// identity plus the force byte).
    ///
    /// The default implementation fails closed with [`Errno::NotImplemented`].
    fn volume_detach(
        &self,
        _caller: &CallerContext<'_>,
        _request: u64,
        _request_len: usize,
    ) -> SyscallResult {
        Err(Errno::NotImplemented)
    }

    /// End the machine's power state: flush every mounted volume, then
    /// power the platform off or reset it (`plans/NEW-TASKBAR.md` T13).
    ///
    /// The dispatcher has already checked the caller holds
    /// [`CapabilityId::SYSTEM_POWER`] and decoded `action` against the
    /// closed [`PowerAction`] set. The implementation flushes every
    /// mounted volume before it asks the platform to stop; a flush failure
    /// aborts the transition with the machine still running.
    ///
    /// The default implementation fails closed with [`Errno::NotImplemented`].
    fn system_power(&self, _caller: &CallerContext<'_>, _action: PowerAction) -> SyscallResult {
        Err(Errno::NotImplemented)
    }

    /// Read the operator's one-boot login choice
    /// ([`tairix_abi::BootSession`]), recorded once by the boot path from the
    /// pre-boot Supervisor's `continue text` / `continue gui`.
    ///
    /// No arguments and no capability (the choice is public boot-static
    /// state: it names no account, grants no authority, and reveals no
    /// secret). The implementation returns the recorded discriminant.
    ///
    /// Required rather than defaulted, like [`Self::clock_get`]: there is
    /// always an answer — a boot that never entered the Supervisor reports
    /// [`tairix_abi::BootSession::Unset`] — so there is no unwired state to
    /// fail closed from.
    fn boot_session_get(&self, caller: &CallerContext<'_>) -> SyscallResult;

    /// Create a second thread of execution inside the calling process's own
    /// address space, returning its thread id (`plans/THREADS.md` T3b).
    ///
    /// The dispatcher has already checked that `entry` is a non-null `UserPtr`
    /// and that `stack_len` fits in `usize`; it attaches **no** capability gate,
    /// because a thread runs in the caller's *own* isolated space under the
    /// caller's *own* single capability record and so grants no authority over
    /// anything else. The implementation must therefore bound the request
    /// instead: refuse a process already at its `threads` limit and a
    /// `stack_len` past its `stack-bytes` limit **before** touching any state,
    /// and validate that `entry`, `tls_base`, and `clear_on_exit` name memory of
    /// the caller's own address space (a non-canonical `tls_base` is not merely
    /// useless — on a port whose thread-pointer register is privileged, writing
    /// one would fault inside the kernel).
    ///
    /// `stack_len` of [`tairix_abi::THREAD_STACK_DEFAULT`] asks for the caller's
    /// effective `stack-bytes` bound. The **kernel** reserves the stack, behind
    /// an unbacked guard page, and releases it when the thread dies; user space
    /// supplies no base. `clear_on_exit`, when non-zero, names a `u32` the
    /// implementation zeroes and futex-wakes on that death, which is what a
    /// userland `join` blocks on.
    ///
    /// The default implementation fails closed with [`Errno::NotImplemented`].
    fn thread_create(
        &self,
        _caller: &CallerContext<'_>,
        _entry: u64,
        _arg: u64,
        _stack_len: usize,
        _tls_base: u64,
        _clear_on_exit: u64,
    ) -> SyscallResult {
        Err(Errno::NotImplemented)
    }

    /// End the calling thread without ending its siblings
    /// (`plans/THREADS.md` T3b).
    ///
    /// No arguments and no capability (ending oneself grants nothing). The
    /// implementation zeroes and futex-wakes the thread's `clear_on_exit` word,
    /// releases its stack and per-thread kernel state, and — when it was the
    /// **last** thread of its process — performs the whole process exit with
    /// status `0`. The dispatch boundary turns this syscall number into the
    /// scheduler `Exit` that reaps the task, exactly as it does for
    /// [`Self::exit`], so the implementation never drives the scheduler itself.
    ///
    /// The default implementation fails closed with [`Errno::NotImplemented`].
    fn thread_exit(&self, _caller: &CallerContext<'_>) -> SyscallResult {
        Err(Errno::NotImplemented)
    }

    /// Block until the 32-bit word at `uaddr` is woken, unless it no longer
    /// holds `expected` (`plans/THREADS.md` decision 5).
    ///
    /// The dispatcher has already checked that `uaddr` is a non-null `UserPtr`
    /// and decoded `expected`/`timeout_ns`; it attaches no capability gate,
    /// because the wait key is `(process, uaddr)` and so names nothing outside
    /// the caller's own address space. The implementation must reject a
    /// misaligned `uaddr` and read the word through the validated
    /// `copy_from_user` boundary — never a raw dereference — registering on the
    /// wait queue **before** that read so a wake landing in the window between
    /// the read and the park is not lost.
    ///
    /// Returns `Ok(0)` when woken, [`Errno::WouldBlock`] when the word already
    /// holds something else (the caller re-tests and retries — this is the race
    /// closing, not a failure), [`Errno::TimedOut`] when the relative
    /// `timeout_ns` elapses ([`u64::MAX`] means no timeout), and
    /// [`Errno::Interrupted`] when the thread is being terminated.
    ///
    /// The default implementation fails closed with [`Errno::NotImplemented`].
    fn futex_wait(
        &self,
        _caller: &CallerContext<'_>,
        _uaddr: u64,
        _expected: u32,
        _timeout_ns: u64,
    ) -> SyscallResult {
        Err(Errno::NotImplemented)
    }

    /// Wake up to `count` threads of the calling process blocked in
    /// [`Self::futex_wait`] on `uaddr`, returning how many were woken.
    ///
    /// The dispatcher has already checked that `uaddr` is a non-null `UserPtr`
    /// and decoded `count`. Waiters are released oldest-first, so a `count` of 1
    /// is a genuine wake-one rather than a thundering herd, and
    /// [`u32::MAX`] wakes every waiter. Waking nobody is success: by the
    /// register-before-retest discipline a thread that has not parked yet
    /// re-tests the word itself.
    ///
    /// The default implementation fails closed with [`Errno::NotImplemented`].
    fn futex_wake(&self, _caller: &CallerContext<'_>, _uaddr: u64, _count: u32) -> SyscallResult {
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
    ///   [`tairix_abi::SYSCALLS`] is assigned at that index (no gaps in
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

        // step 2: sandbox confinement, then capability check. Confinement
        // first: a sandboxed task's answer never depends on what the
        // syscall would have required, only on the closed allow-list.
        if caller.caps.is_sandboxed() && !sandbox_allows(number) {
            self.audit_denied(caller, spec);
            return Err(Errno::PermissionDenied);
        }
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
            // `NotFound` is the "no such object" answer, not a rejection
            // either: every check passed and the handler answered a
            // legitimate question (a genuine authorisation refusal is
            // `PermissionDenied` — the VFS never masks one as `NotFound`).
            // Record it below the error level so a routine existence probe
            // (e.g. `login` opening the optional system-configuration store
            // and the desktop bundle each round) cannot flood the log.
            Err(Errno::NotFound) if spec.audit => self.audit_not_found(caller, spec),
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
                self.handlers
                    .ipc_recv(caller, args.0[0], args.0[1], len, args.0[3])
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
                // args[2] and args[3] are the optional attach block (zero
                // address = absent — full inherit): the encoded
                // `SpawnAttach` selecting the child's credential, base
                // console, and per-descriptor wires; the handler stages and
                // parses it fail-closed and owner-checks every named
                // handle. args[4] and args[5] are the optional
                // startup-strings block (zero address = absent); the
                // handler bounds and parses it fail-closed.
                let attach_len = decode_len(args.0[3])?;
                let strings_len = decode_len(args.0[5])?;
                self.handlers.spawn(
                    caller,
                    args.0[0],
                    len,
                    args.0[2],
                    attach_len,
                    args.0[4],
                    strings_len,
                )
            }
            SyscallNumber::STREAM_READ => {
                let len = decode_len(args.0[2])?;
                self.handlers
                    .stream_read(caller, decode_u32(args.0[0]), args.0[1], len, args.0[3])
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
            SyscallNumber::THREAD_CREATE => {
                let stack_len = decode_len(args.0[2])?;
                self.handlers.thread_create(
                    caller, args.0[0], args.0[1], stack_len, args.0[3], args.0[4],
                )
            }
            SyscallNumber::THREAD_EXIT => self.handlers.thread_exit(caller),
            SyscallNumber::FUTEX_WAIT => {
                self.handlers
                    .futex_wait(caller, args.0[0], decode_u32(args.0[1]), args.0[2])
            }
            SyscallNumber::FUTEX_WAKE => {
                self.handlers
                    .futex_wake(caller, args.0[0], decode_u32(args.0[1]))
            }
            SyscallNumber::MEM_PIN => self.handlers.mem_pin(caller),
            SyscallNumber::MEM_UNPIN => self.handlers.mem_unpin(caller),
            SyscallNumber::SIGNAL_INTAKE => {
                // args[0] is the `SignalIntakeOp` discriminant, rejected
                // before dispatch if it is not one of the closed set (fail
                // closed on an unknown or out-of-range value).
                let op = SignalIntakeOp::from_u32(decode_u32(args.0[0]))?;
                self.handlers.signal_intake(caller, op)
            }
            SyscallNumber::SCHED_SET_REALTIME => {
                // args[0] is a `u32` boolean: non-zero enters the
                // strict-priority real-time class, zero returns to fair.
                let realtime = decode_u32(args.0[0]) != 0;
                self.handlers.sched_set_realtime(caller, realtime)
            }
            SyscallNumber::SCHED_SET_PRIORITY => {
                // args[0] is a sign-extended `i32` PID recovered the same way
                // `WAIT`/`SIGNAL` recover theirs; args[1] is the
                // `SchedPriority` discriminant, rejected before dispatch if it
                // is not one of the closed set (fail closed on an unknown or
                // zeroed value).
                #[allow(clippy::cast_possible_wrap)]
                let pid = (args.0[0] & 0xFFFF_FFFF) as i32;
                let priority = SchedPriority::from_u32(decode_u32(args.0[1]))?;
                self.handlers.sched_set_priority(caller, pid, priority)
            }
            SyscallNumber::SYSTEM_POWER => {
                // args[0] is the `PowerAction` discriminant, rejected before
                // dispatch if it is not one of the closed set (fail closed on
                // an unknown or zeroed value).
                let action = PowerAction::from_u32(decode_u32(args.0[0]))?;
                self.handlers.system_power(caller, action)
            }
            SyscallNumber::BOOT_SESSION_GET => self.handlers.boot_session_get(caller),
            SyscallNumber::WAIT => {
                // `validate_arg` guarantees args[0] is a sign-extended
                // `i32`; recover it by truncating the low 32 bits (the
                // same recovery `EXIT` uses), and args[1] is a non-null
                // `UserPtr`. `from_bits` rejects any reserved flag bit.
                #[allow(clippy::cast_possible_wrap)]
                let pid = (args.0[0] & 0xFFFF_FFFF) as i32;
                let flags = WaitFlags::from_bits(decode_u32(args.0[2]))?;
                self.handlers.wait(caller, pid, args.0[1], flags)
            }
            SyscallNumber::SIGNAL => {
                // args[0] is a sign-extended `i32` PID recovered the same way
                // `WAIT`/`EXIT` recover theirs; args[1] is the `Signal`
                // discriminant, rejected before dispatch if it is not one of
                // the closed set (fail closed on an unknown or zeroed value).
                #[allow(clippy::cast_possible_wrap)]
                let pid = (args.0[0] & 0xFFFF_FFFF) as i32;
                let signal = Signal::from_u32(decode_u32(args.0[1]))?;
                self.handlers.signal(caller, pid, signal)
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
            SyscallNumber::USERS_ADMIN => {
                // `validate_arg` guarantees args[0] and args[2] are non-null
                // `UserPtr`s; args[1] is the request length and args[3] the
                // response-buffer capacity.
                let req_len = decode_len(args.0[1])?;
                let out_cap = decode_len(args.0[3])?;
                self.handlers
                    .users_admin(caller, args.0[0], req_len, args.0[2], out_cap)
            }
            SyscallNumber::SEAT_SWITCH => {
                // `validate_arg` guarantees args[1] fits `u32`.
                self.handlers
                    .seat_switch(caller, args.0[0], decode_u32(args.0[1]))
            }
            SyscallNumber::SEAT_REVOKE => self.handlers.seat_revoke(caller, args.0[0]),
            SyscallNumber::CONSOLE_COUNT => self.handlers.console_count(caller),
            SyscallNumber::STREAM_INPUT_MODE => self.handlers.stream_input_mode(
                caller,
                decode_u32(args.0[0]),
                decode_u32(args.0[1]),
            ),
            SyscallNumber::CONSOLE_FOREGROUND => {
                // args[0] is the readable descriptor naming the console;
                // args[1] is a sign-extended `i32` PID recovered the same
                // way `WAIT`/`SIGNAL` recover theirs (`0` clears the slot).
                #[allow(clippy::cast_possible_wrap)]
                let pid = (args.0[1] & 0xFFFF_FFFF) as i32;
                self.handlers
                    .console_foreground(caller, decode_u32(args.0[0]), pid)
            }
            SyscallNumber::TERMINAL_PURGE => {
                // args[0] is the descriptor naming the terminal to purge;
                // `validate_arg` guarantees it fits `u32`.
                self.handlers.terminal_purge(caller, decode_u32(args.0[0]))
            }
            SyscallNumber::PIPE_CREATE => {
                // args[0] is the non-null `UserPtr` (dispatcher-checked)
                // the handler writes the two new descriptors through.
                self.handlers.pipe_create(caller, args.0[0])
            }
            SyscallNumber::PTY_CREATE => {
                // args[0] is the non-null `UserPtr` (dispatcher-checked) the
                // handler writes the two new descriptors through; args[1] and
                // args[2] are the initial row/column geometry (bounds
                // validated in the handler).
                self.handlers.pty_create(
                    caller,
                    args.0[0],
                    decode_u32(args.0[1]),
                    decode_u32(args.0[2]),
                )
            }
            SyscallNumber::PTY_SET_SIZE => {
                // args[0] is the pty master descriptor; args[1] and args[2]
                // are the new row/column geometry (master resolution and
                // bounds validated in the handler).
                self.handlers.pty_set_size(
                    caller,
                    decode_u32(args.0[0]),
                    decode_u32(args.0[1]),
                    decode_u32(args.0[2]),
                )
            }
            SyscallNumber::KEY_INJECT => {
                // args[0] is the seat id; `validate_arg` guarantees args[1]
                // is a non-null `UserPtr`; args[2] is the record length.
                let len = decode_len(args.0[2])?;
                self.handlers.key_inject(caller, args.0[0], args.0[1], len)
            }
            SyscallNumber::DISPLAY_ACQUIRE => {
                // args[0] is the seat id to acquire.
                self.handlers.display_acquire(caller, args.0[0])
            }
            SyscallNumber::DISPLAY_RELEASE => {
                // args[0] is the seat id to release; args[1] says what its
                // screen becomes, refused before any state is touched when
                // it is outside the closed set.
                let next = ReleaseSurface::from_u64(args.0[1])?;
                self.handlers.display_release(caller, args.0[0], next)
            }
            SyscallNumber::KEYBOARD_READ => {
                // args[0] is the seat id; `validate_arg` guarantees args[1]
                // is a non-null `UserPtr`; args[2] is the buffer capacity.
                let len = decode_len(args.0[2])?;
                self.handlers
                    .keyboard_read(caller, args.0[0], args.0[1], len)
            }
            SyscallNumber::POINTER_INJECT => {
                // args[0] is the seat id; `validate_arg` guarantees args[1]
                // is a non-null `UserPtr`; args[2] is the record length.
                let len = decode_len(args.0[2])?;
                self.handlers
                    .pointer_inject(caller, args.0[0], args.0[1], len)
            }
            SyscallNumber::POINTER_READ => {
                // args[0] is the seat id; `validate_arg` guarantees args[1]
                // is a non-null `UserPtr`; args[2] is the buffer capacity.
                let len = decode_len(args.0[2])?;
                self.handlers
                    .pointer_read(caller, args.0[0], args.0[1], len)
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
                // args[2] is the request-buffer capacity; args[4] carries the
                // `CallRecvFlags` bits, rejected fail-closed on any reserved
                // bit before the handler runs.
                let buf_cap = decode_len(args.0[2])?;
                let flags = CallRecvFlags::from_bits(decode_u32(args.0[4]))?;
                self.handlers
                    .call_recv(caller, args.0[0], args.0[1], buf_cap, args.0[3], flags)
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
            SyscallNumber::CALL_POST => {
                // args[0] is the endpoint id; args[1] is a non-null request
                // `UserPtr` (dispatcher-checked); args[2] the request length;
                // args[3] a non-null ticket-out `UserPtr`; args[4] the
                // relative deadline in nanoseconds (`u64::MAX` = none).
                let request_len = decode_len(args.0[2])?;
                self.handlers.call_post(
                    caller,
                    args.0[0],
                    args.0[1],
                    request_len,
                    args.0[3],
                    args.0[4],
                )
            }
            SyscallNumber::CALL_REAP => {
                // args[0] is the endpoint id; args[1] the ticket; args[2] a
                // non-null reply `UserPtr` (dispatcher-checked); args[3] the
                // reply-buffer capacity.
                let reply_cap = decode_len(args.0[3])?;
                self.handlers
                    .call_reap(caller, args.0[0], args.0[1], args.0[2], reply_cap)
            }
            SyscallNumber::CALL_CANCEL => {
                // args[0] is the endpoint id; args[1] the ticket to withdraw.
                self.handlers.call_cancel(caller, args.0[0], args.0[1])
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
                // resolved against the live tree by the handler); args[1] is
                // the `HwRemoveFlags` word, validated in the handler.
                self.handlers.hw_remove_node(caller, args.0[0], args.0[1])
            }
            SyscallNumber::HW_NODE_HEALTH => {
                // args[0] is the `FaultDomainState` discriminant of the
                // caller's own node's new health (a plain `u64`, validated and
                // resolved against the caller's matched node by the handler).
                self.handlers.hw_node_health(caller, args.0[0])
            }
            SyscallNumber::HW_SELF_NODE => {
                // No arguments: the handler resolves the caller's own matched
                // node from its task id and returns that node id.
                self.handlers.hw_self_node(caller)
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
                // args[0] is an opaque `Handle` u64 the handler resolves
                // against the calling task and the grant table (forgery +
                // ownership are checked there); args[1] is the non-null
                // `len_out` `UserPtr` the handler writes the mapped region's
                // byte length to (dispatcher-checked).
                self.handlers.shm_map(caller, args.0[0], args.0[1])
            }
            SyscallNumber::SHM_UNMAP => {
                // args[0] is the base virtual address the map returned; args[1]
                // is its length in bytes.
                let len = decode_len(args.0[1])?;
                self.handlers.shm_unmap(caller, args.0[0], len)
            }
            SyscallNumber::SHM_GRANT => {
                // args[0] is the shared-region id the caller owns; args[1] is
                // the call-endpoint id whose serving task receives the grant
                // (both resolved and owner-checked by the handler).
                self.handlers.shm_grant(caller, args.0[0], args.0[1])
            }
            SyscallNumber::CALL_GRANT => {
                // args[0] is the call-endpoint id the caller already holds a
                // grant for; args[1] is the call-endpoint id whose serving
                // task receives the delegated grant (both resolved and
                // owner-checked by the handler).
                self.handlers.call_grant(caller, args.0[0], args.0[1])
            }
            SyscallNumber::CALL_PEER_SEAT => {
                // args[0] is the endpoint id; args[1] the in-service ticket;
                // args[2] the seat id whose live lease is checked against
                // the ticket's peer.
                self.handlers
                    .call_peer_seat(caller, args.0[0], args.0[1], args.0[2])
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
                // args[0] is the non-null path `UserPtr` (dispatcher-checked);
                // args[1] is the path length; args[2] is the `UnlinkFlags`
                // bits, rejected here for any reserved bit.
                let path_len = decode_len(args.0[1])?;
                let flags = UnlinkFlags::from_bits(decode_u32(args.0[2]))?;
                self.handlers.fs_unlink(caller, args.0[0], path_len, flags)
            }
            SyscallNumber::FS_RENAME => {
                let src_len = decode_len(args.0[1])?;
                let dst_len = decode_len(args.0[3])?;
                self.handlers
                    .fs_rename(caller, args.0[0], src_len, args.0[2], dst_len)
            }
            SyscallNumber::FS_SYMLINK => {
                let target_len = decode_len(args.0[1])?;
                let link_len = decode_len(args.0[3])?;
                self.handlers
                    .fs_symlink(caller, args.0[0], target_len, args.0[2], link_len)
            }
            SyscallNumber::FS_READLINK => {
                let path_len = decode_len(args.0[1])?;
                let out_len = decode_len(args.0[3])?;
                self.handlers
                    .fs_readlink(caller, args.0[0], path_len, args.0[2], out_len)
            }
            SyscallNumber::FS_LINK => {
                // args[0]/args[2] are the non-null path `UserPtr`s
                // (dispatcher-checked); args[4] is the `LinkFlags` bits,
                // rejected here for any reserved bit.
                let existing_len = decode_len(args.0[1])?;
                let link_len = decode_len(args.0[3])?;
                let flags = LinkFlags::from_bits(decode_u32(args.0[4]))?;
                self.handlers
                    .fs_link(caller, args.0[0], existing_len, args.0[2], link_len, flags)
            }
            SyscallNumber::FS_REALPATH => {
                // args[0]/args[2] are the non-null `UserPtr`s
                // (dispatcher-checked); args[4] selects how much of the path
                // must exist, refused here for any undefined value.
                let path_len = decode_len(args.0[1])?;
                let out_len = decode_len(args.0[3])?;
                let mode = RealpathMode::from_raw(decode_u32(args.0[4]))?;
                self.handlers
                    .fs_realpath(caller, args.0[0], path_len, args.0[2], out_len, mode)
            }
            SyscallNumber::FS_SET_MODE => {
                // args[0] is the non-null path `UserPtr` (dispatcher-checked);
                // args[1] is the path length; args[2] is the mode word,
                // refused here for any bit above the permission mask (never
                // masked to a mode the caller did not ask for).
                let path_len = decode_len(args.0[1])?;
                let mode = decode_u32(args.0[2]);
                if mode & !FS_MODE_MASK != 0 {
                    return Err(Errno::OutOfRange);
                }
                self.handlers.fs_set_mode(caller, args.0[0], path_len, mode)
            }
            SyscallNumber::FS_SET_OWNER => {
                // args[0] is the non-null path `UserPtr` (dispatcher-checked);
                // args[1] is the path length; args[2]/args[3] are the new
                // uid/gid (either may be `FS_OWNER_UNCHANGED`). The whole
                // authority rule is the secured VFS's; no value needs
                // rejecting here (an id is any `u32`, the sentinel included).
                let path_len = decode_len(args.0[1])?;
                let uid = decode_u32(args.0[2]);
                let gid = decode_u32(args.0[3]);
                self.handlers
                    .fs_set_owner(caller, args.0[0], path_len, uid, gid)
            }
            SyscallNumber::FS_ATTR_GET => {
                // args[0]/args[2]/args[4] are non-null `UserPtr`s
                // (dispatcher-checked). The key length is bounded here so no
                // user memory beyond the fixed key bound is ever staged; the
                // key grammar itself is the secured VFS's to judge.
                let path_len = decode_len(args.0[1])?;
                let key_len = decode_attr_key_len(args.0[3])?;
                let value_out_len = decode_len(args.0[5])?;
                self.handlers.fs_attr_get(
                    caller,
                    args.0[0],
                    path_len,
                    args.0[2],
                    key_len,
                    args.0[4],
                    value_out_len,
                )
            }
            SyscallNumber::FS_ATTR_SET => {
                // As `fs_attr_get`, plus the value bound: a payload above
                // the fixed attribute-value maximum is refused before any
                // copy (a larger payload is a named stream, never silently
                // truncated into an attribute).
                let path_len = decode_len(args.0[1])?;
                let key_len = decode_attr_key_len(args.0[3])?;
                let value_len = decode_len(args.0[5])?;
                if value_len > FS_ATTR_VALUE_MAX {
                    return Err(Errno::LengthOutOfRange);
                }
                self.handlers.fs_attr_set(
                    caller, args.0[0], path_len, args.0[2], key_len, args.0[4], value_len,
                )
            }
            SyscallNumber::FS_ATTR_LIST => {
                // args[0]/args[3] are non-null `UserPtr`s
                // (dispatcher-checked); args[2] is the enumeration index.
                let path_len = decode_len(args.0[1])?;
                let key_out_len = decode_len(args.0[4])?;
                self.handlers.fs_attr_list(
                    caller,
                    args.0[0],
                    path_len,
                    args.0[2],
                    args.0[3],
                    key_out_len,
                )
            }
            SyscallNumber::FS_ATTR_REMOVE => {
                let path_len = decode_len(args.0[1])?;
                let key_len = decode_attr_key_len(args.0[3])?;
                self.handlers
                    .fs_attr_remove(caller, args.0[0], path_len, args.0[2], key_len)
            }
            SyscallNumber::PORT_BIND => {
                let max_payload = decode_len(args.0[1])?;
                let capacity = decode_len(args.0[2])?;
                self.handlers
                    .port_bind(caller, args.0[0], max_payload, capacity)
            }
            SyscallNumber::PORT_RESOLVE => {
                // args[0] is the non-null name `UserPtr`
                // (dispatcher-checked); args[1] is the name length in
                // bytes. The grammar and length bound are the handler's to
                // enforce after the copy-in.
                let name_len = decode_len(args.0[1])?;
                self.handlers.port_resolve(caller, args.0[0], name_len)
            }
            SyscallNumber::FILE_MAP => {
                // args[0] is the descriptor; args[1] the page-aligned file
                // offset; args[2] the length in bytes. Offset and length
                // stay 64-bit end to end (storage width is never pointer
                // width); the handler validates alignment and extent.
                self.handlers
                    .file_map(caller, decode_u32(args.0[0]), args.0[1], args.0[2])
            }
            SyscallNumber::FILE_UNMAP => {
                // args[0] is the region base; args[1] its full length.
                self.handlers.file_unmap(caller, args.0[0], args.0[1])
            }
            SyscallNumber::VOLUME_ATTACH => {
                // args[0] is the non-null request `UserPtr`
                // (dispatcher-checked); args[1] is its length. The frame
                // grammar and bounds are the handler's to enforce after
                // the copy-in.
                let request_len = decode_len(args.0[1])?;
                self.handlers.volume_attach(caller, args.0[0], request_len)
            }
            SyscallNumber::VOLUME_DETACH => {
                // args[0] is the non-null request `UserPtr`
                // (dispatcher-checked); args[1] is its exact length.
                let request_len = decode_len(args.0[1])?;
                self.handlers.volume_detach(caller, args.0[0], request_len)
            }
            SyscallNumber::FS_CHDIR => {
                // args[0] is the non-null path `UserPtr` (dispatcher-checked);
                // args[1] is the path length.
                let path_len = decode_len(args.0[1])?;
                self.handlers.fs_chdir(caller, args.0[0], path_len)
            }
            SyscallNumber::FS_GETCWD => {
                // args[0] is the non-null out `UserPtr` (dispatcher-checked);
                // args[1] is its capacity in bytes.
                let out_cap = decode_len(args.0[1])?;
                self.handlers.fs_getcwd(caller, args.0[0], out_cap)
            }
            SyscallNumber::RESOURCE_OPEN => {
                // args[0] is the non-null reference `UserPtr`
                // (dispatcher-checked); args[1] is the reference length;
                // args[2] is the `OpenFlags` bits, rejected here for any
                // reserved/illegal combination.
                let reference_len = decode_len(args.0[1])?;
                let flags = OpenFlags::from_bits(decode_u32(args.0[2]))?;
                self.handlers
                    .resource_open(caller, args.0[0], reference_len, flags)
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
            SyscallNumber::BOOT_FACTS_GET => {
                // args[0] is the non-null out `UserPtr` (dispatcher-checked);
                // args[1] is its capacity in bytes.
                let out_cap = decode_len(args.0[1])?;
                self.handlers.boot_facts_get(caller, args.0[0], out_cap)
            }
            SyscallNumber::FD_GRANT => {
                // args[0] is the caller's own path-backed descriptor;
                // args[1] is the recipient's kernel task id (both resolved
                // and owner-/liveness-checked by the handler); args[2] is
                // the write-extent ceiling, which the handler checks against
                // the descriptor's own access.
                let fd = decode_u32(args.0[0]);
                self.handlers.fd_grant(caller, fd, args.0[1], args.0[2])
            }
            SyscallNumber::FD_REDEEM => {
                // args[0] is the grant handle minted to the calling task
                // (resolved owner-bound by the handler; one-shot).
                self.handlers.fd_redeem(caller, args.0[0])
            }
            SyscallNumber::SYSINFO_INTROSPECT => {
                // args[0] is the `IntrospectDomain` discriminant (validated by
                // the handler); args[1] is the domain-specific selector/offset;
                // args[2] is the non-null out `UserPtr` (dispatcher-checked);
                // args[3] is its capacity in bytes.
                let domain = decode_u32(args.0[0]);
                let out_cap = decode_len(args.0[3])?;
                self.handlers
                    .sysinfo_introspect(caller, domain, args.0[1], args.0[2], out_cap)
            }
            SyscallNumber::TERMINAL_SIZE => {
                // args[0] is the descriptor to query; args[1] is the non-null
                // out `UserPtr` (dispatcher-checked); args[2] is its capacity
                // in bytes.
                let fd = decode_u32(args.0[0]);
                let out_cap = decode_len(args.0[2])?;
                self.handlers.terminal_size(caller, fd, args.0[1], out_cap)
            }
            SyscallNumber::SELF_ORIGIN => {
                // args[0] is the non-null out `UserPtr` (dispatcher-checked);
                // args[1] is its capacity in bytes.
                let out_cap = decode_len(args.0[1])?;
                self.handlers.self_origin(caller, args.0[0], out_cap)
            }
            _ => Err(Errno::NotFound),
        }
    }

    /// Emit a security-relevant dispatcher audit record carrying the
    /// caller's kernel-attested identity prefix — numeric task id,
    /// process-instance id, parent process-instance id, process name
    /// (`comm`), and monotonic admission time (`start`) — followed by the
    /// site-specific `extra` fields.
    ///
    /// One definition of the identity prefix, so the audit sites cannot
    /// drift in which attested fields they record or how they render them.
    /// Every prefix field is read from the kernel-attested [`CallerContext`],
    /// never from caller-supplied bytes. `extra` carries at most two fields
    /// at every call site, which the fixed seven-slot buffer accommodates
    /// alongside the five identity fields.
    fn audit_with_identity(
        &self,
        event: AuditEvent,
        caller: &CallerContext<'_>,
        extra: &[Field<'_>],
    ) {
        let mut t = [0u8; 16];
        let mut p = [0u8; PROC_ID_HEX_LEN];
        let mut pp = [0u8; PROC_ID_HEX_LEN];
        let mut fields = [Field {
            key: "",
            value: tairix_log::FieldValue::Null,
        }; 7];
        fields[0] = Field {
            key: "task",
            value: tairix_log::FieldValue::Str(format_hex_u64(caller.task_id.0, &mut t)),
        };
        fields[1] = Field {
            key: "proc",
            value: tairix_log::FieldValue::Str(caller.caps.proc_id().write_hex(&mut p)),
        };
        fields[2] = Field {
            key: "pproc",
            value: tairix_log::FieldValue::Str(caller.caps.parent_proc_id().write_hex(&mut pp)),
        };
        fields[3] = Field {
            key: "comm",
            value: tairix_log::FieldValue::Str(caller.caps.name()),
        };
        fields[4] = Field {
            key: "start",
            value: tairix_log::FieldValue::UnsignedInt(caller.caps.start_time()),
        };
        let n = 5 + extra.len();
        fields[5..n].copy_from_slice(extra);
        record(self.audit, event, &fields[..n]);
    }

    fn audit_unknown(&self, caller: &CallerContext<'_>, number: u16) {
        let mut n = [0u8; 12];
        // The number always fits in `u32` (it is a `u16`); `format_usize`
        // saturates above `i32::MAX` which never trips for a `u16`.
        self.audit_with_identity(
            AuditEvent::SyscallUnknown,
            caller,
            &[Field {
                key: "no",
                value: tairix_log::FieldValue::Str(tairix_util::fmt::format_usize(
                    usize::from(number),
                    &mut n,
                )),
            }],
        );
    }

    fn audit_denied(&self, caller: &CallerContext<'_>, spec: &SyscallSpec) {
        self.audit_with_identity(
            AuditEvent::SyscallPermissionDenied,
            caller,
            &[Field {
                key: "sc",
                value: tairix_log::FieldValue::Str(spec.name),
            }],
        );
    }

    fn audit_bad_args(&self, caller: &CallerContext<'_>, spec: &SyscallSpec) {
        self.audit_with_identity(
            AuditEvent::SyscallBadArguments,
            caller,
            &[Field {
                key: "sc",
                value: tairix_log::FieldValue::Str(spec.name),
            }],
        );
    }

    fn audit_invoked(&self, caller: &CallerContext<'_>, spec: &SyscallSpec) {
        self.audit_with_identity(
            AuditEvent::SyscallInvoked,
            caller,
            &[Field {
                key: "sc",
                value: tairix_log::FieldValue::Str(spec.name),
            }],
        );
    }

    fn audit_would_block(&self, caller: &CallerContext<'_>, spec: &SyscallSpec) {
        self.audit_with_identity(
            AuditEvent::SyscallHandlerWouldBlock,
            caller,
            &[Field {
                key: "sc",
                value: tairix_log::FieldValue::Str(spec.name),
            }],
        );
    }

    fn audit_not_found(&self, caller: &CallerContext<'_>, spec: &SyscallSpec) {
        self.audit_with_identity(
            AuditEvent::SyscallHandlerNotFound,
            caller,
            &[Field {
                key: "sc",
                value: tairix_log::FieldValue::Str(spec.name),
            }],
        );
    }

    fn audit_rejected(&self, caller: &CallerContext<'_>, spec: &SyscallSpec, err: Option<&Errno>) {
        let mut e = [0u8; 12];
        let err_field = match err {
            Some(e_ref) => format_i32(e_ref.as_i32(), &mut e),
            None => "?",
        };
        self.audit_with_identity(
            AuditEvent::SyscallHandlerRejected,
            caller,
            &[
                Field {
                    key: "sc",
                    value: tairix_log::FieldValue::Str(spec.name),
                },
                Field {
                    key: "err",
                    value: tairix_log::FieldValue::Str(err_field),
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

/// Decode an extended-attribute key length, bounding it to
/// `1..=FS_ATTR_KEY_MAX` before any user memory is staged.
///
/// An empty key can never satisfy the `namespace.rest` grammar and an
/// over-long one can never be stored, so both are refused at dispatch —
/// the cheap-reject shape `fs_set_mode` uses for an out-of-mask mode word.
fn decode_attr_key_len(raw: u64) -> Result<usize, Errno> {
    let len = decode_len(raw)?;
    if len == 0 || len > FS_ATTR_KEY_MAX {
        return Err(Errno::LengthOutOfRange);
    }
    Ok(len)
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
    use tairix_abi::{CapabilityId, SyscallNumber};
    use tairix_caps::CapabilitySet;
    use tairix_kernel_sec::{ProcName, ProcessId, TaskCapabilities, TaskId, UserId};
    use tairix_log::{set_max_level, Event, Level};

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
        let t = TaskCapabilities::derive(ProcessId(0xA), UserId(1000), set, set, sink);
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
        fn ipc_recv(
            &self,
            _c: &CallerContext<'_>,
            _e: u64,
            _p: u64,
            len: usize,
            _sender_out: u64,
        ) -> SyscallResult {
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
        fn boot_session_get(&self, _c: &CallerContext<'_>) -> SyscallResult {
            self.record("boot_session_get");
            Ok(tairix_abi::BootSession::Graphical.as_u64())
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
        // Mirrors the trait's register-shaped signature (see the trait's
        // justification).
        #[allow(clippy::too_many_arguments)]
        fn spawn(
            &self,
            _c: &CallerContext<'_>,
            _path: u64,
            path_len: usize,
            _attach: u64,
            _attach_len: usize,
            _strings: u64,
            _strings_len: usize,
        ) -> SyscallResult {
            self.record("spawn");
            // Echo the path length back so the reachability test can
            // assert the dispatcher decoded the `(path, path_len,
            // attach, attach_len, strings, strings_len)` arguments
            // without wiring a real spawn service here.
            Ok(path_len as u64)
        }
        fn pipe_create(&self, _c: &CallerContext<'_>, _out: u64) -> SyscallResult {
            self.record("pipe_create");
            Ok(0)
        }
        fn pty_create(
            &self,
            _c: &CallerContext<'_>,
            _out: u64,
            rows: u32,
            _cols: u32,
        ) -> SyscallResult {
            self.record("pty_create");
            // Echo the row count back so the reachability test can assert
            // the dispatcher decoded the `(out, rows, cols)` arguments
            // without wiring a real pty facility here.
            Ok(u64::from(rows))
        }
        fn pty_set_size(
            &self,
            _c: &CallerContext<'_>,
            _fd: u32,
            rows: u32,
            _cols: u32,
        ) -> SyscallResult {
            self.record("pty_set_size");
            // Echo the row count back so the reachability test can assert
            // the dispatcher decoded the `(fd, rows, cols)` arguments
            // without wiring a real pty facility here.
            Ok(u64::from(rows))
        }
        fn stream_read(
            &self,
            _c: &CallerContext<'_>,
            _fd: u32,
            _buf: u64,
            len: usize,
            _timeout_ns: u64,
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
        fn wait(
            &self,
            _c: &CallerContext<'_>,
            pid: i32,
            _status: u64,
            _flags: WaitFlags,
        ) -> SyscallResult {
            self.record("wait");
            // Echo the requested pid back as a fabricated reaped PID so the
            // reachability test can assert the dispatcher decoded the
            // `(pid, status, flags)` arguments without wiring a real wait
            // service here. The reachability test passes pid 0 (a valid I32).
            #[allow(clippy::cast_sign_loss)]
            Ok(u64::from(pid as u32))
        }
        fn signal(&self, _c: &CallerContext<'_>, pid: i32, _signal: Signal) -> SyscallResult {
            self.record("signal");
            // Echo the requested pid back so the reachability test can assert
            // the dispatcher decoded the `(pid, signal)` arguments without
            // wiring a real signal service here.
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
        fn mem_pin(&self, _c: &CallerContext<'_>) -> SyscallResult {
            self.record("mem_pin");
            Ok(0)
        }
        fn mem_unpin(&self, _c: &CallerContext<'_>) -> SyscallResult {
            self.record("mem_unpin");
            Ok(0)
        }
        fn signal_intake(&self, _c: &CallerContext<'_>, op: SignalIntakeOp) -> SyscallResult {
            self.record("signal_intake");
            // Echo the decoded op back so the reachability test can assert
            // the dispatcher validated and decoded it without wiring the
            // real intake bookkeeping here.
            Ok(u64::from(op.as_u32()))
        }
        fn sched_set_realtime(&self, _c: &CallerContext<'_>, realtime: bool) -> SyscallResult {
            self.record("sched_set_realtime");
            // Echo the decoded boolean back so the reachability test can
            // assert the dispatcher decoded it without wiring the real
            // scheduler class control here.
            Ok(u64::from(realtime))
        }
        fn sched_set_priority(
            &self,
            _c: &CallerContext<'_>,
            pid: i32,
            priority: SchedPriority,
        ) -> SyscallResult {
            self.record("sched_set_priority");
            // Echo the decoded pid and level back so the reachability test
            // can assert the dispatcher decoded `(pid, priority)` without
            // wiring the real scheduler control here.
            #[allow(clippy::cast_sign_loss)]
            Ok(u64::from(pid as u32) + u64::from(priority.as_u32()))
        }
        fn users_db_read(&self, _c: &CallerContext<'_>, _buf: u64, len: usize) -> SyscallResult {
            self.record("users_db_read");
            // Echo the capacity back so the reachability test can assert
            // the dispatcher decoded `(buf, len)` without wiring a real
            // users-database service here.
            Ok(len as u64)
        }

        fn users_admin(
            &self,
            _c: &CallerContext<'_>,
            _req: u64,
            req_len: usize,
            _out: u64,
            out_cap: usize,
        ) -> SyscallResult {
            self.record("users_admin");
            // Echo both decoded lengths back so the reachability test can
            // assert the dispatcher decoded `(req_len, out_cap)` without
            // wiring a real account-administration engine here.
            Ok((req_len + out_cap) as u64)
        }
        fn seat_switch(&self, _c: &CallerContext<'_>, seat_id: u64, console: u32) -> SyscallResult {
            self.record("seat_switch");
            // Echo both decoded arguments back so the reachability test can
            // assert the dispatcher decoded `(seat_id, console)` without
            // wiring a real seat registry here.
            Ok(seat_id + u64::from(console))
        }
        fn seat_revoke(&self, _c: &CallerContext<'_>, seat_id: u64) -> SyscallResult {
            self.record("seat_revoke");
            // Echo the seat id back so the reachability test can assert the
            // dispatcher decoded it without wiring a real seat registry here.
            Ok(seat_id)
        }
        fn console_count(&self, _c: &CallerContext<'_>) -> SyscallResult {
            self.record("console_count");
            // A fabricated single-console topology so the reachability
            // test can assert the dispatcher routed the call without
            // wiring a real console list here.
            Ok(1)
        }
        fn stream_input_mode(&self, _c: &CallerContext<'_>, _fd: u32, _mode: u32) -> SyscallResult {
            self.record("stream_input_mode");
            // Success so the reachability test can assert the dispatcher
            // decoded `(fd, mode)` without wiring a real console here.
            Ok(0)
        }
        fn console_foreground(&self, _c: &CallerContext<'_>, _fd: u32, pid: i32) -> SyscallResult {
            self.record("console_foreground");
            // Echo the decoded pid back so the decode test can assert the
            // dispatcher recovered the sign-extended `i32` without wiring a
            // real console list here.
            #[allow(clippy::cast_sign_loss)]
            Ok(u64::from(pid as u32))
        }
        fn thread_create(
            &self,
            _c: &CallerContext<'_>,
            entry: u64,
            arg: u64,
            stack_len: usize,
            tls_base: u64,
            clear_on_exit: u64,
        ) -> SyscallResult {
            self.record("thread_create");
            // Echo the decoded arguments back, folded, so the reachability
            // test can assert the dispatcher decoded every one of them
            // without wiring a real scheduler here.
            Ok(entry ^ arg ^ stack_len as u64 ^ tls_base ^ clear_on_exit)
        }
        fn thread_exit(&self, _c: &CallerContext<'_>) -> SyscallResult {
            self.record("thread_exit");
            Ok(0)
        }
        fn futex_wait(
            &self,
            _c: &CallerContext<'_>,
            uaddr: u64,
            expected: u32,
            timeout_ns: u64,
        ) -> SyscallResult {
            self.record("futex_wait");
            Ok(uaddr ^ u64::from(expected) ^ timeout_ns)
        }
        fn futex_wake(&self, _c: &CallerContext<'_>, uaddr: u64, count: u32) -> SyscallResult {
            self.record("futex_wake");
            Ok(uaddr ^ u64::from(count))
        }
        fn terminal_purge(&self, _c: &CallerContext<'_>, fd: u32) -> SyscallResult {
            self.record("terminal_purge");
            // Echo the decoded descriptor back so the reachability test can
            // assert the dispatcher decoded it without wiring a real console
            // list here.
            Ok(u64::from(fd))
        }
        fn key_inject(
            &self,
            _c: &CallerContext<'_>,
            seat: u64,
            _buf: u64,
            len: usize,
        ) -> SyscallResult {
            self.record("key_inject");
            // Echo `seat + len` back so the reachability test can assert the
            // dispatcher decoded `(seat, buf, len)` without wiring a real
            // input-focus arbiter here.
            Ok(seat + len as u64)
        }
        fn display_acquire(&self, _c: &CallerContext<'_>, seat: u64) -> SyscallResult {
            self.record("display_acquire");
            // Echo the seat id back so the decode test can assert the
            // dispatcher recovered it without wiring a real seat registry.
            Ok(seat)
        }
        fn display_release(
            &self,
            _c: &CallerContext<'_>,
            seat: u64,
            _next: ReleaseSurface,
        ) -> SyscallResult {
            self.record("display_release");
            Ok(seat)
        }
        fn keyboard_read(
            &self,
            _c: &CallerContext<'_>,
            seat: u64,
            _buf: u64,
            len: usize,
        ) -> SyscallResult {
            self.record("keyboard_read");
            // Echo `seat + len` back so the reachability test can assert the
            // dispatcher decoded `(seat, buf, len)` without wiring a real
            // keyboard channel here.
            Ok(seat + len as u64)
        }

        fn pointer_inject(
            &self,
            _c: &CallerContext<'_>,
            seat: u64,
            _buf: u64,
            len: usize,
        ) -> SyscallResult {
            self.record("pointer_inject");
            // Echo `seat + len` back so the reachability test can assert the
            // dispatcher decoded `(seat, buf, len)` without wiring a real
            // seat registry here.
            Ok(seat + len as u64)
        }

        fn pointer_read(
            &self,
            _c: &CallerContext<'_>,
            seat: u64,
            _buf: u64,
            len: usize,
        ) -> SyscallResult {
            self.record("pointer_read");
            // Echo `seat + len` back so the reachability test can assert the
            // dispatcher decoded `(seat, buf, len)` without wiring a real
            // pointer channel here.
            Ok(seat + len as u64)
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
            _flags: CallRecvFlags,
        ) -> SyscallResult {
            self.record("call_recv");
            // Echo the buffer capacity so the reachability test can assert the
            // dispatcher decoded the arguments without wiring a real
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

        fn call_post(
            &self,
            _c: &CallerContext<'_>,
            _endpoint: u64,
            _request: u64,
            request_len: usize,
            _ticket_out: u64,
            _deadline_ns: u64,
        ) -> SyscallResult {
            self.record("call_post");
            // Echo the request length so the reachability test can assert the
            // dispatcher decoded the five arguments without wiring a real
            // call-endpoint registry / scheduler here.
            Ok(request_len as u64)
        }

        fn call_reap(
            &self,
            _c: &CallerContext<'_>,
            _endpoint: u64,
            _ticket: u64,
            _reply: u64,
            reply_cap: usize,
        ) -> SyscallResult {
            self.record("call_reap");
            // Echo the reply-buffer capacity so the reachability test can
            // assert the dispatcher decoded the four arguments.
            Ok(reply_cap as u64)
        }

        fn call_cancel(
            &self,
            _c: &CallerContext<'_>,
            _endpoint: u64,
            ticket: u64,
        ) -> SyscallResult {
            self.record("call_cancel");
            // Echo the ticket so the reachability test can assert the
            // dispatcher decoded both arguments.
            Ok(ticket)
        }

        fn call_peer_seat(
            &self,
            _c: &CallerContext<'_>,
            _endpoint: u64,
            _ticket: u64,
            seat: u64,
        ) -> SyscallResult {
            self.record("call_peer_seat");
            // Echo the seat id so the reachability test can assert the
            // dispatcher decoded the three arguments without wiring a real
            // endpoint / seat registry here.
            Ok(seat)
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

        fn boot_facts_get(
            &self,
            _c: &CallerContext<'_>,
            _out: u64,
            out_cap: usize,
        ) -> SyscallResult {
            self.record("boot_facts_get");
            // Echo the capacity so the reachability test can assert the
            // dispatcher decoded both arguments without wiring real facts.
            Ok(out_cap as u64)
        }

        fn self_origin(&self, _c: &CallerContext<'_>, _out: u64, out_cap: usize) -> SyscallResult {
            self.record("self_origin");
            // Echo the capacity so the reachability test can assert the
            // dispatcher decoded both arguments without wiring a real origin.
            Ok(out_cap as u64)
        }

        fn sysinfo_introspect(
            &self,
            _c: &CallerContext<'_>,
            _domain: u32,
            _arg: u64,
            _out: u64,
            out_cap: usize,
        ) -> SyscallResult {
            self.record("sysinfo_introspect");
            // Echo the capacity so the reachability test can assert the
            // dispatcher decoded all four arguments without wiring a real
            // introspection source.
            Ok(out_cap as u64)
        }

        fn terminal_size(
            &self,
            _c: &CallerContext<'_>,
            _fd: u32,
            _out: u64,
            out_cap: usize,
        ) -> SyscallResult {
            self.record("terminal_size");
            // Echo the capacity so the reachability test can assert the
            // dispatcher decoded all three arguments without wiring a real
            // console geometry.
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

        fn hw_remove_node(
            &self,
            _c: &CallerContext<'_>,
            _node_id: u64,
            _flags: u64,
        ) -> SyscallResult {
            self.record("hw_remove_node");
            Ok(0)
        }

        fn hw_node_health(&self, _c: &CallerContext<'_>, _health: u64) -> SyscallResult {
            self.record("hw_node_health");
            Ok(0)
        }

        fn hw_self_node(&self, _c: &CallerContext<'_>) -> SyscallResult {
            self.record("hw_self_node");
            // Echo a node id so the reachability test sees a non-error result
            // without wiring a real address-space registry here.
            Ok(9)
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

        fn shm_map(&self, _c: &CallerContext<'_>, handle: u64, _len_out: u64) -> SyscallResult {
            self.record("shm_map");
            Ok(handle)
        }

        fn shm_unmap(&self, _c: &CallerContext<'_>, _base: u64, _len: usize) -> SyscallResult {
            self.record("shm_unmap");
            Ok(0)
        }

        fn call_grant(
            &self,
            _c: &CallerContext<'_>,
            endpoint: u64,
            _recipient: u64,
        ) -> SyscallResult {
            self.record("call_grant");
            // Echo the delegated endpoint so the reachability test can assert
            // the dispatcher decoded the argument without wiring a real grant
            // table or endpoint registry here.
            Ok(endpoint)
        }

        fn shm_grant(&self, _c: &CallerContext<'_>, region: u64, _endpoint: u64) -> SyscallResult {
            self.record("shm_grant");
            // Echo the region id so the reachability test can assert the
            // dispatcher decoded both arguments without wiring a real grant
            // table here.
            Ok(region)
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

        fn fs_symlink(
            &self,
            _c: &CallerContext<'_>,
            _target: u64,
            _target_len: usize,
            _link: u64,
            _link_len: usize,
        ) -> SyscallResult {
            self.record("fs_symlink");
            Ok(0)
        }

        fn fs_readlink(
            &self,
            _c: &CallerContext<'_>,
            _path: u64,
            _path_len: usize,
            _out: u64,
            _out_len: usize,
        ) -> SyscallResult {
            self.record("fs_readlink");
            Ok(0)
        }

        fn fs_link(
            &self,
            _c: &CallerContext<'_>,
            _existing: u64,
            _existing_len: usize,
            _link: u64,
            _link_len: usize,
            _flags: LinkFlags,
        ) -> SyscallResult {
            self.record("fs_link");
            Ok(0)
        }

        fn fs_realpath(
            &self,
            _c: &CallerContext<'_>,
            _path: u64,
            _path_len: usize,
            _out: u64,
            _out_len: usize,
            _mode: RealpathMode,
        ) -> SyscallResult {
            self.record("fs_realpath");
            Ok(0)
        }

        fn fs_unlink(
            &self,
            _c: &CallerContext<'_>,
            _path: u64,
            _path_len: usize,
            _flags: UnlinkFlags,
        ) -> SyscallResult {
            self.record("fs_unlink");
            Ok(0)
        }

        fn fs_set_mode(
            &self,
            _c: &CallerContext<'_>,
            _path: u64,
            _path_len: usize,
            _mode: u32,
        ) -> SyscallResult {
            self.record("fs_set_mode");
            Ok(0)
        }

        fn fs_set_owner(
            &self,
            _c: &CallerContext<'_>,
            _path: u64,
            _path_len: usize,
            _uid: u32,
            _gid: u32,
        ) -> SyscallResult {
            self.record("fs_set_owner");
            Ok(0)
        }

        fn fs_attr_get(
            &self,
            _c: &CallerContext<'_>,
            _path: u64,
            _path_len: usize,
            _key: u64,
            _key_len: usize,
            _value_out: u64,
            _value_out_len: usize,
        ) -> SyscallResult {
            self.record("fs_attr_get");
            Ok(0)
        }

        fn fs_attr_set(
            &self,
            _c: &CallerContext<'_>,
            _path: u64,
            _path_len: usize,
            _key: u64,
            _key_len: usize,
            _value: u64,
            _value_len: usize,
        ) -> SyscallResult {
            self.record("fs_attr_set");
            Ok(0)
        }

        fn fs_attr_list(
            &self,
            _c: &CallerContext<'_>,
            _path: u64,
            _path_len: usize,
            _index: u64,
            _key_out: u64,
            _key_out_len: usize,
        ) -> SyscallResult {
            self.record("fs_attr_list");
            Ok(0)
        }

        fn fs_attr_remove(
            &self,
            _c: &CallerContext<'_>,
            _path: u64,
            _path_len: usize,
            _key: u64,
            _key_len: usize,
        ) -> SyscallResult {
            self.record("fs_attr_remove");
            Ok(0)
        }

        fn port_resolve(
            &self,
            _c: &CallerContext<'_>,
            _name: u64,
            _name_len: usize,
        ) -> SyscallResult {
            self.record("port_resolve");
            Ok(0)
        }

        fn port_bind(
            &self,
            _c: &CallerContext<'_>,
            _endpoint: u64,
            _max_payload: usize,
            _capacity: usize,
        ) -> SyscallResult {
            self.record("port_bind");
            Ok(0)
        }

        fn file_map(
            &self,
            _c: &CallerContext<'_>,
            _fd: u32,
            _offset: u64,
            _len: u64,
        ) -> SyscallResult {
            self.record("file_map");
            Ok(0)
        }

        fn file_unmap(&self, _c: &CallerContext<'_>, _base: u64, _len: u64) -> SyscallResult {
            self.record("file_unmap");
            Ok(0)
        }

        fn volume_attach(
            &self,
            _c: &CallerContext<'_>,
            _request: u64,
            _request_len: usize,
        ) -> SyscallResult {
            self.record("volume_attach");
            Ok(0)
        }

        fn volume_detach(
            &self,
            _c: &CallerContext<'_>,
            _request: u64,
            _request_len: usize,
        ) -> SyscallResult {
            self.record("volume_detach");
            Ok(0)
        }

        fn system_power(&self, _c: &CallerContext<'_>, _action: PowerAction) -> SyscallResult {
            self.record("system_power");
            Ok(0)
        }

        fn fs_chdir(&self, _c: &CallerContext<'_>, _path: u64, _path_len: usize) -> SyscallResult {
            self.record("fs_chdir");
            Ok(0)
        }

        fn resource_open(
            &self,
            _c: &CallerContext<'_>,
            _reference: u64,
            _reference_len: usize,
            _flags: OpenFlags,
        ) -> SyscallResult {
            self.record("resource_open");
            Ok(5)
        }

        fn fs_getcwd(&self, _c: &CallerContext<'_>, _buf: u64, _buf_cap: usize) -> SyscallResult {
            self.record("fs_getcwd");
            Ok(0)
        }

        fn fd_grant(
            &self,
            _c: &CallerContext<'_>,
            fd: u32,
            _pid: u64,
            _write_ceiling: u64,
        ) -> SyscallResult {
            self.record("fd_grant");
            // Echo the descriptor so the reachability test can assert the
            // dispatcher decoded both arguments without a real grant table.
            Ok(u64::from(fd))
        }

        fn fd_redeem(&self, _c: &CallerContext<'_>, handle: u64) -> SyscallResult {
            self.record("fd_redeem");
            // Echo the handle so the reachability test sees a non-error
            // result.
            Ok(handle)
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
                CapabilityId::SYSINFO_INTROSPECT,
                CapabilityId::SEAT_ADMIN,
                CapabilityId::FS_MOUNT,
                CapabilityId::MEM_PIN,
                CapabilityId::SCHED_REALTIME,
                CapabilityId::SYSTEM_POWER,
                CapabilityId::IPC_ENDPOINT,
            ],
            &sink,
        );
        let ctx = CallerContext {
            task_id: TaskId(7),
            caps: &caps,
        };
        let h = MockHandlers::default();
        let d = Dispatcher::new(&h, &sink);

        for spec in tairix_abi::SYSCALLS {
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
    fn spawn_reaches_its_handler_without_a_dispatcher_capability() {
        // The graphical login screen holds only the narrow sandbox
        // authority; a blanket dispatcher gate on CAP_PROC_SPAWN used to
        // refuse its wallpaper decode before the handler could see that the
        // request was a parser sandbox. The gate now belongs to the handler,
        // which decodes the attach block and applies the precise rule, so
        // the dispatcher must let both of these through.
        for held in [
            &[CapabilityId::SANDBOX_SPAWN][..],
            // …and a caller holding neither reaches it too: the handler
            // refuses that one itself, before staging anything.
            &[][..],
        ] {
            let sink = RecordingSink::new();
            let caps = build_caps(held, &sink);
            let ctx = CallerContext {
                task_id: TaskId(7),
                caps: &caps,
            };
            let h = MockHandlers::default();
            let d = Dispatcher::new(&h, &sink);

            let spec = spec_for(SyscallNumber::SPAWN).unwrap();
            let mut args = RawArgs::ZERO;
            populate_valid_args(spec, &mut args);
            assert!(d
                .dispatch(&ctx, SyscallNumber::SPAWN.as_u16(), args)
                .is_ok());
            assert_eq!(h.last(), Some("spawn"));
            assert!(!sink
                .ids()
                .contains(&AuditEvent::SyscallPermissionDenied.id().0));
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
    fn sandbox_allow_list_is_closed_and_exact() {
        // The confinement list is a frozen security decision: widening it
        // must fail this test, so the widening is made deliberately, in
        // review, never by accident.
        let mut allowed: Vec<&str> = tairix_abi::SYSCALLS
            .iter()
            .filter(|spec| sandbox_allows(spec.number))
            .map(|spec| spec.name)
            .collect();
        allowed.sort_unstable();
        assert_eq!(
            allowed,
            [
                "exit",
                "fs_close",
                "fs_read",
                "fs_write",
                "futex_wait",
                "futex_wake",
                "mem_map",
                "mem_unmap",
                "stream_read",
                "stream_write",
                "thread_create",
                "thread_exit",
                "yield",
            ]
        );
    }

    #[test]
    fn sandboxed_caller_is_confined_to_the_allow_list() {
        // Exhaustive over the whole table: an allow-listed syscall reaches
        // its handler; every other syscall is refused before its handler
        // runs, with the denial audited. A fresh handler and sink per
        // syscall keeps the assertions independent.
        for spec in tairix_abi::SYSCALLS {
            let sink = RecordingSink::new();
            let caps = build_caps(&[], &sink).as_sandboxed();
            let ctx = CallerContext {
                task_id: TaskId(8),
                caps: &caps,
            };
            let h = MockHandlers::default();
            let d = Dispatcher::new(&h, &sink);
            let mut args = RawArgs::ZERO;
            populate_valid_args(spec, &mut args);
            let r = d.dispatch(&ctx, spec.number.as_u16(), args);
            if sandbox_allows(spec.number) {
                assert!(
                    r.is_ok(),
                    "{} should pass the sandbox gate: {r:?}",
                    spec.name
                );
                assert_eq!(h.last(), Some(spec.name));
            } else {
                assert_eq!(
                    r,
                    Err(Errno::PermissionDenied),
                    "{} must be refused for a sandboxed task",
                    spec.name
                );
                assert_eq!(h.last(), None, "{} reached its handler", spec.name);
                assert!(
                    sink.ids()
                        .contains(&AuditEvent::SyscallPermissionDenied.id().0),
                    "{} denial was not audited",
                    spec.name
                );
            }
        }
    }

    #[test]
    fn audit_records_carry_the_callers_attested_proc_id() {
        use tairix_abi::{ProcId, PROC_ID_HEX_LEN};

        /// Sink that captures the value of the `proc` field of each event.
        struct ProcFieldSink {
            seen: RefCell<Vec<alloc::string::String>>,
        }
        impl Sink for ProcFieldSink {
            fn write_event(&self, event: &Event<'_>) {
                for f in event.fields {
                    if f.key == "proc" {
                        self.seen
                            .borrow_mut()
                            .push(alloc::string::ToString::to_string(&f.value));
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
            ProcessId(7),
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
    fn audit_records_carry_the_callers_attested_parent_proc_id() {
        use tairix_abi::{ProcId, PROC_ID_HEX_LEN};

        /// Sink that captures the value of the `pproc` field of each event.
        struct PparentFieldSink {
            seen: RefCell<Vec<alloc::string::String>>,
        }
        impl Sink for PparentFieldSink {
            fn write_event(&self, event: &Event<'_>) {
                for f in event.fields {
                    if f.key == "pproc" {
                        self.seen
                            .borrow_mut()
                            .push(alloc::string::ToString::to_string(&f.value));
                    }
                }
            }
        }
        set_max_level(Level::Trace);
        let sink = PparentFieldSink {
            seen: RefCell::new(Vec::new()),
        };

        // The parentage lives on the capability record, attested kernel-side
        // from the parent's own record; it is unrelated to the numeric task
        // id and distinct from the task's own `proc_id`.
        let own = ProcId::from_raw([0xAB; 16]);
        let parent = ProcId::from_raw([0xC3; 16]);
        let caps = TaskCapabilities::derive(
            ProcessId(7),
            UserId(1000),
            CapabilitySet::empty(),
            CapabilitySet::empty(),
            &sink,
        )
        .with_proc_id(own)
        .with_parent_proc_id(parent);
        let ctx = CallerContext {
            task_id: TaskId(7),
            caps: &caps,
        };
        let h = MockHandlers::default();
        let d = Dispatcher::new(&h, &sink);

        // A capability-gated syscall the empty set cannot satisfy → denied,
        // which emits an audited record carrying the `pproc` field.
        let mut args = RawArgs::ZERO;
        args.0[0] = 1;
        args.0[1] = u64::from(CapabilityId::FS_MOUNT.as_u16());
        assert_eq!(
            d.dispatch(&ctx, SyscallNumber::CAP_REVOKE.as_u16(), args),
            Err(Errno::PermissionDenied)
        );

        let mut expected = [0u8; PROC_ID_HEX_LEN];
        let expected = parent.write_hex(&mut expected);
        let seen = sink.seen.borrow();
        assert_eq!(seen.len(), 1, "exactly the one denied record");
        assert_eq!(seen[0], expected);
        // The parentage comes from the caps record, not the task id and not
        // the task's own identity: a record whose numeric id is 7 and whose
        // own proc_id is `own` still carries the distinct parent id.
        assert_ne!(seen[0], "0000000000000007");
        let mut own_hex = [0u8; PROC_ID_HEX_LEN];
        assert_ne!(seen[0], own.write_hex(&mut own_hex));
    }

    #[test]
    fn audit_records_carry_the_callers_attested_name() {
        /// Sink that captures the value of the `comm` field of each event.
        struct CommFieldSink {
            seen: RefCell<Vec<alloc::string::String>>,
        }
        impl Sink for CommFieldSink {
            fn write_event(&self, event: &Event<'_>) {
                for f in event.fields {
                    if f.key == "comm" {
                        self.seen
                            .borrow_mut()
                            .push(alloc::string::ToString::to_string(&f.value));
                    }
                }
            }
        }
        set_max_level(Level::Trace);
        let sink = CommFieldSink {
            seen: RefCell::new(Vec::new()),
        };

        // The name lives on the capability record, attested kernel-side at
        // admission; it is not derived from the numeric task id and cannot be
        // set by the caller.
        let caps = TaskCapabilities::derive(
            ProcessId(7),
            UserId(1000),
            CapabilitySet::empty(),
            CapabilitySet::empty(),
            &sink,
        )
        .with_name(ProcName::from_bytes_truncating(b"sysinfod"));
        let ctx = CallerContext {
            task_id: TaskId(7),
            caps: &caps,
        };
        let h = MockHandlers::default();
        let d = Dispatcher::new(&h, &sink);

        // A capability-gated syscall the empty set cannot satisfy → denied,
        // which emits an audited record carrying the `comm` field.
        let mut args = RawArgs::ZERO;
        args.0[0] = 1;
        args.0[1] = u64::from(CapabilityId::FS_MOUNT.as_u16());
        assert_eq!(
            d.dispatch(&ctx, SyscallNumber::CAP_REVOKE.as_u16(), args),
            Err(Errno::PermissionDenied)
        );

        let seen = sink.seen.borrow();
        assert_eq!(seen.len(), 1, "exactly the one denied record");
        assert_eq!(seen[0], "sysinfod");
    }

    #[test]
    fn audit_records_carry_the_callers_attested_start_time() {
        /// Sink that captures the value of the `start` field of each event.
        struct StartFieldSink {
            seen: RefCell<Vec<alloc::string::String>>,
        }
        impl Sink for StartFieldSink {
            fn write_event(&self, event: &Event<'_>) {
                for f in event.fields {
                    if f.key == "start" {
                        self.seen
                            .borrow_mut()
                            .push(alloc::string::ToString::to_string(&f.value));
                    }
                }
            }
        }
        set_max_level(Level::Trace);
        let sink = StartFieldSink {
            seen: RefCell::new(Vec::new()),
        };

        // The admission timestamp lives on the capability record, attested
        // kernel-side from the monotonic clock; it is not derived from the
        // numeric task id and cannot be set by the caller.
        let start = 0x00A1_B2C3_u64;
        let caps = TaskCapabilities::derive(
            ProcessId(7),
            UserId(1000),
            CapabilitySet::empty(),
            CapabilitySet::empty(),
            &sink,
        )
        .with_start_time(start);
        let ctx = CallerContext {
            task_id: TaskId(7),
            caps: &caps,
        };
        let h = MockHandlers::default();
        let d = Dispatcher::new(&h, &sink);

        // A capability-gated syscall the empty set cannot satisfy → denied,
        // which emits an audited record carrying the `start` field.
        let mut args = RawArgs::ZERO;
        args.0[0] = 1;
        args.0[1] = u64::from(CapabilityId::FS_MOUNT.as_u16());
        assert_eq!(
            d.dispatch(&ctx, SyscallNumber::CAP_REVOKE.as_u16(), args),
            Err(Errno::PermissionDenied)
        );

        let seen = sink.seen.borrow();
        assert_eq!(seen.len(), 1, "exactly the one denied record");
        // The typed unsigned value renders as its decimal, and it is the
        // attested record value, not the numeric task id (7).
        assert_eq!(seen[0], alloc::format!("{start}"));
        assert_ne!(seen[0], "7");
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
        let unassigned = u16::try_from(tairix_abi::SYSCALLS.len()).unwrap();
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
            force_err: Some(Errno::MessageTooLarge),
            ..Default::default()
        };
        let d = Dispatcher::new(&h, &sink);
        let mut args = RawArgs::ZERO;
        args.0[0] = 1;
        args.0[1] = 0x2000;
        args.0[2] = 4;
        assert_eq!(
            d.dispatch(&ctx, SyscallNumber::IPC_SEND.as_u16(), args),
            Err(Errno::MessageTooLarge)
        );
        assert_eq!(sink.ids(), [AuditEvent::SyscallHandlerRejected.id().0]);
    }

    #[test]
    fn a_not_found_outcome_is_audited_as_absent_not_rejected() {
        // `NotFound` is the "no such object" answer, not a rejection: an
        // audited syscall returning it must emit the benign, below-error
        // `SyscallHandlerNotFound` (id 5006) rather than the ERROR-level
        // `SyscallHandlerRejected` (id 5004) a genuine refusal gets —
        // otherwise a caller that legitimately probes for an optional
        // object (e.g. `login` opening the system-configuration store and
        // the desktop bundle each round) floods the boot log with errors.
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
        assert_eq!(sink.ids(), [AuditEvent::SyscallHandlerNotFound.id().0]);
    }

    #[test]
    fn a_would_block_outcome_is_audited_as_pending_not_rejected() {
        // `WouldBlock` is the `abi-v1` "nothing yet, retry" signal, not a
        // rejection: an audited syscall returning it must emit the benign,
        // below-error `SyscallHandlerWouldBlock` (id 5005) rather than the
        // ERROR-level `SyscallHandlerRejected` (id 5004) a genuine refusal
        // gets — otherwise a caller that legitimately polls while pending
        // (e.g. `login` reading `users_db_read` while the encrypted root
        // unlocks, or a sender hitting a full IPC mailbox) floods the boot
        // log with errors.
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

        // ipc_send returning WouldBlock (mailbox full)
        assert_eq!(
            d.dispatch(&ctx, SyscallNumber::IPC_SEND.as_u16(), args),
            Err(Errno::WouldBlock)
        );
        assert_eq!(sink.ids(), [AuditEvent::SyscallHandlerWouldBlock.id().0]);
        assert!(!sink
            .ids()
            .contains(&AuditEvent::SyscallHandlerRejected.id().0));
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
    fn fs_set_mode_rejects_a_mode_above_the_permission_mask() {
        // `mode` is declared `U32`; the per-arg validator accepts the
        // 32-bit value, but a bit above `FS_MODE_MASK` (a file-type bit,
        // say) must be refused with `Errno::OutOfRange` before the handler
        // is reached — never silently masked to a mode the caller did not
        // ask for.
        let sink = RecordingSink::new();
        let caps = build_caps(&[CapabilityId::FS_ACCESS], &sink);
        let ctx = CallerContext {
            task_id: TaskId(7),
            caps: &caps,
        };
        let h = MockHandlers::default();
        let d = Dispatcher::new(&h, &sink);

        let mut args = RawArgs::ZERO;
        args.0[0] = 0x1000; // path — a non-null user pointer
        args.0[1] = 16; // path length
        args.0[2] = 0o10_0644; // a file-type bit above the mask
        assert_eq!(
            d.dispatch(&ctx, SyscallNumber::FS_SET_MODE.as_u16(), args),
            Err(Errno::OutOfRange)
        );
        assert_eq!(h.last(), None);

        // The full permission word (all twelve bits) is the inclusive
        // upper bound and reaches the handler.
        let mut args = RawArgs::ZERO;
        args.0[0] = 0x1000;
        args.0[1] = 16;
        args.0[2] = u64::from(FS_MODE_MASK);
        assert!(d
            .dispatch(&ctx, SyscallNumber::FS_SET_MODE.as_u16(), args)
            .is_ok());
        assert_eq!(h.last(), Some("fs_set_mode"));
    }

    #[test]
    fn fs_set_owner_requires_fs_access_and_reaches_the_handler() {
        let sink = RecordingSink::new();
        let caps = build_caps(&[CapabilityId::FS_ACCESS], &sink);
        let ctx = CallerContext {
            task_id: TaskId(7),
            caps: &caps,
        };
        let h = MockHandlers::default();
        let d = Dispatcher::new(&h, &sink);

        let mut args = RawArgs::ZERO;
        args.0[0] = 0x1000; // path — a non-null user pointer
        args.0[1] = 16; // path length
        args.0[2] = 1000; // new uid
        args.0[3] = u64::from(tairix_abi::FS_OWNER_UNCHANGED); // gid unchanged
        assert!(d
            .dispatch(&ctx, SyscallNumber::FS_SET_OWNER.as_u16(), args)
            .is_ok());
        assert_eq!(h.last(), Some("fs_set_owner"));

        // Without `CAP_FS_ACCESS` the dispatcher refuses before the handler
        // (the privileged `CAP_FS_CHOWN` per-inode rule is the VFS's, deeper).
        let bare = build_caps(&[], &sink);
        let ctx_bare = CallerContext {
            task_id: TaskId(7),
            caps: &bare,
        };
        let h_bare = MockHandlers::default();
        let d_bare = Dispatcher::new(&h_bare, &sink);
        assert_eq!(
            d_bare.dispatch(&ctx_bare, SyscallNumber::FS_SET_OWNER.as_u16(), args),
            Err(Errno::PermissionDenied)
        );
        assert_eq!(h_bare.last(), None);
    }

    #[test]
    fn fs_attr_calls_bound_key_and_value_lengths_at_dispatch() {
        // The key length is bounded to `1..=FS_ATTR_KEY_MAX` and the set
        // value to `FS_ATTR_VALUE_MAX` before any handler (and so any user
        // copy) is reached; in-bounds calls reach their handlers.
        let sink = RecordingSink::new();
        let caps = build_caps(&[CapabilityId::FS_ACCESS], &sink);
        let ctx = CallerContext {
            task_id: TaskId(7),
            caps: &caps,
        };
        let h = MockHandlers::default();
        let d = Dispatcher::new(&h, &sink);

        // An empty key never satisfies the `namespace.rest` grammar.
        let mut args = RawArgs::ZERO;
        args.0[0] = 0x1000; // path
        args.0[1] = 16; // path length
        args.0[2] = 0x2000; // key
        args.0[3] = 0; // key length — empty, refused
        args.0[4] = 0x3000; // value_out
        args.0[5] = 64;
        assert_eq!(
            d.dispatch(&ctx, SyscallNumber::FS_ATTR_GET.as_u16(), args),
            Err(Errno::LengthOutOfRange)
        );
        assert_eq!(h.last(), None);

        // A key above the fixed bound is refused on every attr call
        // (remove is the 4-argument shape: its trailing slots stay zero).
        let mut remove_args = RawArgs::ZERO;
        remove_args.0[0] = 0x1000;
        remove_args.0[1] = 16;
        remove_args.0[2] = 0x2000;
        remove_args.0[3] = (FS_ATTR_KEY_MAX + 1) as u64;
        assert_eq!(
            d.dispatch(&ctx, SyscallNumber::FS_ATTR_REMOVE.as_u16(), remove_args),
            Err(Errno::LengthOutOfRange)
        );
        assert_eq!(h.last(), None);

        // A value above the fixed bound is refused before any copy.
        args.0[3] = 9; // "user.demo"
        args.0[5] = (FS_ATTR_VALUE_MAX + 1) as u64;
        assert_eq!(
            d.dispatch(&ctx, SyscallNumber::FS_ATTR_SET.as_u16(), args),
            Err(Errno::LengthOutOfRange)
        );
        assert_eq!(h.last(), None);

        // In-bounds calls reach their handlers.
        args.0[5] = 64;
        assert!(d
            .dispatch(&ctx, SyscallNumber::FS_ATTR_GET.as_u16(), args)
            .is_ok());
        assert_eq!(h.last(), Some("fs_attr_get"));
        assert!(d
            .dispatch(&ctx, SyscallNumber::FS_ATTR_SET.as_u16(), args)
            .is_ok());
        assert_eq!(h.last(), Some("fs_attr_set"));
        remove_args.0[3] = 9;
        assert!(d
            .dispatch(&ctx, SyscallNumber::FS_ATTR_REMOVE.as_u16(), remove_args)
            .is_ok());
        assert_eq!(h.last(), Some("fs_attr_remove"));
        let mut args = RawArgs::ZERO;
        args.0[0] = 0x1000;
        args.0[1] = 16;
        args.0[2] = 0; // index — a plain U64, zero is valid
        args.0[3] = 0x2000; // key_out
        args.0[4] = 64;
        assert!(d
            .dispatch(&ctx, SyscallNumber::FS_ATTR_LIST.as_u16(), args)
            .is_ok());
        assert_eq!(h.last(), Some("fs_attr_list"));
    }

    #[test]
    fn fs_attr_calls_require_the_filesystem_capability() {
        // The coarse CAP_FS_ACCESS gate refuses every attr call before the
        // handler is reached, exactly as the other path-taking fs calls.
        let sink = RecordingSink::new();
        let caps = build_caps(&[], &sink);
        let ctx = CallerContext {
            task_id: TaskId(7),
            caps: &caps,
        };
        let h = MockHandlers::default();
        let d = Dispatcher::new(&h, &sink);
        let mut args = RawArgs::ZERO;
        args.0[0] = 0x1000;
        args.0[1] = 16;
        args.0[2] = 0x2000;
        args.0[3] = 9;
        args.0[4] = 0x3000;
        args.0[5] = 64;
        for n in [SyscallNumber::FS_ATTR_GET, SyscallNumber::FS_ATTR_SET] {
            assert_eq!(
                d.dispatch(&ctx, n.as_u16(), args),
                Err(Errno::PermissionDenied)
            );
        }
        // The shorter shapes, with their trailing slots zero.
        let mut remove_args = RawArgs::ZERO;
        remove_args.0[0] = 0x1000;
        remove_args.0[1] = 16;
        remove_args.0[2] = 0x2000;
        remove_args.0[3] = 9;
        assert_eq!(
            d.dispatch(&ctx, SyscallNumber::FS_ATTR_REMOVE.as_u16(), remove_args),
            Err(Errno::PermissionDenied)
        );
        let mut list_args = RawArgs::ZERO;
        list_args.0[0] = 0x1000;
        list_args.0[1] = 16;
        list_args.0[2] = 0;
        list_args.0[3] = 0x2000;
        list_args.0[4] = 64;
        assert_eq!(
            d.dispatch(&ctx, SyscallNumber::FS_ATTR_LIST.as_u16(), list_args),
            Err(Errno::PermissionDenied)
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
        let extended = i64::from(tairix_abi::WAIT_PID_ANY) as u64;
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
    fn signal_decodes_pid_and_signal_and_is_audited() {
        // `signal` is ungated (a process signals its own children, no
        // capability) but audited — delivering a signal is a
        // process-lifecycle decision. With a well-typed `(pid, signal)` tuple
        // the dispatcher recovers the `i32` pid, validates the `Signal`,
        // reaches the handler, and emits exactly one `SyscallInvoked` record
        // on success.
        let sink = RecordingSink::new();
        let caps = build_caps(&[], &sink); // no capability needed
        let ctx = CallerContext {
            task_id: TaskId(7),
            caps: &caps,
        };
        let h = MockHandlers::default();
        let d = Dispatcher::new(&h, &sink);

        let mut args = RawArgs::ZERO;
        args.0[0] = 5; // pid 5 as a sign-extended `i32`
        args.0[1] = u64::from(Signal::Terminate.as_u32());
        let r = d.dispatch(&ctx, SyscallNumber::SIGNAL.as_u16(), args);
        // The Mock echoes the decoded pid back.
        assert_eq!(r, Ok(5));
        assert_eq!(h.last(), Some("signal"));
        assert_eq!(sink.ids(), [AuditEvent::SyscallInvoked.id().0]);
    }

    #[test]
    fn signal_rejects_an_undefined_signal_before_dispatch() {
        // An out-of-range `Signal` discriminant (including the reserved 0) is
        // rejected by the dispatcher before the handler is reached, so a
        // caller cannot smuggle an unknown signal past the closed set.
        let sink = RecordingSink::new();
        let caps = build_caps(&[], &sink);
        let ctx = CallerContext {
            task_id: TaskId(7),
            caps: &caps,
        };
        let h = MockHandlers::default();
        let d = Dispatcher::new(&h, &sink);

        for bad in [0u64, 6, u64::from(u32::MAX)] {
            let mut args = RawArgs::ZERO;
            args.0[0] = 1; // pid
            args.0[1] = bad;
            assert_eq!(
                d.dispatch(&ctx, SyscallNumber::SIGNAL.as_u16(), args),
                Err(Errno::OutOfRange)
            );
        }
        assert_eq!(h.last(), None);
    }

    #[test]
    fn console_foreground_decodes_fd_and_signed_pid() {
        // The dispatcher gates on `CAP_CONSOLE_READ` (the same fd-scoped
        // terminal-control gate `stream_input_mode` carries), recovers the
        // sign-extended `i32` pid, and forwards both to the handler.
        let sink = RecordingSink::new();
        let caps = build_caps(&[CapabilityId::CONSOLE_READ], &sink);
        let ctx = CallerContext {
            task_id: TaskId(7),
            caps: &caps,
        };
        let h = MockHandlers::default();
        let d = Dispatcher::new(&h, &sink);

        let mut args = RawArgs::ZERO;
        args.0[0] = 0; // fd 0 (stdin)
        args.0[1] = 9; // the child pid to mark foreground
        assert_eq!(
            d.dispatch(&ctx, SyscallNumber::CONSOLE_FOREGROUND.as_u16(), args),
            Ok(9)
        );
        assert_eq!(h.last(), Some("console_foreground"));

        // Without the capability the dispatcher refuses before the handler.
        let no_caps = build_caps(&[], &sink);
        let no_ctx = CallerContext {
            task_id: TaskId(7),
            caps: &no_caps,
        };
        assert_eq!(
            d.dispatch(&no_ctx, SyscallNumber::CONSOLE_FOREGROUND.as_u16(), args),
            Err(Errno::PermissionDenied)
        );
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
    fn mem_pin_requires_capability_and_is_audited() {
        // Without `CAP_MEM_PIN` the dispatcher refuses before the handler
        // is reached and audits the denial; with it the call dispatches
        // and the invocation is audited (a pin is a security-relevant
        // resource decision).
        let sink = RecordingSink::new();
        let caps = build_caps(&[], &sink);
        let ctx = CallerContext {
            task_id: TaskId(7),
            caps: &caps,
        };
        let h = MockHandlers::default();
        let d = Dispatcher::new(&h, &sink);
        assert_eq!(
            d.dispatch(&ctx, SyscallNumber::MEM_PIN.as_u16(), RawArgs::ZERO),
            Err(Errno::PermissionDenied)
        );
        assert_eq!(h.last(), None, "handler must not be reached");
        assert_eq!(sink.ids(), [AuditEvent::SyscallPermissionDenied.id().0]);

        let sink2 = RecordingSink::new();
        let caps2 = build_caps(&[CapabilityId::MEM_PIN], &sink2);
        let ctx2 = CallerContext {
            task_id: TaskId(7),
            caps: &caps2,
        };
        let h2 = MockHandlers::default();
        let d2 = Dispatcher::new(&h2, &sink2);
        assert_eq!(
            d2.dispatch(&ctx2, SyscallNumber::MEM_PIN.as_u16(), RawArgs::ZERO),
            Ok(0)
        );
        assert_eq!(h2.last(), Some("mem_pin"));
        assert_eq!(sink2.ids(), [AuditEvent::SyscallInvoked.id().0]);
    }

    #[test]
    fn mem_unpin_is_ungated_and_audited() {
        // Releasing the caller's own exemption needs no capability, but
        // the unpin edge is still audited so the trail carries both edges
        // of every pin window.
        let sink = RecordingSink::new();
        let caps = build_caps(&[], &sink);
        let ctx = CallerContext {
            task_id: TaskId(7),
            caps: &caps,
        };
        let h = MockHandlers::default();
        let d = Dispatcher::new(&h, &sink);
        assert_eq!(
            d.dispatch(&ctx, SyscallNumber::MEM_UNPIN.as_u16(), RawArgs::ZERO),
            Ok(0)
        );
        assert_eq!(h.last(), Some("mem_unpin"));
        assert_eq!(sink.ids(), [AuditEvent::SyscallInvoked.id().0]);
    }

    #[test]
    fn signal_intake_is_ungated_audited_and_decodes_every_op() {
        // Own-process signal disposition needs no capability, but every
        // call is audited so the trail carries the opt-in, the opt-out,
        // and each observed delivery's drain. The dispatcher decodes each
        // closed-set op and hands it through.
        for op in [
            SignalIntakeOp::Enable,
            SignalIntakeOp::Disable,
            SignalIntakeOp::Take,
        ] {
            let sink = RecordingSink::new();
            let caps = build_caps(&[], &sink);
            let ctx = CallerContext {
                task_id: TaskId(7),
                caps: &caps,
            };
            let h = MockHandlers::default();
            let d = Dispatcher::new(&h, &sink);
            let mut args = RawArgs::ZERO;
            args.0[0] = u64::from(op.as_u32());
            // The Mock echoes the decoded op back.
            assert_eq!(
                d.dispatch(&ctx, SyscallNumber::SIGNAL_INTAKE.as_u16(), args),
                Ok(u64::from(op.as_u32()))
            );
            assert_eq!(h.last(), Some("signal_intake"));
            assert_eq!(sink.ids(), [AuditEvent::SyscallInvoked.id().0]);
        }
    }

    #[test]
    fn signal_intake_rejects_an_unknown_op_before_the_handler() {
        // An op outside the closed set fails closed in the dispatch arm;
        // the handler is never reached.
        let sink = RecordingSink::new();
        let caps = build_caps(&[], &sink);
        let ctx = CallerContext {
            task_id: TaskId(7),
            caps: &caps,
        };
        let h = MockHandlers::default();
        let d = Dispatcher::new(&h, &sink);
        let mut args = RawArgs::ZERO;
        args.0[0] = 3; // one past the closed set
        assert_eq!(
            d.dispatch(&ctx, SyscallNumber::SIGNAL_INTAKE.as_u16(), args),
            Err(Errno::OutOfRange)
        );
        assert_eq!(h.last(), None, "handler must not be reached");
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
