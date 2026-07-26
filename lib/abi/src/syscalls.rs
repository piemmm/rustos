//! Frozen `abi-v1` syscall specification table.
//!
//! This module is the **source of truth** for the user/kernel syscall
//! contract described in: every entry in [`SYSCALLS`] pins
//! one syscall's number, argument shape, return type, and the capability a
//! caller must hold to invoke it. The kernel side
//! (`kernel/syscall/src/table.rs`) is generated against this table and the
//! two are cross-checked by `cargo xtask abi-check` (final
//! paragraph). Mutating an existing entry is **not** allowed under
//! `abi-v1`; new behaviour ships in `abi-v2` (second
//! paragraph).
//!
//! # Cross-check protocol
//!
//! The byte layout encoded in [`ENCODED_TABLE`] is the canonical input to
//! the SHA-256 fingerprint embedded in every `rxe` manifest
//! ([`crate::manifest`]). The kernel re-computes that digest at boot and
//! refuses to run binaries whose embedded value disagrees. The same digest
//! is independently stored as `SYSCALL_TABLE_HASH` in
//! `kernel/syscall/src/table.rs`; the `xtask abi-check` tool fails if
//! either half is missing or if the two halves disagree on a single byte.
//!
//! # Layout of [`ENCODED_TABLE`]
//!
//! For each [`SyscallSpec`], in [`SyscallNumber`] ascending order, the
//! encoded record is a fixed-stride [`SYSCALL_ENCODED_RECORD_LEN`]-byte
//! tuple (14 fixed bytes + [`SYSCALL_NAME_MAX`] name bytes):
//!
//! | Offset | Size | Field |
//! |-------:|-----:|-------|
//! |   0    |  2   | `number` as little-endian `u16` |
//! |   2    |  1   | `arg_count` |
//! |   3    |  1   | `ret` (raw [`AbiType`] discriminant) |
//! |   4    |  6   | `args[0..6]` (raw [`AbiType`] discriminants; `Unit` for unused slots) |
//! |  10    |  1   | `required_capability.is_some()` (`0` or `1`) |
//! |  11    |  2   | `required_capability` as little-endian `u16` (`0` when absent) |
//! |  13    |  1   | `audit` (`0` or `1`) |
//! |  14    | [`SYSCALL_NAME_MAX`] | `name`, ASCII, right-padded with `0x00` |
//!
//! Names exceeding [`SYSCALL_NAME_MAX`] bytes are forbidden — the const
//! encoder produces a compile error rather than silently truncate.

use crate::{CapabilityId, SyscallNumber};

/// Maximum number of register-passed arguments per syscall.
///
/// Sized for the six argument registers every Tier-1 architecture exposes
/// on its syscall ABI (x86_64 System V: `rdi`/`rsi`/`rdx`/`r10`/`r8`/`r9`;
/// `AArch64`: `x0`..=`x5`; RISC-V: `a0`..=`a5`). Growing this is a breaking
/// ABI change and would require `abi-v2`.
pub const SYSCALL_MAX_ARGS: usize = 6;

/// Maximum length, in bytes, of the ASCII `name` of any [`SyscallSpec`].
///
/// Pinned so that [`ENCODED_TABLE`] uses a fixed stride per record and the
/// encoding is computable in a `const fn` without an allocator. Sized to fit
/// the longest `abi-v1` name (`sysinfo_introspect`, 18 bytes).
pub const SYSCALL_NAME_MAX: usize = 18;

/// Stride, in bytes, of one record inside [`ENCODED_TABLE`].
pub const SYSCALL_ENCODED_RECORD_LEN: usize = 14 + SYSCALL_NAME_MAX;

/// Stable ABI type tag carried by [`SyscallSpec`].
///
/// The discriminants are part of the `abi-v1` cross-check encoding and may
/// not be re-numbered or removed. New tags take the next free value.
#[repr(u8)]
#[derive(Copy, Clone, Debug, Eq, PartialEq, Hash)]
pub enum AbiType {
    /// Slot is unused (only valid past `arg_count` and as the `ret` of a
    /// syscall that does not return a value).
    Unit = 0,
    /// 32-bit signed integer in the low 32 bits of the register; the upper
    /// bits must equal the sign extension of the low 32 bits.
    I32 = 1,
    /// 32-bit unsigned integer in the low 32 bits; the upper bits must be
    /// zero.
    U32 = 2,
    /// Full-width 64-bit unsigned integer.
    U64 = 3,
    /// [`CapabilityId`] in the low 16 bits, upper bits zero, value within
    /// [`crate::CAPABILITY_ID_MAX`].
    Cap = 4,
    /// [`crate::Errno`] discriminant as `i32` (used as a return type).
    Errno = 5,
    /// User-space pointer. The kernel dispatcher checks non-null and the
    /// owning subsystem walks page tables.
    UserPtr = 6,
    /// Length in bytes; must fit in `usize` on the target.
    Len = 7,
    /// IPC endpoint handle (opaque `u64`).
    IpcEndpoint = 8,
    /// Generic kernel-issued handle (opaque `u64`).
    Handle = 9,
}

impl AbiType {
    /// Numeric representation carried by [`ENCODED_TABLE`].
    #[must_use]
    pub const fn as_u8(self) -> u8 {
        self as u8
    }
}

/// One row of the frozen `abi-v1` syscall table.
///
/// Fields are public and `const`-constructible so that the table can be
/// declared as a `&'static [SyscallSpec]`. Existing entries must never
/// change; see the module-level frozen-ABI note.
#[derive(Copy, Clone, Debug)]
pub struct SyscallSpec {
    /// Stable identifier.
    pub number: SyscallNumber,
    /// ASCII name. `len <= SYSCALL_NAME_MAX`.
    pub name: &'static str,
    /// Number of meaningful entries in [`Self::args`] (`<= SYSCALL_MAX_ARGS`).
    pub arg_count: u8,
    /// Argument types. Trailing unused slots must be [`AbiType::Unit`].
    pub args: [AbiType; SYSCALL_MAX_ARGS],
    /// Return type.
    pub ret: AbiType,
    /// Capability required to invoke this syscall, if any.
    ///
    /// `None` means any task may invoke the syscall (subject to its own
    /// internal checks); `Some(cap)` means the dispatcher refuses with
    /// [`crate::Errno::PermissionDenied`] if the caller's effective set
    /// does not contain `cap`.
    pub required_capability: Option<CapabilityId>,
    /// Whether the dispatcher must emit an audit record for every
    /// invocation. Security-relevant calls (`exit`, IPC, capability
    /// management) are audited; pure observers (`yield`, `cap_query`,
    /// `clock_get`) are not, to avoid drowning the audit log.
    pub audit: bool,
}

/// The frozen `abi-v1` syscall table.
///
/// Indexed by [`SyscallNumber::as_u16`]. Every entry's array index equals
/// its `number` field (verified by the in-module `table_is_dense_and_ordered`
/// unit test).
pub const SYSCALLS: &[SyscallSpec] = &[
    SyscallSpec {
        number: SyscallNumber::YIELD,
        name: "yield",
        arg_count: 0,
        args: [AbiType::Unit; SYSCALL_MAX_ARGS],
        ret: AbiType::Unit,
        required_capability: None,
        audit: false,
    },
    SyscallSpec {
        number: SyscallNumber::EXIT,
        name: "exit",
        arg_count: 1,
        args: [
            AbiType::I32,
            AbiType::Unit,
            AbiType::Unit,
            AbiType::Unit,
            AbiType::Unit,
            AbiType::Unit,
        ],
        ret: AbiType::Unit,
        required_capability: None,
        audit: true,
    },
    SyscallSpec {
        number: SyscallNumber::IPC_SEND,
        name: "ipc_send",
        arg_count: 3,
        args: [
            AbiType::IpcEndpoint,
            AbiType::UserPtr,
            AbiType::Len,
            AbiType::Unit,
            AbiType::Unit,
            AbiType::Unit,
        ],
        ret: AbiType::Errno,
        required_capability: None,
        audit: true,
    },
    SyscallSpec {
        number: SyscallNumber::IPC_RECV,
        name: "ipc_recv",
        arg_count: 4,
        args: [
            // Port id, payload buffer ptr, payload buffer cap, and the
            // sender-origin out pointer (exactly `ORIGIN_WIRE_LEN` bytes):
            // the kernel-attested identity snapshotted at send time, so
            // the owner authenticates each message's sender fail-closed.
            AbiType::IpcEndpoint,
            AbiType::UserPtr,
            AbiType::Len,
            AbiType::UserPtr,
            AbiType::Unit,
            AbiType::Unit,
        ],
        // `U64` carries the payload-bytes-written-or-`-errno` register
        // convention `call_recv` / `ipc_call` use.
        ret: AbiType::U64,
        required_capability: None,
        audit: false,
    },
    SyscallSpec {
        number: SyscallNumber::CAP_QUERY,
        name: "cap_query",
        arg_count: 1,
        args: [
            AbiType::Cap,
            AbiType::Unit,
            AbiType::Unit,
            AbiType::Unit,
            AbiType::Unit,
            AbiType::Unit,
        ],
        ret: AbiType::U32,
        required_capability: None,
        audit: false,
    },
    SyscallSpec {
        number: SyscallNumber::CAP_DELEGATE,
        name: "cap_delegate",
        arg_count: 2,
        args: [
            AbiType::Handle,
            AbiType::UserPtr,
            AbiType::Unit,
            AbiType::Unit,
            AbiType::Unit,
            AbiType::Unit,
        ],
        ret: AbiType::Errno,
        required_capability: None,
        audit: true,
    },
    SyscallSpec {
        number: SyscallNumber::CAP_REVOKE,
        name: "cap_revoke",
        arg_count: 2,
        args: [
            AbiType::Handle,
            AbiType::Cap,
            AbiType::Unit,
            AbiType::Unit,
            AbiType::Unit,
            AbiType::Unit,
        ],
        ret: AbiType::Errno,
        required_capability: Some(CapabilityId::USER_ADMIN),
        audit: true,
    },
    SyscallSpec {
        number: SyscallNumber::CLOCK_GET,
        name: "clock_get",
        arg_count: 0,
        args: [AbiType::Unit; SYSCALL_MAX_ARGS],
        ret: AbiType::U64,
        required_capability: None,
        audit: false,
    },
    SyscallSpec {
        number: SyscallNumber::IRQ_BIND,
        name: "irq_bind",
        arg_count: 1,
        args: [
            AbiType::U32,
            AbiType::Unit,
            AbiType::Unit,
            AbiType::Unit,
            AbiType::Unit,
            AbiType::Unit,
        ],
        ret: AbiType::Handle,
        required_capability: Some(CapabilityId::IRQ_BIND),
        audit: true,
    },
    SyscallSpec {
        number: SyscallNumber::IRQ_WAIT,
        name: "irq_wait",
        arg_count: 2,
        args: [
            AbiType::Handle,
            AbiType::U64,
            AbiType::Unit,
            AbiType::Unit,
            AbiType::Unit,
            AbiType::Unit,
        ],
        ret: AbiType::Errno,
        required_capability: Some(CapabilityId::IRQ_BIND),
        audit: false,
    },
    SyscallSpec {
        number: SyscallNumber::RANDOM_GET,
        name: "random_get",
        arg_count: 3,
        args: [
            AbiType::UserPtr,
            AbiType::Len,
            AbiType::U32,
            AbiType::Unit,
            AbiType::Unit,
            AbiType::Unit,
        ],
        ret: AbiType::U64,
        // Drawing randomness needs no capability (: a
        // normal request must not block and is available to every
        // task); it is a pure observer, so — like `clock_get` — it is
        // not audited, to avoid drowning the audit log.
        required_capability: None,
        audit: false,
    },
    SyscallSpec {
        number: SyscallNumber::STREAM_WRITE,
        name: "stream_write",
        arg_count: 3,
        args: [
            AbiType::U32,
            AbiType::UserPtr,
            AbiType::Len,
            AbiType::Unit,
            AbiType::Unit,
            AbiType::Unit,
        ],
        ret: AbiType::U64,
        // Writing one of the calling process's inherited standard
        // streams routes to that descriptor's kernel
        // stream backing. Authority is the per-process descriptor table
        // the spawner established — never an ambient device — so the
        // dispatcher applies no blanket capability: a stream may be
        // backed by a pipe or a wired file (`plans/SPAWN.md` SP10),
        // which needs no console authority. The handler checks
        // `CAP_CONSOLE_WRITE` exactly when the descriptor resolves to a
        // console backing (the `fs_read`/`CAP_FS_ACCESS` precedent).
        // Like the other high-volume data movers (`ipc_recv`,
        // `random_get`) it is not audited per call, to avoid drowning the
        // audit log.
        required_capability: None,
        audit: false,
    },
    SyscallSpec {
        number: SyscallNumber::SPAWN,
        name: "spawn",
        arg_count: 6,
        args: [
            AbiType::UserPtr,
            AbiType::Len,
            // The attach block: the address of an encoded
            // `tairix_abi::SpawnAttach` block selecting the child's
            // target user (`SPAWN_UID_INHERIT` or a concrete uid,
            // kernel-gated on `CAP_SPAWN_AS_USER`), its base console
            // (`CONSOLE_INHERIT` or an installed index), and one `FdWire`
            // per standard descriptor (`plans/SPAWN.md` SP10). Zero means
            // "no block": full inherit, the pre-SP10 semantics. `U64`
            // rather than `UserPtr` so the absent case is representable;
            // the handler stages and parses a present block fail-closed
            // and owner-checks every named handle before any state is
            // touched.
            AbiType::U64,
            // Exact byte length of the attach block
            // (`SPAWN_ATTACH_LEN`), zero when absent; any other value
            // fails closed before staging.
            AbiType::Len,
            // The child's startup strings: the address of an encoded
            // `tairix_abi::process` startup-vector block (the same `PSV1`
            // format the kernel writes into a child's image) carrying the
            // argument vector and environment the caller chose. Zero means
            // "no block": the child receives the program's registered
            // default arguments and an empty environment. `U64` rather than
            // `UserPtr` so the absent case is representable; the handler
            // stages and parses a present block fail-closed (the strings
            // are data — they carry no authority and the kernel mints the
            // child's canary itself, ignoring the block's).
            AbiType::U64,
            // Byte length of the startup-strings block (zero when absent);
            // bounded by the handler against
            // `PROCESS_START_MAX_TOTAL_LEN` before staging.
            AbiType::Len,
        ],
        ret: AbiType::U64,
        // Spawning a process materialises a new principal and hands it
        // the CPU, so it is privileged rather than ambient (no ambient authority). It is a security-relevant state
        // change — a new process appears — so unlike the high-volume
        // data movers it IS audited per call; the
        // `ProcessSpawn*` events the spawn caller already emits cover the
        // decision, and the dispatcher's per-call record attributes the
        // request to the caller.
        required_capability: Some(CapabilityId::PROC_SPAWN),
        audit: true,
    },
    SyscallSpec {
        number: SyscallNumber::STREAM_READ,
        name: "stream_read",
        arg_count: 4,
        args: [
            AbiType::U32,
            AbiType::UserPtr,
            AbiType::Len,
            // `timeout_ns`: how long a read with no pending input may park,
            // in nanoseconds. `0` waits indefinitely (the interactive
            // default); a non-zero bound returns `-TimedOut` when it
            // elapses with no input, so a full-screen program can refresh
            // a clock or status figure without a busy poll.
            AbiType::U64,
            AbiType::Unit,
            AbiType::Unit,
        ],
        ret: AbiType::U64,
        // Reading one of the calling process's inherited standard streams routes to that descriptor's kernel stream
        // backing. Authority is the per-process descriptor table the
        // spawner established — never an ambient device — so the
        // dispatcher applies no blanket capability: a stream may be
        // backed by a pipe or a wired file (`plans/SPAWN.md` SP10),
        // which needs no console authority. The handler checks
        // `CAP_CONSOLE_READ` exactly when the descriptor resolves to a
        // console backing (the `fs_read`/`CAP_FS_ACCESS` precedent).
        // Like the other high-volume data movers (`stream_write`,
        // `ipc_recv`, `random_get`) it is not audited per call, to avoid
        // drowning the audit log.
        required_capability: None,
        audit: false,
    },
    SyscallSpec {
        number: SyscallNumber::MEM_MAP,
        name: "mem_map",
        arg_count: 3,
        args: [
            AbiType::Len,
            AbiType::U32,
            AbiType::U64,
            AbiType::Unit,
            AbiType::Unit,
            AbiType::Unit,
        ],
        ret: AbiType::U64,
        // Growing one's *own* hardware-isolated address space with
        // anonymous RW memory is the unprivileged baseline (precedent — "list my own processes" needs no capability):
        // a region is mapped only into the caller's own space, so it
        // grants no authority over anything else (no
        // global user heap, no cross-process mapping). Like the other
        // high-volume own-process operations it is not audited per call,
        // to avoid drowning the audit log.
        required_capability: None,
        audit: false,
    },
    SyscallSpec {
        number: SyscallNumber::MEM_UNMAP,
        name: "mem_unmap",
        arg_count: 2,
        args: [
            AbiType::U64,
            AbiType::Len,
            AbiType::Unit,
            AbiType::Unit,
            AbiType::Unit,
            AbiType::Unit,
        ],
        ret: AbiType::Errno,
        // The release half of `mem_map`; same unprivileged, unaudited
        // posture — it only releases the caller's own anonymous memory.
        required_capability: None,
        audit: false,
    },
    SyscallSpec {
        number: SyscallNumber::WAIT,
        name: "wait",
        arg_count: 3,
        args: [
            AbiType::I32,
            AbiType::UserPtr,
            AbiType::U32,
            AbiType::Unit,
            AbiType::Unit,
            AbiType::Unit,
        ],
        ret: AbiType::U64,
        // Reaping one's *own* child is the unprivileged baseline
        // (precedent — observing/managing one's own
        // processes needs no capability): a process may only wait on
        // children it spawned, so waiting grants no authority over any
        // other principal (no ambient authority). Unlike
        // the high-volume own-process data movers it IS audited per call:
        // reaping a child is a security-relevant process-lifecycle state
        // change — a principal disappears — exactly as `spawn`/`exit` are
        // audited. The `flags` argument selects blocking (the default) or a
        // non-blocking poll (`WaitFlags::NONBLOCK`); a poll that finds no
        // reapable child returns `Errno::WouldBlock`, which the dispatcher
        // records below the error level, so neither the blocking wait nor a
        // polling job-control loop drowns the log.
        required_capability: None,
        audit: true,
    },
    SyscallSpec {
        number: SyscallNumber::RLIMIT_GET,
        name: "rlimit_get",
        arg_count: 2,
        args: [
            AbiType::U32,
            AbiType::UserPtr,
            AbiType::Unit,
            AbiType::Unit,
            AbiType::Unit,
            AbiType::Unit,
        ],
        ret: AbiType::Errno,
        // Reading one's *own* effective resource limit grants no authority
        // over anything else, so — like the other own-process observers
        // (`mem_map`, `wait`'s self-scoping) — it is the unprivileged
        // baseline and is not audited per call.
        required_capability: None,
        audit: false,
    },
    SyscallSpec {
        number: SyscallNumber::RLIMIT_SET,
        name: "rlimit_set",
        arg_count: 2,
        args: [
            AbiType::U32,
            AbiType::UserPtr,
            AbiType::Unit,
            AbiType::Unit,
            AbiType::Unit,
            AbiType::Unit,
        ],
        ret: AbiType::Errno,
        // Lowering one's own bound needs no capability; the dispatcher
        // therefore leaves the syscall ungated and the handler performs the
        // finer `CAP_RLIMIT_RAISE` check only when a request would *raise* a
        // hard bound — the same pattern `stream_*` uses
        // (coarse syscall gate, fine handler-side check). It changes a
        // task's enforced limits, a security-relevant policy change, so it
        // IS audited per call.
        required_capability: None,
        audit: true,
    },
    SyscallSpec {
        number: SyscallNumber::USERS_DB_READ,
        name: "users_db_read",
        arg_count: 2,
        args: [
            AbiType::UserPtr,
            AbiType::Len,
            AbiType::Unit,
            AbiType::Unit,
            AbiType::Unit,
            AbiType::Unit,
        ],
        ret: AbiType::U64,
        // The user database carries every account's identity and salted
        // password record, so reading it is privileged rather than ambient: only the authentication principal (login) is
        // granted `CAP_USERS_READ`. It IS audited per call — credential-database access is a security-relevant
        // decision and is low-volume (once per login process), so the
        // record cannot drown the log.
        required_capability: Some(CapabilityId::USERS_READ),
        audit: true,
    },
    SyscallSpec {
        number: SyscallNumber::CONSOLE_COUNT,
        name: "console_count",
        arg_count: 0,
        args: [AbiType::Unit; SYSCALL_MAX_ARGS],
        // `U64` so the C view carries the count-or-`-errno` register
        // convention `spawn` / `users_db_read` use (the stub returns the
        // raw register).
        ret: AbiType::U64,
        // Console topology belongs to the principals that drive
        // consoles (PID 1 `init`, login) rather than to every task; the count itself is low-sensitivity
        // metadata, so like `cap_query` it is a pure observer and is
        // NOT audited (avoid drowning the log).
        required_capability: Some(CapabilityId::CONSOLE_WRITE),
        audit: false,
    },
    SyscallSpec {
        number: SyscallNumber::STREAM_INPUT_MODE,
        name: "stream_input_mode",
        arg_count: 2,
        args: [
            AbiType::U32,
            AbiType::U32,
            AbiType::Unit,
            AbiType::Unit,
            AbiType::Unit,
            AbiType::Unit,
        ],
        ret: AbiType::Errno,
        // The input discipline is a property of the console the reader
        // holds, so the control shares `stream_read`'s `CAP_CONSOLE_READ`
        // gate — never ambient. The kernel performs the echo/indicator
        // itself as part of the read line discipline, so setting the mode
        // needs no separate `CAP_CONSOLE_WRITE`. Like the other terminal
        // operations it is low-volume configuration, not a
        // security-relevant state change, so — like `console_count` — it
        // is NOT audited per call.
        required_capability: Some(CapabilityId::CONSOLE_READ),
        audit: false,
    },
    SyscallSpec {
        number: SyscallNumber::KEY_INJECT,
        name: "key_inject",
        arg_count: 3,
        args: [
            // The seat the decoded key edge belongs to (the seat whose
            // keyboard produced it), then the encoded record. An unknown
            // seat id fails closed with `NotFound`.
            AbiType::U64,
            AbiType::UserPtr,
            AbiType::Len,
            AbiType::Unit,
            AbiType::Unit,
            AbiType::Unit,
        ],
        // `U64` so the C view carries the bytes-consumed-or-`-errno`
        // register convention `stream_write` / `console_count` use.
        ret: AbiType::U64,
        // Feeding the system keyboard stream is privileged, never ambient: only the keyboard-input driver that decoded a
        // discovered keyboard holds `CAP_INPUT_INJECT`. Like the other
        // per-event stream operations (`stream_write` / `stream_read`) it
        // fires once per key edge, so auditing every call would drown the
        // log — it is NOT audited; the device
        // manager's one-time driver load IS the audited security decision.
        required_capability: Some(CapabilityId::INPUT_INJECT),
        audit: false,
    },
    SyscallSpec {
        number: SyscallNumber::DISPLAY_ACQUIRE,
        name: "display_acquire",
        arg_count: 1,
        args: [
            // The seat to acquire. Seat 0 is the boot seat; each further
            // discovered display node mints its own seat (`SEAT_LIST`
            // enumerates them). An unknown seat id fails closed with
            // `NotFound`.
            AbiType::U64,
            AbiType::Unit,
            AbiType::Unit,
            AbiType::Unit,
            AbiType::Unit,
            AbiType::Unit,
        ],
        // `U64` for the value-or-`-errno` register convention: a
        // successful acquire returns the minted lease's generation
        // (>= 1), the handle the present right is later derived from
        // (`plans/DISPLAY.md` D4).
        ret: AbiType::U64,
        // Owning the seat (the display and, with it, the keyboard) is
        // privileged, never ambient: only a session's
        // window manager holds `CAP_DISPLAY`, and the kernel additionally
        // records and checks the owning task, so a held seat is never
        // displaced (`plans/DISPLAY.md`). Taking the screen and
        // re-routing the system keyboard stream is a security-relevant
        // ownership change — the analogue of a foreground-tty switch — so
        // unlike the high-volume stream operations it IS audited per call; it is low-volume (once per session
        // hand-over), so the record cannot drown the log.
        required_capability: Some(CapabilityId::DISPLAY),
        audit: true,
    },
    SyscallSpec {
        number: SyscallNumber::DISPLAY_RELEASE,
        name: "display_release",
        arg_count: 1,
        args: [
            // The seat to release; only its recorded owner may. An unknown
            // seat id fails closed with `NotFound`.
            AbiType::U64,
            AbiType::Unit,
            AbiType::Unit,
            AbiType::Unit,
            AbiType::Unit,
            AbiType::Unit,
        ],
        ret: AbiType::Errno,
        // The release half of `display_acquire`; same `CAP_DISPLAY` gate
        // (plus the kernel-side owner check — only the recorded owner may
        // release) and same audited posture — returning input to the text
        // console is the matching security-relevant ownership change.
        required_capability: Some(CapabilityId::DISPLAY),
        audit: true,
    },
    SyscallSpec {
        number: SyscallNumber::KEYBOARD_READ,
        name: "keyboard_read",
        arg_count: 3,
        args: [
            // The seat whose desktop keyboard channel is drained (only its
            // owner may), then the record buffer. An unknown seat id fails
            // closed with `NotFound`.
            AbiType::U64,
            AbiType::UserPtr,
            AbiType::Len,
            AbiType::Unit,
            AbiType::Unit,
            AbiType::Unit,
        ],
        // `U64` so the C view carries the bytes-read-or-`-errno` register
        // convention `stream_read` uses.
        ret: AbiType::U64,
        // Reading the keyboard channel is privileged, never ambient: the
        // capability is `CAP_INPUT_READ`, and the drain is additionally
        // owner-gated kernel-side against the seat's live lease, so the
        // keyboard stream is delivered only to the task that owns the
        // surface (`plans/DISPLAY.md`). Like the other
        // high-volume stream readers (`stream_read`) it fires once per key
        // edge, so it is NOT audited.
        required_capability: Some(CapabilityId::INPUT_READ),
        audit: false,
    },
    SyscallSpec {
        number: SyscallNumber::MMIO_MAP,
        name: "mmio_map",
        arg_count: 3,
        args: [
            AbiType::Handle,
            AbiType::Len,
            AbiType::Len,
            AbiType::Unit,
            AbiType::Unit,
            AbiType::Unit,
        ],
        // `U64` carries the mapped base virtual address (or, by the
        // shared register convention, a negated errno) back to the
        // driver, exactly like `mem_map`.
        ret: AbiType::U64,
        // Mapping a device's register block is privileged, never ambient: only a driver granted the matched node's MMIO
        // resource holds `CAP_MMIO_MAP`, and the kernel additionally maps
        // only the `[offset, offset + len)` sub-region — bounded inside the
        // unforgeable grant handle the driver owns — so a driver granted a
        // large outbound bus aperture maps just the one BAR it enumerated,
        // never the whole window. It IS audited per call —
        // handing a principal direct access to hardware registers is a
        // security-relevant grant and is low-volume (once per window at
        // driver init), so the record cannot drown the log.
        required_capability: Some(CapabilityId::MMIO_MAP),
        audit: true,
    },
    SyscallSpec {
        number: SyscallNumber::DMA_ALLOC,
        name: "dma_alloc",
        arg_count: 3,
        args: [
            AbiType::Handle,
            AbiType::Len,
            AbiType::UserPtr,
            AbiType::Unit,
            AbiType::Unit,
            AbiType::Unit,
        ],
        // `U64` carries the mapped base virtual address (or, by the shared
        // register convention, a negated errno) back to the driver; the
        // device-visible address is written to the `device_out` user
        // pointer, exactly as `wait` writes the reaped status.
        ret: AbiType::U64,
        // Carving a driver a DMA-coherent buffer the hardware reads/writes
        // is privileged, never ambient: only a driver
        // granted the matched node's DMA constraint holds `CAP_MEM_DMA`, and
        // the kernel bounds the carve by that unforgeable grant. It
        // IS audited per call — handing a principal a
        // region the hardware can touch is a security-relevant grant and is
        // low-volume (once per buffer at driver init), so the record cannot
        // drown the log.
        required_capability: Some(CapabilityId::MEM_DMA),
        audit: true,
    },
    SyscallSpec {
        number: SyscallNumber::RESOURCE_GRANTS,
        name: "resource_grants",
        arg_count: 2,
        args: [
            AbiType::UserPtr,
            AbiType::Len,
            AbiType::Unit,
            AbiType::Unit,
            AbiType::Unit,
            AbiType::Unit,
        ],
        // `U64` carries the bytes-written-or-`-errno` register convention
        // `users_db_read` / `keyboard_read` use.
        ret: AbiType::U64,
        // Reading the calling task's *own* minted device-resource grants
        // confers no authority over anything else (the handles are useless
        // without the `CAP_MMIO_MAP` / `CAP_MEM_DMA` the driver also holds,
        // and the kernel re-checks ownership when they are presented), so —
        // like the other own-process observers (`mem_map`, `rlimit_get`) — it
        // is the unprivileged baseline and is not
        // audited per call: the device manager's one-time driver load IS the
        // audited security decision.
        required_capability: None,
        audit: false,
    },
    SyscallSpec {
        number: SyscallNumber::HW_TREE_READ,
        name: "hw_tree_read",
        arg_count: 2,
        args: [
            AbiType::UserPtr,
            AbiType::Len,
            AbiType::Unit,
            AbiType::Unit,
            AbiType::Unit,
            AbiType::Unit,
        ],
        // `U64` carries the bytes-written-or-`-errno` register convention
        // `resource_grants` / `users_db_read` use.
        ret: AbiType::U64,
        // The discovered hardware inventory is a privileged *global* view,
        // not a calling-task observation: it reveals every device on the
        // machine, so it is gated by `CAP_SYSINFO_HW` exactly like the
        // System Information API's hardware query, never the unprivileged own-process baseline. Not audited
        // per call: the device manager re-reads the tree on every change
        // (it is the high-volume reactive consumer), and the audited
        // security decision is the subsequent driver load,
        // not the observation; the capability *denial* is audited by the
        // dispatcher regardless.
        required_capability: Some(CapabilityId::SYSINFO_HW),
        audit: false,
    },
    SyscallSpec {
        number: SyscallNumber::HW_TREE_WAIT,
        name: "hw_tree_wait",
        arg_count: 2,
        args: [
            // `last_generation` then `timeout_ns`.
            AbiType::U64,
            AbiType::U64,
            AbiType::Unit,
            AbiType::Unit,
            AbiType::Unit,
            AbiType::Unit,
        ],
        // `Errno` register convention `irq_wait` uses: `Ok(0)` on a change,
        // `-TimedOut` on deadline.
        ret: AbiType::Errno,
        // Same privilege as reading the tree — waiting for it to change is
        // the reactive half of the same global observation. Not audited per call: it is a high-volume blocking wait,
        // and a refused capability is audited by the dispatcher regardless.
        required_capability: Some(CapabilityId::SYSINFO_HW),
        audit: false,
    },
    SyscallSpec {
        number: SyscallNumber::IPC_CALL,
        name: "ipc_call",
        arg_count: 5,
        args: [
            // endpoint, request ptr, request len, reply ptr, reply cap.
            AbiType::IpcEndpoint,
            AbiType::UserPtr,
            AbiType::Len,
            AbiType::UserPtr,
            AbiType::Len,
            AbiType::Unit,
        ],
        // `U64` carries the reply-bytes-written-or-`-errno` register
        // convention `hw_tree_read` / `users_db_read` use.
        ret: AbiType::U64,
        // The endpoint enforces its own required send capability against the
        // caller before posting, exactly like `ipc_send`
        // over a port, so the dispatcher gate is `None`. Audited per call:
        // a synchronous system-service call is a security-relevant IPC, like
        // `ipc_send`; the driver-store consumer is
        // low-volume (a boot/hotplug match pass), so the record cannot drown
        // the log.
        required_capability: None,
        audit: true,
    },
    SyscallSpec {
        number: SyscallNumber::CALL_CREATE,
        name: "call_create",
        arg_count: 6,
        args: [
            // endpoint id, send-caps ptr, recv-caps ptr, max_request,
            // max_reply, capacity.
            AbiType::IpcEndpoint,
            AbiType::UserPtr,
            AbiType::UserPtr,
            AbiType::Len,
            AbiType::Len,
            AbiType::Len,
        ],
        // `Errno` register convention: `Ok(0)` on a bind, else `-errno`.
        ret: AbiType::Errno,
        // No flat dispatcher gate: binding a *restricted-sender* endpoint
        // requires `CAP_IPC_BIND_PRIVILEGED`, but an unrestricted (open)
        // endpoint needs none, so the gate is conditional and enforced
        // inside the handler/`CallEndpoint::create`,
        // exactly as port binding gates conditionally. Audited: binding a
        // service endpoint is a security-relevant, low-volume event.
        required_capability: None,
        audit: true,
    },
    SyscallSpec {
        number: SyscallNumber::CALL_RECV,
        name: "call_recv",
        arg_count: 5,
        args: [
            // endpoint id, request buffer ptr, request buffer cap,
            // ticket-out ptr, `CallRecvFlags` bits (`NON_BLOCKING` makes
            // an empty queue return `-WouldBlock` instead of parking —
            // the wait-set event-loop mode; reserved bits fail closed).
            AbiType::IpcEndpoint,
            AbiType::UserPtr,
            AbiType::Len,
            AbiType::UserPtr,
            AbiType::U32,
            AbiType::Unit,
        ],
        // `U64` carries the request-bytes-written-or-`-errno` register
        // convention `ipc_recv` / `ipc_call` use.
        ret: AbiType::U64,
        // Gated by the endpoint's required *receive* capability against the
        // caller (enforced in the handler), not a flat
        // dispatcher gate. Not audited per call: a server's receive loop is
        // high-volume, and a refused capability is audited by the dispatcher
        // regardless (mirrors `ipc_recv`).
        required_capability: None,
        audit: false,
    },
    SyscallSpec {
        number: SyscallNumber::CALL_REPLY,
        name: "call_reply",
        arg_count: 4,
        args: [
            // endpoint id, ticket, reply ptr, reply len.
            AbiType::IpcEndpoint,
            AbiType::Handle,
            AbiType::UserPtr,
            AbiType::Len,
            AbiType::Unit,
            AbiType::Unit,
        ],
        // `Errno` register convention: `Ok(0)` on a reply, else `-errno`.
        ret: AbiType::Errno,
        // Gated like `call_recv` by the endpoint's required receive
        // capability (the same task that receives answers). Not audited per
        // call for the same high-volume reason.
        required_capability: None,
        audit: false,
    },
    SyscallSpec {
        number: SyscallNumber::USERS_DB_WAIT,
        name: "users_db_wait",
        arg_count: 1,
        args: [
            // `timeout_ns` (`u64::MAX` for an unbounded wait).
            AbiType::U64,
            AbiType::Unit,
            AbiType::Unit,
            AbiType::Unit,
            AbiType::Unit,
            AbiType::Unit,
        ],
        // `Errno` register convention `hw_tree_wait` uses: `Ok(0)` once the
        // database is no longer pending, `-TimedOut` on deadline.
        ret: AbiType::Errno,
        // Same privilege as reading the database — waiting for it to become
        // available is the reactive half of the same access. NOT audited per call: it is a blocking wait, not a state
        // change, and a refused capability is audited by the dispatcher
        // regardless (the same pattern `hw_tree_wait` uses). Auditing the
        // wait per call is what flooded the boot log when `login` polled
        // `users_db_read` instead.
        required_capability: Some(CapabilityId::USERS_READ),
        audit: false,
    },
    SyscallSpec {
        number: SyscallNumber::LOG_EMIT,
        name: "log_emit",
        arg_count: 2,
        args: [
            // The encoded `LogRecordRef` wire image pointer, then its length.
            AbiType::UserPtr,
            AbiType::Len,
            AbiType::Unit,
            AbiType::Unit,
            AbiType::Unit,
            AbiType::Unit,
        ],
        // `Errno` register convention: `Ok(0)` once the record is accepted,
        // else `-errno` for a malformed record.
        ret: AbiType::Errno,
        // Emitting a diagnostic record to the system console log is a
        // privileged grant (`CAP_LOG_EMIT`), held only by trusted system
        // services so an ordinary app cannot scribble on the captured serial
        // line. NOT audited per call: this is
        // the diagnostic log, not the hash-chained security audit log, and a
        // service emits records at volume — auditing each one would drown the
        // audit log; a refused capability is
        // audited by the dispatcher regardless.
        required_capability: Some(CapabilityId::LOG_EMIT),
        audit: false,
    },
    SyscallSpec {
        number: SyscallNumber::HW_EMIT_NODE,
        name: "hw_emit_node",
        arg_count: 2,
        args: [
            // The encoded `HwNode` wire image pointer, then its length.
            AbiType::UserPtr,
            AbiType::Len,
            AbiType::Unit,
            AbiType::Unit,
            AbiType::Unit,
            AbiType::Unit,
        ],
        // `Errno` register convention: `Ok(0)` once the node is published,
        // else `-errno` for a malformed node, an unknown parent, or a
        // resource outside the caller's grants.
        ret: AbiType::Errno,
        // Publishing a discovered child into the global hardware tree is a
        // privileged grant (`CAP_HW_EMIT`), held only by an autoloaded
        // user-space bus driver. It IS audited per
        // call: admitting a node that drives the device
        // manager to autoload a further driver — and that carries
        // device-resource grants — is a security-relevant event, and it is
        // low-volume (once per enumerated device), so the record cannot
        // drown the log.
        required_capability: Some(CapabilityId::HW_EMIT),
        audit: true,
    },
    SyscallSpec {
        number: SyscallNumber::HW_REMOVE_NODE,
        name: "hw_remove_node",
        arg_count: 1,
        args: [
            // The `HwNode::id` of the node to remove.
            AbiType::U64,
            AbiType::Unit,
            AbiType::Unit,
            AbiType::Unit,
            AbiType::Unit,
            AbiType::Unit,
        ],
        // `Errno` register convention: `Ok(0)` once the node (and its
        // subtree) is removed, else `-errno` for an unknown id or a node the
        // caller does not own.
        ret: AbiType::Errno,
        // Removing a discovered child from the global hardware tree is the
        // exact mirror of publishing it: the same privileged grant
        // (`CAP_HW_EMIT`), held only by an autoloaded user-space bus driver
        // reporting a device it owns has gone. It IS
        // audited per call: retiring a node drives the
        // device manager to unload the driver bound to it, a security-relevant
        // event, and it is low-volume (once per hot-removed device), so the
        // record cannot drown the log — symmetric with `hw_emit_node`.
        required_capability: Some(CapabilityId::HW_EMIT),
        audit: true,
    },
    SyscallSpec {
        number: SyscallNumber::MSI_ALLOC,
        name: "msi_alloc",
        arg_count: 2,
        args: [
            // The out buffer the encoded `MsiAllocation` is written into,
            // then its capacity.
            AbiType::UserPtr,
            AbiType::Len,
            AbiType::Unit,
            AbiType::Unit,
            AbiType::Unit,
            AbiType::Unit,
        ],
        // `U64` carries the bytes-written-or-`-errno` register convention
        // `resource_grants` / `dma_alloc` use.
        ret: AbiType::U64,
        // Allocating an MSI vector is gated on `CAP_IRQ_BIND` — the same
        // privilege the driver needs to `irq_bind` the line it returns — and
        // is never ambient: the kernel mints a vector, brings the MSI
        // controller up, and grants the caller the matching device resource.
        // It IS audited per call — handing a principal an interrupt line is a
        // security-relevant grant and is low-volume (once per device at
        // bring-up), so the record cannot drown the log, exactly like
        // `mmio_map` / `dma_alloc`.
        required_capability: Some(CapabilityId::IRQ_BIND),
        audit: true,
    },
    SyscallSpec {
        number: SyscallNumber::SHM_CREATE,
        name: "shm_create",
        arg_count: 2,
        args: [
            // The region length in bytes, then the out pointer the new
            // region's id is written to.
            AbiType::Len,
            AbiType::UserPtr,
            AbiType::Unit,
            AbiType::Unit,
            AbiType::Unit,
            AbiType::Unit,
        ],
        // `U64` carries the mapped base virtual address (or, by the shared
        // register convention, a negated errno) back to the caller; the
        // region id is written to the `id_out` user pointer, exactly as
        // `dma_alloc` writes the device address.
        ret: AbiType::U64,
        // Creating a shared region the caller then grants to another task is
        // privileged, never ambient: only a service holding `CAP_SHM` may
        // mint one, and the kernel grants the creator only the matching
        // per-region resource. It IS audited per call — minting cross-process
        // shared memory is a security-relevant grant and is low-volume (once
        // per served device at bring-up), so the record cannot drown the log,
        // exactly like `mmio_map` / `dma_alloc`.
        required_capability: Some(CapabilityId::SHM),
        audit: true,
    },
    SyscallSpec {
        number: SyscallNumber::SHM_MAP,
        name: "shm_map",
        arg_count: 2,
        args: [
            // The grant handle, then the out pointer the mapped region's
            // byte length is written to — the kernel's own record of the
            // region size, so a server never sizes a frame slice from a
            // client's claimed geometry.
            AbiType::Handle,
            AbiType::UserPtr,
            AbiType::Unit,
            AbiType::Unit,
            AbiType::Unit,
            AbiType::Unit,
        ],
        // `U64` carries the mapped base virtual address (or a negated errno)
        // back to the driver, exactly like `mmio_map`; the region's byte
        // length is written to the `len_out` user pointer, exactly as
        // `shm_create` writes the region id.
        ret: AbiType::U64,
        // Mapping a granted shared region is privileged, never ambient: only
        // a driver granted the matched node's shared-region resource holds
        // `CAP_SHM`, and the kernel resolves the unforgeable grant handle
        // against the calling task so a driver maps only the one region it was
        // granted. It IS audited per call — handing a principal a window onto
        // another process's memory is a security-relevant grant and is
        // low-volume (once per buffer at driver init), so the record cannot
        // drown the log, exactly like `mmio_map`.
        required_capability: Some(CapabilityId::SHM),
        audit: true,
    },
    SyscallSpec {
        number: SyscallNumber::SHM_UNMAP,
        name: "shm_unmap",
        arg_count: 2,
        args: [
            AbiType::U64,
            AbiType::Len,
            AbiType::Unit,
            AbiType::Unit,
            AbiType::Unit,
            AbiType::Unit,
        ],
        ret: AbiType::Errno,
        // The release half of `shm_create` / `shm_map`; same unprivileged,
        // unaudited posture as `mem_unmap` — it only releases the caller's
        // own shared mapping and drops its reference to the region.
        required_capability: None,
        audit: false,
    },
    SyscallSpec {
        number: SyscallNumber::WAITSET_CREATE,
        name: "waitset_create",
        arg_count: 0,
        args: [AbiType::Unit; SYSCALL_MAX_ARGS],
        // `Handle` carries the kernel-minted wait-set handle (or, by the
        // shared register convention, a negated errno), exactly like
        // `irq_bind` returns its bound-line handle.
        ret: AbiType::Handle,
        // Needs no capability: the set observes only resources the caller
        // already holds, each owner-checked when added. Low-volume (once per
        // multiplexing service) but not security-relevant on its own, so it is
        // not audited.
        required_capability: None,
        audit: false,
    },
    SyscallSpec {
        number: SyscallNumber::WAITSET_CTL,
        name: "waitset_ctl",
        arg_count: 5,
        args: [
            // The wait-set handle, the op (Add/Del), the source kind
            // (Endpoint/Irq), the resource id (endpoint id or IRQ handle),
            // then the caller's opaque token for this member.
            AbiType::Handle,
            AbiType::U32,
            AbiType::U32,
            AbiType::U64,
            AbiType::U64,
            AbiType::Unit,
        ],
        ret: AbiType::Errno,
        // Needs no capability: an `Add` resolves and owner-checks the named
        // resource against the kernel-trusted caller before recording it (a
        // resource the caller does not own fails closed), so the set can never
        // observe authority the caller lacks. Modifying membership is
        // low-volume and not audited per call.
        required_capability: None,
        audit: false,
    },
    SyscallSpec {
        number: SyscallNumber::WAITSET_WAIT,
        name: "waitset_wait",
        arg_count: 3,
        args: [
            // The wait-set handle, the relative timeout in nanoseconds
            // (`u64::MAX` = no timeout), then the non-null `token_out`
            // `UserPtr` the ready member's token is written to.
            AbiType::Handle,
            AbiType::U64,
            AbiType::UserPtr,
            AbiType::Unit,
            AbiType::Unit,
            AbiType::Unit,
        ],
        ret: AbiType::Errno,
        // Needs no capability of its own: it only *observes* readiness of
        // resources the caller already holds (the members owner-checked when
        // added) and re-checks each against the kernel-trusted caller as it is
        // scanned. Like the other high-volume blocking waiters (`call_recv`,
        // `irq_wait`) it is not audited per call, to avoid drowning the audit
        // log.
        required_capability: None,
        audit: false,
    },
    SyscallSpec {
        number: SyscallNumber::FS_OPEN,
        name: "fs_open",
        arg_count: 3,
        args: [
            // Non-null `UserPtr` to the absolute path, its length, then the
            // `OpenFlags` bits.
            AbiType::UserPtr,
            AbiType::Len,
            AbiType::U32,
            AbiType::Unit,
            AbiType::Unit,
            AbiType::Unit,
        ],
        // Returns the new file descriptor; a `Handle` minted against the
        // caller's per-process descriptor table.
        ret: AbiType::Handle,
        // The coarse filesystem-access gate; the per-path authority is the
        // VFS inode model under the caller's real credentials. Opening a
        // path (which may create) is security-relevant and audited.
        required_capability: Some(CapabilityId::FS_ACCESS),
        audit: true,
    },
    SyscallSpec {
        number: SyscallNumber::FS_CLOSE,
        name: "fs_close",
        arg_count: 1,
        args: [
            AbiType::U32,
            AbiType::Unit,
            AbiType::Unit,
            AbiType::Unit,
            AbiType::Unit,
            AbiType::Unit,
        ],
        ret: AbiType::Errno,
        // Ungated at the dispatcher: a descriptor may be backed by a
        // filesystem path (opened under `CAP_FS_ACCESS`) or by a resource
        // reference (opened under its namespace's own authority), so the
        // authority is possession of the descriptor, established at open —
        // not a blanket filesystem gate re-checked on every operation. The
        // handler resolves the backing and applies the backing-specific
        // check (a path-backed descriptor still requires `CAP_FS_ACCESS`),
        // like `rlimit_set`'s fine-grained handler gate. Releasing one's own
        // descriptor is high-volume and not audited.
        required_capability: None,
        audit: false,
    },
    SyscallSpec {
        number: SyscallNumber::FS_READ,
        name: "fs_read",
        arg_count: 4,
        args: [
            // fd, byte offset, non-null `UserPtr` destination, length.
            AbiType::U32,
            AbiType::U64,
            AbiType::UserPtr,
            AbiType::Len,
            AbiType::Unit,
            AbiType::Unit,
        ],
        ret: AbiType::U64,
        // Ungated at the dispatcher: the descriptor's backing decides the
        // authority (a path-backed descriptor requires `CAP_FS_ACCESS`, a
        // resource-backed one was authorised by its namespace at open), so
        // the handler applies the backing-specific check rather than a
        // blanket filesystem gate. Reads are high-volume; not audited.
        required_capability: None,
        audit: false,
    },
    SyscallSpec {
        number: SyscallNumber::FS_WRITE,
        name: "fs_write",
        arg_count: 4,
        args: [
            // fd, byte offset, non-null `UserPtr` source, length.
            AbiType::U32,
            AbiType::U64,
            AbiType::UserPtr,
            AbiType::Len,
            AbiType::Unit,
            AbiType::Unit,
        ],
        ret: AbiType::U64,
        // Ungated at the dispatcher: the descriptor's backing decides the
        // authority (a path-backed descriptor requires `CAP_FS_ACCESS`, a
        // resource-backed one was authorised by its namespace at open), so
        // the handler applies the backing-specific check rather than a
        // blanket filesystem gate. A write mutates state; audited.
        required_capability: None,
        audit: true,
    },
    SyscallSpec {
        number: SyscallNumber::FS_READDIR,
        name: "fs_readdir",
        arg_count: 3,
        args: [
            AbiType::U32,
            AbiType::UserPtr,
            AbiType::Len,
            AbiType::Unit,
            AbiType::Unit,
            AbiType::Unit,
        ],
        ret: AbiType::U64,
        required_capability: Some(CapabilityId::FS_ACCESS),
        audit: false,
    },
    SyscallSpec {
        number: SyscallNumber::FS_STAT,
        name: "fs_stat",
        arg_count: 3,
        args: [
            AbiType::U32,
            AbiType::UserPtr,
            AbiType::Len,
            AbiType::Unit,
            AbiType::Unit,
            AbiType::Unit,
        ],
        ret: AbiType::U64,
        required_capability: Some(CapabilityId::FS_ACCESS),
        audit: false,
    },
    SyscallSpec {
        number: SyscallNumber::FS_TRUNCATE,
        name: "fs_truncate",
        arg_count: 2,
        args: [
            AbiType::U32,
            AbiType::U64,
            AbiType::Unit,
            AbiType::Unit,
            AbiType::Unit,
            AbiType::Unit,
        ],
        ret: AbiType::Errno,
        required_capability: Some(CapabilityId::FS_ACCESS),
        // Mutates persistent state; audited.
        audit: true,
    },
    SyscallSpec {
        number: SyscallNumber::FS_SYNC,
        name: "fs_sync",
        arg_count: 1,
        args: [
            AbiType::U32,
            AbiType::Unit,
            AbiType::Unit,
            AbiType::Unit,
            AbiType::Unit,
            AbiType::Unit,
        ],
        ret: AbiType::Errno,
        required_capability: Some(CapabilityId::FS_ACCESS),
        audit: false,
    },
    SyscallSpec {
        number: SyscallNumber::FS_MKDIR,
        name: "fs_mkdir",
        arg_count: 2,
        args: [
            AbiType::UserPtr,
            AbiType::Len,
            AbiType::Unit,
            AbiType::Unit,
            AbiType::Unit,
            AbiType::Unit,
        ],
        ret: AbiType::Errno,
        required_capability: Some(CapabilityId::FS_ACCESS),
        // Creates a directory; audited.
        audit: true,
    },
    SyscallSpec {
        number: SyscallNumber::FS_UNLINK,
        name: "fs_unlink",
        arg_count: 3,
        args: [
            AbiType::UserPtr,
            AbiType::Len,
            // The validated `UnlinkFlags` word: empty removes the named
            // file or (empty) directory; `DIRECTORY` restricts the removal
            // to an (empty) directory (the atomic `rmdir` posture). A
            // reserved bit fails closed at dispatch.
            AbiType::U32,
            AbiType::Unit,
            AbiType::Unit,
            AbiType::Unit,
        ],
        ret: AbiType::Errno,
        required_capability: Some(CapabilityId::FS_ACCESS),
        // Removes a name; audited.
        audit: true,
    },
    SyscallSpec {
        number: SyscallNumber::DMA_FREE,
        name: "dma_free",
        arg_count: 2,
        args: [
            AbiType::Handle,
            AbiType::U64,
            AbiType::Unit,
            AbiType::Unit,
            AbiType::Unit,
            AbiType::Unit,
        ],
        ret: AbiType::Errno,
        // Releasing a DMA buffer is gated by the same `CAP_MEM_DMA` that
        // carved it: a task that may carve a device-readable region must be
        // the one to reclaim it, and the kernel additionally frees only a
        // buffer live in the caller's own DMA window.
        required_capability: Some(CapabilityId::MEM_DMA),
        // IS audited per call, symmetric with `dma_alloc`: releasing a
        // region the hardware could touch is a security-relevant event, and
        // a long-running driver frees one buffer per transfer — low-volume
        // relative to the data it moves — so the record cannot drown the log.
        audit: true,
    },
    SyscallSpec {
        number: SyscallNumber::FS_RENAME,
        name: "fs_rename",
        arg_count: 4,
        args: [
            AbiType::UserPtr,
            AbiType::Len,
            AbiType::UserPtr,
            AbiType::Len,
            AbiType::Unit,
            AbiType::Unit,
        ],
        ret: AbiType::Errno,
        required_capability: Some(CapabilityId::FS_ACCESS),
        // Moves a name (and may replace a destination); audited like the
        // other mutating filesystem calls.
        audit: true,
    },
    SyscallSpec {
        number: SyscallNumber::CALL_PEER_ORIGIN,
        name: "call_peer_origin",
        arg_count: 4,
        args: [
            // endpoint id, in-service ticket, origin-out ptr, out cap.
            AbiType::IpcEndpoint,
            AbiType::Handle,
            AbiType::UserPtr,
            AbiType::Len,
            AbiType::Unit,
            AbiType::Unit,
        ],
        // `U64` carries the origin-bytes-written-or-`-errno` convention, like
        // `call_recv`.
        ret: AbiType::U64,
        // Gated like `call_recv`/`call_reply` by the endpoint's required
        // receive capability against the reading server (enforced in the
        // handler), not a flat dispatcher gate. Not audited per call: a
        // server reads a caller's origin on its high-volume serve path, and a
        // refused capability is audited by the dispatcher regardless.
        required_capability: None,
        audit: false,
    },
    SyscallSpec {
        number: SyscallNumber::WALL_TIME_GET,
        name: "wall_time_get",
        arg_count: 2,
        args: [
            AbiType::UserPtr,
            AbiType::Len,
            AbiType::Unit,
            AbiType::Unit,
            AbiType::Unit,
            AbiType::Unit,
        ],
        // `U64` carries the bytes-written-or-`-errno` convention, like
        // `call_peer_origin` / `call_recv`.
        ret: AbiType::U64,
        // Reading the wall clock is unprivileged, like `clock_get`: any task
        // may ask what time it is. Not audited — a pure observer, and a
        // high-volume one for a time-stamping caller.
        required_capability: None,
        audit: false,
    },
    SyscallSpec {
        number: SyscallNumber::WALL_TIME_SET,
        name: "wall_time_set",
        arg_count: 3,
        args: [
            AbiType::UserPtr,
            AbiType::Len,
            AbiType::U32,
            AbiType::Unit,
            AbiType::Unit,
            AbiType::Unit,
        ],
        ret: AbiType::Errno,
        // Driving the system clock is a privileged, security-relevant act
        // (it can move timestamps and certificate-validity windows), so it
        // is gated by `CAP_TIME_SET` and audited per call. The setter is
        // low-volume (a boot seed, occasional re-syncs).
        required_capability: Some(CapabilityId::TIME_SET),
        audit: true,
    },
    SyscallSpec {
        number: SyscallNumber::BOOT_ID_GET,
        name: "boot_id_get",
        arg_count: 2,
        args: [
            AbiType::UserPtr,
            AbiType::Len,
            AbiType::Unit,
            AbiType::Unit,
            AbiType::Unit,
            AbiType::Unit,
        ],
        // `U64` carries the bytes-written-or-`-errno` convention, like
        // `wall_time_get`.
        ret: AbiType::U64,
        // The boot id is a public per-boot nonce, not a secret, so reading it
        // is unprivileged like `clock_get` / `wall_time_get`. Not audited — a
        // pure observer.
        required_capability: None,
        audit: false,
    },
    SyscallSpec {
        number: SyscallNumber::SYSINFO_INTROSPECT,
        name: "sysinfo_introspect",
        arg_count: 4,
        args: [
            // domain, arg (selector/offset), out ptr, out capacity.
            AbiType::U32,
            AbiType::U64,
            AbiType::UserPtr,
            AbiType::Len,
            AbiType::Unit,
            AbiType::Unit,
        ],
        // `U64` carries the bytes-written-or-`-errno` register convention
        // `hw_tree_read` / `users_db_read` use.
        ret: AbiType::U64,
        // The unfiltered global system view is privileged and held only by
        // the `sysinfod` broker, gated exactly like the hardware-tree read.
        // Not audited per call: the broker re-reads on every client query (it
        // is the high-volume consumer) and the audited security decision is
        // the client-facing query the broker records, not this observation;
        // a capability denial is audited by the dispatcher regardless.
        required_capability: Some(CapabilityId::SYSINFO_INTROSPECT),
        audit: false,
    },
    SyscallSpec {
        number: SyscallNumber::TERMINAL_SIZE,
        name: "terminal_size",
        arg_count: 3,
        args: [
            // The standard descriptor to query, then the out buffer the
            // encoded `TerminalSize` is written into, then its capacity.
            AbiType::U32,
            AbiType::UserPtr,
            AbiType::Len,
            AbiType::Unit,
            AbiType::Unit,
            AbiType::Unit,
        ],
        // `U64` carries the bytes-written-or-`-errno` register convention
        // `wall_time_get` / `boot_id_get` use.
        ret: AbiType::U64,
        // Asking how big one's own terminal is unprivileged, like
        // `clock_get` / `wall_time_get`. Not audited — a pure observer a
        // full-screen program may re-read freely.
        required_capability: None,
        audit: false,
    },
    SyscallSpec {
        number: SyscallNumber::SIGNAL,
        name: "signal",
        arg_count: 2,
        args: [
            // The child PID to signal (an `I32`, sign-extended in the
            // register per the ABI convention), then the `Signal`
            // discriminant. The handler validates both.
            AbiType::I32,
            AbiType::U32,
            AbiType::Unit,
            AbiType::Unit,
            AbiType::Unit,
            AbiType::Unit,
        ],
        ret: AbiType::Errno,
        // Signalling a child the caller spawned is the unprivileged baseline
        // (like `wait`, the parent/child relationship is the authority): it
        // grants no authority over any other principal, so no capability is
        // required. It IS audited per call — delivering a signal is a
        // security-relevant process-lifecycle decision, exactly as
        // `spawn`/`wait`/`exit` are audited.
        required_capability: None,
        audit: true,
    },
    SyscallSpec {
        number: SyscallNumber::FS_CHDIR,
        name: "fs_chdir",
        arg_count: 2,
        args: [
            // Non-null `UserPtr` to the (absolute or cwd-relative) path, then
            // its length.
            AbiType::UserPtr,
            AbiType::Len,
            AbiType::Unit,
            AbiType::Unit,
            AbiType::Unit,
            AbiType::Unit,
        ],
        ret: AbiType::Errno,
        // The coarse filesystem-access gate; the per-path authority is the
        // VFS inode model (search on the target directory) under the caller's
        // real credentials. Changing the working directory is a
        // security-relevant resolve+authorise, so it is audited like
        // `fs_open`.
        required_capability: Some(CapabilityId::FS_ACCESS),
        audit: true,
    },
    SyscallSpec {
        number: SyscallNumber::FS_GETCWD,
        name: "fs_getcwd",
        arg_count: 2,
        args: [
            // Non-null `UserPtr` the working directory is written into, then
            // its capacity.
            AbiType::UserPtr,
            AbiType::Len,
            AbiType::Unit,
            AbiType::Unit,
            AbiType::Unit,
            AbiType::Unit,
        ],
        // `U64` carries the bytes-written-or-`-errno` register convention
        // `terminal_size` / `wall_time_get` use.
        ret: AbiType::U64,
        // Reading one's own working directory grants no authority, so — like
        // `terminal_size` — it needs no capability and is not audited.
        required_capability: None,
        audit: false,
    },
    SyscallSpec {
        number: SyscallNumber::RESOURCE_OPEN,
        name: "resource_open",
        arg_count: 3,
        args: [
            // Non-null `UserPtr` to the textual resource reference, its
            // length (at most `RESOURCE_REF_MAX`), then the `OpenFlags` bits.
            AbiType::UserPtr,
            AbiType::Len,
            AbiType::U32,
            AbiType::Unit,
            AbiType::Unit,
            AbiType::Unit,
        ],
        // Returns the new descriptor; a `Handle` minted against the caller's
        // per-process descriptor space, exactly as `fs_open` does.
        ret: AbiType::Handle,
        // Ungated at the dispatcher: authorisation is per namespace and
        // selector inside the resolver (an unprivileged resource such as
        // `sys:random` needs no capability; a privileged namespace is
        // checked against the kernel-attested caller and fails closed),
        // mirroring how `ipc_call` / `rlimit_set` carry no blanket gate but
        // enforce a fine-grained check in the handler. Resolving a resource
        // to a descriptor is a security-relevant decision, so it IS audited
        // per call, like `fs_open`.
        required_capability: None,
        audit: true,
    },
    SyscallSpec {
        number: SyscallNumber::SELF_ORIGIN,
        name: "self_origin",
        arg_count: 2,
        args: [
            // Non-null `UserPtr` to the caller's output buffer, then its
            // capacity (at least `ORIGIN_WIRE_LEN`).
            AbiType::UserPtr,
            AbiType::Len,
            AbiType::Unit,
            AbiType::Unit,
            AbiType::Unit,
            AbiType::Unit,
        ],
        // `U64` carries the bytes-written-or-`-errno` register convention
        // `call_peer_origin` / `boot_id_get` use.
        ret: AbiType::U64,
        // A task may always read its own kernel-attested identity; doing so
        // grants no authority over any other principal, so — like `boot_id_get`
        // — it needs no capability and is not audited (a pure self-observer).
        required_capability: None,
        audit: false,
    },
    SyscallSpec {
        number: SyscallNumber::USERS_ADMIN,
        name: "users_admin",
        arg_count: 4,
        args: [
            // Non-null `UserPtr` to the typed request record, its length,
            // then the non-null `UserPtr` response buffer the list
            // operations fill and its capacity.
            AbiType::UserPtr,
            AbiType::Len,
            AbiType::UserPtr,
            AbiType::Len,
            AbiType::Unit,
            AbiType::Unit,
        ],
        // `U64` carries the bytes-written-or-`-errno` register convention
        // `hw_tree_read` / `users_db_read` use (mutating operations answer
        // zero bytes).
        ret: AbiType::U64,
        // Editing the account databases is the account-administration
        // authority, never ambient: gated on `CAP_USER_ADMIN` at dispatch,
        // with the finer never-widen / last-administrator / format checks
        // enforced in the handler. Every call IS audited — account
        // administration is a security-relevant decision and is low-volume,
        // so the records cannot drown the log.
        required_capability: Some(CapabilityId::USER_ADMIN),
        audit: true,
    },
    SyscallSpec {
        number: SyscallNumber::SEAT_SWITCH,
        name: "seat_switch",
        arg_count: 2,
        args: [
            // The seat to retarget, then the index of the installed text
            // console that becomes its foreground.
            AbiType::U64,
            AbiType::U32,
            AbiType::Unit,
            AbiType::Unit,
            AbiType::Unit,
            AbiType::Unit,
        ],
        ret: AbiType::Errno,
        // Retargeting a seat's foreground redirects every subsequent
        // keystroke of an unowned seat — the console-hijack primitive — so
        // it is the seat-multiplexing authority's alone (`CAP_SEAT_ADMIN`,
        // held only by the seat manager), never ambient and never a
        // `CAP_DISPLAY` power. A security-relevant ownership change, so it
        // IS audited per call; switches are low-volume (a session
        // hand-over), so the record cannot drown the log.
        required_capability: Some(CapabilityId::SEAT_ADMIN),
        audit: true,
    },
    SyscallSpec {
        number: SyscallNumber::SEAT_REVOKE,
        name: "seat_revoke",
        arg_count: 1,
        args: [
            // The seat whose current lease is revoked.
            AbiType::U64,
            AbiType::Unit,
            AbiType::Unit,
            AbiType::Unit,
            AbiType::Unit,
            AbiType::Unit,
        ],
        ret: AbiType::Errno,
        // Evicting another principal's lease is the seat-multiplexing
        // authority's alone (`CAP_SEAT_ADMIN`), never ambient: `CAP_DISPLAY`
        // owns one lease and cannot revoke another's. A security-relevant
        // ownership change, so it IS audited per call — the handler's record
        // carries the evicted owner's task id, so every eviction is
        // attributable — and revocations are low-volume, so the record
        // cannot drown the log.
        required_capability: Some(CapabilityId::SEAT_ADMIN),
        audit: true,
    },
    SyscallSpec {
        number: SyscallNumber::CONSOLE_FOREGROUND,
        name: "console_foreground",
        arg_count: 2,
        args: [
            // The readable standard-stream descriptor naming the console,
            // then the child PID to mark foreground (`I32`, sign-extended
            // per the ABI convention; `0` clears the slot).
            AbiType::U32,
            AbiType::I32,
            AbiType::Unit,
            AbiType::Unit,
            AbiType::Unit,
            AbiType::Unit,
        ],
        ret: AbiType::Errno,
        // The controlling ownership is a property of the console the
        // reader holds, so the control shares `stream_input_mode`'s
        // `CAP_CONSOLE_READ` gate; the *target* authority is the
        // parent/child relationship the handler validates, and the slot
        // transition itself is owner/granter-checked on the device
        // (`plans/DISPLAY.md` D5) so a bystander can neither take nor
        // clear the drain right. It IS audited — redirecting who drains
        // the console and receives `^C`/`^Z` signal delivery is a
        // security-relevant process-lifecycle decision (like `signal`) and
        // is low-volume (once per foreground job), so the record cannot
        // drown the log.
        required_capability: Some(CapabilityId::CONSOLE_READ),
        audit: true,
    },
    SyscallSpec {
        number: SyscallNumber::PIPE_CREATE,
        name: "pipe_create",
        arg_count: 1,
        args: [
            // The out-pointer the kernel writes the two new descriptors
            // into: the read end first, then the write end (two `u32`s).
            AbiType::UserPtr,
            AbiType::Unit,
            AbiType::Unit,
            AbiType::Unit,
            AbiType::Unit,
            AbiType::Unit,
        ],
        ret: AbiType::Errno,
        // Unprivileged: a pipe mints two descriptors of the caller's own
        // open table and reaches nothing else — no cross-principal
        // authority exists to gate (`plans/SPAWN.md` SP10; the `mem_map`
        // precedent). Handing an end to a child rides the
        // `CAP_PROC_SPAWN`-gated spawn. Not audited: creating a pipe is a
        // high-volume, security-neutral allocation (every shell pipeline
        // mints one), and the spawn that transfers an end IS audited.
        required_capability: None,
        audit: false,
    },
    SyscallSpec {
        number: SyscallNumber::FS_SET_MODE,
        name: "fs_set_mode",
        arg_count: 3,
        args: [
            AbiType::UserPtr,
            AbiType::Len,
            // The new permission bits. A word carrying any bit above
            // `FS_MODE_MASK` (the `rwx` triads plus setuid/setgid/sticky)
            // fails closed at dispatch — never masked to a mode the caller
            // did not ask for.
            AbiType::U32,
            AbiType::Unit,
            AbiType::Unit,
            AbiType::Unit,
        ],
        ret: AbiType::Errno,
        required_capability: Some(CapabilityId::FS_ACCESS),
        // Rewrites an inode's permission bits; audited.
        audit: true,
    },
    SyscallSpec {
        number: SyscallNumber::PORT_RESOLVE,
        name: "port_resolve",
        arg_count: 2,
        args: [
            // The ASCII name bytes; validated against the `PortName`
            // grammar kernel-side before the registry is consulted.
            AbiType::UserPtr,
            AbiType::Len,
            AbiType::Unit,
            AbiType::Unit,
            AbiType::Unit,
            AbiType::Unit,
        ],
        ret: AbiType::IpcEndpoint,
        // Unprivileged: resolving a name grants nothing — every send is
        // still capability-checked at the port, and publication is a
        // kernel-side bind-authority-checked operation. Not audited: a
        // pure observer, like `cap_query` and `clock_get`.
        required_capability: None,
        audit: false,
    },
    SyscallSpec {
        number: SyscallNumber::FILE_MAP,
        name: "file_map",
        arg_count: 3,
        args: [
            // The descriptor to map; must be open for reading and backed
            // by a filesystem path (a resource or pipe backing is refused
            // by the handler — only a positional byte store can page).
            AbiType::U32,
            // The file byte offset the mapping starts at (page-aligned).
            // Always 64-bit: a mappable file may exceed both `usize` and
            // any 32-bit figure (storage width is never pointer width).
            AbiType::U64,
            // The mapping length in bytes (rounded up to whole pages);
            // 64-bit for the same reason as the offset.
            AbiType::U64,
            AbiType::Unit,
            AbiType::Unit,
            AbiType::Unit,
        ],
        ret: AbiType::U64,
        // The mapping is a read view of a filesystem object, so the same
        // coarse gate as the other filesystem calls applies at dispatch;
        // per-inode owner/mode/ACL enforcement is the secured VFS's, and
        // is re-applied by the fault path on every demand-paged read
        // under the mapping-time identity. Like `fs_read` it is not
        // audited per call.
        required_capability: Some(CapabilityId::FS_ACCESS),
        audit: false,
    },
    SyscallSpec {
        number: SyscallNumber::FILE_UNMAP,
        name: "file_unmap",
        arg_count: 2,
        args: [
            AbiType::U64,
            AbiType::U64,
            AbiType::Unit,
            AbiType::Unit,
            AbiType::Unit,
            AbiType::Unit,
        ],
        ret: AbiType::Errno,
        // The release half of `file_map`: it only shrinks the caller's
        // own address space, so it is the unprivileged, unaudited
        // baseline (the `mem_unmap` posture).
        required_capability: None,
        audit: false,
    },
    SyscallSpec {
        number: SyscallNumber::POINTER_INJECT,
        name: "pointer_inject",
        arg_count: 3,
        args: [
            // The seat the decoded pointer event belongs to (the seat whose
            // pointing device produced it), then the encoded record. An
            // unknown seat id fails closed with `NotFound`.
            AbiType::U64,
            AbiType::UserPtr,
            AbiType::Len,
            AbiType::Unit,
            AbiType::Unit,
            AbiType::Unit,
        ],
        // `U64` so the C view carries the bytes-consumed-or-`-errno`
        // register convention `key_inject` uses.
        ret: AbiType::U64,
        // Feeding the system pointer stream is privileged, never ambient:
        // only the pointer-input driver that decoded a discovered device
        // holds `CAP_INPUT_INJECT` — the same gate, and the same posture,
        // as `key_inject`. It fires once per motion/button event (far more
        // often than a key edge), so auditing every call would drown the
        // log — it is NOT audited; the device manager's one-time driver
        // load IS the audited security decision.
        required_capability: Some(CapabilityId::INPUT_INJECT),
        audit: false,
    },
    SyscallSpec {
        number: SyscallNumber::POINTER_READ,
        name: "pointer_read",
        arg_count: 3,
        args: [
            // The seat whose desktop pointer channel is drained (only its
            // owner may), then the record buffer. An unknown seat id fails
            // closed with `NotFound`.
            AbiType::U64,
            AbiType::UserPtr,
            AbiType::Len,
            AbiType::Unit,
            AbiType::Unit,
            AbiType::Unit,
        ],
        // `U64` so the C view carries the bytes-read-or-`-errno` register
        // convention `keyboard_read` uses.
        ret: AbiType::U64,
        // Reading the pointer channel is privileged, never ambient: the
        // capability is `CAP_INPUT_READ`, and the drain is additionally
        // owner-gated kernel-side against the seat's live lease, so pointer
        // input is delivered only to the task that owns the surface
        // (`plans/DISPLAY.md`) — the same double gate as `keyboard_read`.
        // Like the other high-volume stream readers it is NOT audited.
        required_capability: Some(CapabilityId::INPUT_READ),
        audit: false,
    },
    SyscallSpec {
        number: SyscallNumber::VOLUME_ATTACH,
        name: "volume_attach",
        arg_count: 2,
        args: [
            // Non-null `UserPtr` to the encoded `VolumeAttachRequest`,
            // then its length (at most `VOLUME_ATTACH_MAX_LEN`).
            AbiType::UserPtr,
            AbiType::Len,
            AbiType::Unit,
            AbiType::Unit,
            AbiType::Unit,
            AbiType::Unit,
        ],
        ret: AbiType::Errno,
        // Attaching a filesystem and publishing its root reshapes the
        // storage namespace for every principal, so it carries the mount
        // authority and every decision is audited (the drives.md
        // fs.root.attached / fs.hotplug.root_added events).
        required_capability: Some(CapabilityId::FS_MOUNT),
        audit: true,
    },
    SyscallSpec {
        number: SyscallNumber::VOLUME_DETACH,
        name: "volume_detach",
        arg_count: 2,
        args: [
            // Non-null `UserPtr` to the encoded `VolumeDetachRequest`
            // (the 16-byte volume identity), then its exact length.
            AbiType::UserPtr,
            AbiType::Len,
            AbiType::Unit,
            AbiType::Unit,
            AbiType::Unit,
            AbiType::Unit,
        ],
        ret: AbiType::Errno,
        // The retraction half of `volume_attach`: the same mount
        // authority, and every decision is audited (the drives.md
        // fs.hotplug.root_removed events).
        required_capability: Some(CapabilityId::FS_MOUNT),
        audit: true,
    },
    SyscallSpec {
        number: SyscallNumber::SHM_GRANT,
        name: "shm_grant",
        arg_count: 2,
        args: [
            // The shared-region id the caller owns, then the call-endpoint
            // id whose serving task receives the map grant.
            AbiType::Handle,
            AbiType::IpcEndpoint,
            AbiType::Unit,
            AbiType::Unit,
            AbiType::Unit,
            AbiType::Unit,
        ],
        // `U64` carries the minted grant handle (or a negated errno) back to
        // the caller, who forwards it in-band; the handle is owner-checked
        // at `shm_map`, so the value is useless to a bystander.
        ret: AbiType::U64,
        // Delegating a mapping of cross-process shared memory is privileged
        // exactly as minting one: the same `CAP_SHM` gate as `shm_create`,
        // and every mint is audited — it is a security-relevant grant and
        // low-volume (once per display-surface configure), so the record
        // cannot drown the log.
        required_capability: Some(CapabilityId::SHM),
        audit: true,
    },
    SyscallSpec {
        number: SyscallNumber::CALL_PEER_SEAT,
        name: "call_peer_seat",
        arg_count: 3,
        args: [
            // endpoint id, in-service ticket, then the seat id checked.
            AbiType::IpcEndpoint,
            AbiType::Handle,
            AbiType::Handle,
            AbiType::Unit,
            AbiType::Unit,
            AbiType::Unit,
        ],
        // `U64` carries the live lease generation (>= 1) or a negated errno.
        ret: AbiType::U64,
        // Gated like `call_peer_origin` by the endpoint's required receive
        // capability against the reading server (enforced in the handler),
        // not a flat dispatcher gate. Not audited per call: it is the
        // per-frame present gate on the display service's serve path — the
        // kernel-side `PresentGate` is not audited per check either.
        required_capability: None,
        audit: false,
    },
    SyscallSpec {
        number: SyscallNumber::FS_ATTR_GET,
        name: "fs_attr_get",
        arg_count: 6,
        args: [
            // Path bytes, then the fsmeta-grammar key bytes, then the
            // caller's value-out buffer. The dispatcher bounds the key
            // length to 1..=FS_ATTR_KEY_MAX before any copy; the grammar
            // itself is validated by the secured VFS through lib/fsmeta.
            AbiType::UserPtr,
            AbiType::Len,
            AbiType::UserPtr,
            AbiType::Len,
            AbiType::UserPtr,
            AbiType::Len,
        ],
        // `U64` carries the value byte count (or a negated errno; an
        // absent attribute is `-NoData`, never an empty read).
        ret: AbiType::U64,
        required_capability: Some(CapabilityId::FS_ACCESS),
        // A pure read, high-volume like fs_stat; not audited per call.
        audit: false,
    },
    SyscallSpec {
        number: SyscallNumber::FS_ATTR_SET,
        name: "fs_attr_set",
        arg_count: 6,
        args: [
            // Path bytes, key bytes, then the opaque value bytes. The
            // dispatcher bounds the key to 1..=FS_ATTR_KEY_MAX and the
            // value to FS_ATTR_VALUE_MAX before any copy.
            AbiType::UserPtr,
            AbiType::Len,
            AbiType::UserPtr,
            AbiType::Len,
            AbiType::UserPtr,
            AbiType::Len,
        ],
        ret: AbiType::Errno,
        required_capability: Some(CapabilityId::FS_ACCESS),
        // Rewrites persistent inode metadata; audited like fs_set_mode.
        audit: true,
    },
    SyscallSpec {
        number: SyscallNumber::FS_ATTR_LIST,
        name: "fs_attr_list",
        arg_count: 5,
        args: [
            // Path bytes, the index to yield, then the caller's key-out
            // buffer (the fs_readdir one-entry-per-call iteration shape).
            AbiType::UserPtr,
            AbiType::Len,
            AbiType::U64,
            AbiType::UserPtr,
            AbiType::Len,
            AbiType::Unit,
        ],
        // `U64` carries the key byte count, `0` for end-of-list (a real
        // key is never empty), or a negated errno.
        ret: AbiType::U64,
        required_capability: Some(CapabilityId::FS_ACCESS),
        // A pure read, high-volume like fs_readdir; not audited per call.
        audit: false,
    },
    SyscallSpec {
        number: SyscallNumber::FS_ATTR_REMOVE,
        name: "fs_attr_remove",
        arg_count: 4,
        args: [
            // Path bytes, then the key bytes of the attribute to remove.
            AbiType::UserPtr,
            AbiType::Len,
            AbiType::UserPtr,
            AbiType::Len,
            AbiType::Unit,
            AbiType::Unit,
        ],
        ret: AbiType::Errno,
        required_capability: Some(CapabilityId::FS_ACCESS),
        // Rewrites persistent inode metadata; audited like fs_set_mode.
        audit: true,
    },
    SyscallSpec {
        number: SyscallNumber::PORT_BIND,
        name: "port_bind",
        arg_count: 3,
        args: [
            // Port id, maximum payload bytes, mailbox capacity (both
            // fail-closed bounds re-checked against the ABI ceilings in
            // the handler).
            AbiType::IpcEndpoint,
            AbiType::Len,
            AbiType::Len,
            AbiType::Unit,
            AbiType::Unit,
            AbiType::Unit,
        ],
        ret: AbiType::Errno,
        // No flat dispatcher gate: an unrestricted port is an ordinary
        // process resource (the app's window-event mailbox). Binding a
        // reserved well-known id still requires CAP_IPC_BIND_PRIVILEGED,
        // enforced in `Port::create` exactly as `call_create` does.
        required_capability: None,
        // Binding a rendezvous point is a security-relevant, low-volume
        // decision, audited like `call_create`.
        audit: true,
    },
    SyscallSpec {
        number: SyscallNumber::BOOT_FACTS_GET,
        name: "boot_facts_get",
        arg_count: 2,
        args: [
            AbiType::UserPtr,
            AbiType::Len,
            AbiType::Unit,
            AbiType::Unit,
            AbiType::Unit,
            AbiType::Unit,
        ],
        // `U64` carries the bytes-written-or-`-errno` convention, like
        // `boot_id_get`.
        ret: AbiType::U64,
        // The boot facts are the machine's public shape (arch, core count,
        // installed memory), minted once at boot and immutable — not live
        // state and not a secret — so reading them is unprivileged like
        // `boot_id_get`. Not audited — a pure observer.
        required_capability: None,
        audit: false,
    },
    SyscallSpec {
        number: SyscallNumber::FD_GRANT,
        name: "fd_grant",
        arg_count: 2,
        args: [
            // The caller's own path-backed descriptor, then the recipient's
            // kernel task id (from a kernel-attested source; task ids are
            // never reused).
            AbiType::U32,
            AbiType::U64,
            AbiType::Unit,
            AbiType::Unit,
            AbiType::Unit,
            AbiType::Unit,
        ],
        // `U64` carries the minted grant handle (or a negated errno) back to
        // the caller, who forwards it in-band; the handle resolves only when
        // presented by the recipient task (`fd_redeem`), so the value is
        // useless to a bystander — the `shm_grant` shape.
        ret: AbiType::U64,
        // Delegating filesystem authority is gated exactly as acquiring it:
        // the same `CAP_FS_ACCESS` the descriptor's `fs_open` required. The
        // mint is audited — a user-mediated, security-relevant grant of
        // authority to another process, and low-volume (once per picker
        // choice), so the record cannot drown the log.
        required_capability: Some(CapabilityId::FS_ACCESS),
        audit: true,
    },
    SyscallSpec {
        number: SyscallNumber::FD_REDEEM,
        name: "fd_redeem",
        arg_count: 1,
        args: [
            // The grant handle minted to the calling task by `fd_grant`.
            AbiType::Handle,
            AbiType::Unit,
            AbiType::Unit,
            AbiType::Unit,
            AbiType::Unit,
            AbiType::Unit,
        ],
        // `U64` carries the freshly installed descriptor number (or a
        // negated errno), like `fs_open`.
        ret: AbiType::U64,
        // Ungated: receiving user-mediated, already-checked authority is the
        // point of the delegation — the recipient may hold no filesystem
        // capability at all, and every later operation on the descriptor is
        // still VFS-checked under the grantor's captured identity. The
        // redemption is audited so the grant's consumption is attributable
        // in the same trail as its mint; it is one-shot, so the volume is
        // bounded by the audited mints.
        required_capability: None,
        audit: true,
    },
    SyscallSpec {
        number: SyscallNumber::MEM_PIN,
        name: "mem_pin",
        arg_count: 0,
        args: [
            AbiType::Unit,
            AbiType::Unit,
            AbiType::Unit,
            AbiType::Unit,
            AbiType::Unit,
            AbiType::Unit,
        ],
        ret: AbiType::Errno,
        // Exempting the caller's anonymous memory from the swap tiers is a
        // system-wide denial-of-service lever (pinned bytes can never be
        // reclaimed by compression), so the pin is gated on the dedicated
        // capability and audited per call — a pin is a security-relevant
        // resource decision, and the volume is low (once per monitor/
        // controller start), so the record cannot drown the log.
        required_capability: Some(CapabilityId::MEM_PIN),
        audit: true,
    },
    SyscallSpec {
        number: SyscallNumber::MEM_UNPIN,
        name: "mem_unpin",
        arg_count: 0,
        args: [
            AbiType::Unit,
            AbiType::Unit,
            AbiType::Unit,
            AbiType::Unit,
            AbiType::Unit,
            AbiType::Unit,
        ],
        ret: AbiType::Errno,
        // Ungated: releasing the caller's own exemption narrows its
        // footprint and grants nothing (the `mem_unmap` posture). Audited
        // like `mem_pin`, so the trail carries both edges of every pin
        // window.
        required_capability: None,
        audit: true,
    },
    SyscallSpec {
        number: SyscallNumber::SIGNAL_INTAKE,
        name: "signal_intake",
        arg_count: 1,
        args: [
            // The `SignalIntakeOp` discriminant; the dispatcher rejects an
            // unknown value before the handler runs (fail closed).
            AbiType::U32,
            AbiType::Unit,
            AbiType::Unit,
            AbiType::Unit,
            AbiType::Unit,
            AbiType::Unit,
        ],
        // `U64` carries the value-or-negative-errno convention: `Take`
        // returns the drained signal's wire discriminant, the other ops 0.
        ret: AbiType::U64,
        // Own-process signal disposition grants no authority over any other
        // principal (the `stream_input_mode` tier), so no capability is
        // required. It IS audited per call — changing one's own
        // termination-signal disposition and draining an observed delivery
        // are security-relevant process-lifecycle decisions, exactly as
        // `signal` is audited — so the trail carries the opt-in, the
        // opt-out, and every observed delivery's drain.
        required_capability: None,
        audit: true,
    },
    SyscallSpec {
        number: SyscallNumber::SCHED_SET_REALTIME,
        name: "sched_set_realtime",
        arg_count: 1,
        args: [
            // `realtime` boolean: non-zero enters the strict-priority
            // real-time class, zero returns to the fair time-shared class.
            AbiType::U32,
            AbiType::Unit,
            AbiType::Unit,
            AbiType::Unit,
            AbiType::Unit,
            AbiType::Unit,
        ],
        ret: AbiType::Errno,
        // Entering the real-time class lets a task preempt every ordinary
        // task on its CPU system-wide, so the whole syscall is gated on the
        // dedicated capability. A task's scheduling class is per-task state
        // and the capability is static, so only a holder is ever real-time
        // and only a holder ever needs to leave the class: gating both
        // directions denies a legitimate caller nothing while keeping the
        // privileged direction firmly closed. Audited per call: entering or
        // leaving strict priority is a security-relevant scheduling
        // decision, and the volume is low (once per driver start).
        required_capability: Some(CapabilityId::SCHED_REALTIME),
        audit: true,
    },
    SyscallSpec {
        number: SyscallNumber::FS_SET_OWNER,
        name: "fs_set_owner",
        arg_count: 4,
        args: [
            AbiType::UserPtr,
            AbiType::Len,
            // The new owning user id, or `FS_OWNER_UNCHANGED` to leave it.
            AbiType::U32,
            // The new owning group id, or `FS_OWNER_UNCHANGED` to leave it.
            AbiType::U32,
            AbiType::Unit,
            AbiType::Unit,
        ],
        ret: AbiType::Errno,
        // The coarse "may use the filesystem at all" gate, like the other
        // path-taking calls; the privileged per-inode rule (reassigning the
        // uid, or setting a gid the caller is not a member of, requires
        // `CAP_FS_CHOWN`) is the secured VFS's. Rewrites persistent inode
        // ownership metadata; audited like `fs_set_mode`.
        required_capability: Some(CapabilityId::FS_ACCESS),
        audit: true,
    },
    SyscallSpec {
        number: SyscallNumber::PTY_CREATE,
        name: "pty_create",
        arg_count: 3,
        args: [
            // The out-pointer the kernel writes the two new descriptors
            // into: the master end first, then the slave end (two `u32`s).
            AbiType::UserPtr,
            // The pty's initial row count; non-zero and `u16`-bounded, else
            // `OutOfRange` before any state is touched.
            AbiType::U32,
            // The pty's initial column count; same bounds.
            AbiType::U32,
            AbiType::Unit,
            AbiType::Unit,
            AbiType::Unit,
        ],
        ret: AbiType::Errno,
        // Unprivileged, exactly like `pipe_create`: a pty mints two
        // descriptors of the caller's own open table and reaches nothing
        // else, so there is no cross-principal authority to gate. Handing
        // the slave to a child rides the `CAP_PROC_SPAWN`-gated spawn. Not
        // audited: creating a pty is a security-neutral allocation (a
        // terminal mints one per shell), and the spawn that transfers the
        // slave IS audited.
        required_capability: None,
        audit: false,
    },
];

/// Length, in bytes, of the canonical encoding stored in
/// [`ENCODED_TABLE`].
///
/// Derived from [`SYSCALLS`]'s length so that appending a syscall row keeps
/// the encoding buffer in step automatically (`abi-v1` grows by appending —
/// existing rows never change).
pub const ENCODED_TABLE_LEN: usize = SYSCALL_ENCODED_RECORD_LEN * SYSCALLS.len();

/// Canonical byte representation of [`SYSCALLS`].
///
/// Computed in a `const fn` so that the encoding is fully determined at
/// compile time; `cargo xtask abi-check` hashes this buffer with SHA-256
/// and compares the result against the kernel-side `SYSCALL_TABLE_HASH`
/// literal. See the module-level layout table.
pub const ENCODED_TABLE: [u8; ENCODED_TABLE_LEN] = encode_table();

const fn encode_table() -> [u8; ENCODED_TABLE_LEN] {
    let mut out = [0u8; ENCODED_TABLE_LEN];
    let mut i = 0;
    while i < SYSCALLS.len() {
        let spec = &SYSCALLS[i];
        let base = i * SYSCALL_ENCODED_RECORD_LEN;
        let number = spec.number.as_u16();
        let [n_lo, n_hi] = number.to_le_bytes();
        out[base] = n_lo;
        out[base + 1] = n_hi;
        out[base + 2] = spec.arg_count;
        out[base + 3] = spec.ret.as_u8();
        let mut a = 0;
        while a < SYSCALL_MAX_ARGS {
            out[base + 4 + a] = spec.args[a].as_u8();
            a += 1;
        }
        let (present, cap_id) = match spec.required_capability {
            Some(c) => (1u8, c.as_u16()),
            None => (0u8, 0u16),
        };
        out[base + 10] = present;
        let [c_lo, c_hi] = cap_id.to_le_bytes();
        out[base + 11] = c_lo;
        out[base + 12] = c_hi;
        out[base + 13] = spec.audit as u8;
        // Name (ASCII), right-padded to SYSCALL_NAME_MAX with NUL.
        let name = spec.name.as_bytes();
        // Reject overlong names at compile time. `assert!` in a `const`
        // context surfaces the diagnostic at the use-site, so a future
        // rename that exceeds the fixed stride fails to build rather
        // than silently truncate the encoding.
        assert!(
            name.len() <= SYSCALL_NAME_MAX,
            "syscall name exceeds SYSCALL_NAME_MAX"
        );
        let mut n = 0;
        while n < name.len() {
            out[base + 14 + n] = name[n];
            n += 1;
        }
        i += 1;
    }
    out
}

/// Look up the [`SyscallSpec`] for a given identifier.
///
/// Returns `None` if `number` is not assigned in `abi-v1` (either above
/// the populated range or a reserved gap — there are no gaps today).
#[must_use]
pub const fn spec_for(number: SyscallNumber) -> Option<&'static SyscallSpec> {
    let raw = number.as_u16() as usize;
    if raw < SYSCALLS.len() {
        let spec = &SYSCALLS[raw];
        // Defence in depth: SYSCALLS[i].number must equal i. The dedicated
        // unit test below pins this invariant; the runtime check exists so
        // a future re-shuffle that silently breaks the index cannot escape
        // a non-test caller either.
        if spec.number.as_u16() as usize == raw {
            return Some(spec);
        }
    }
    None
}

/// Borrow the canonical encoding as a byte slice.
///
/// Convenience wrapper around [`ENCODED_TABLE`] so callers do not have to
/// name the constant explicitly when feeding a hasher.
#[must_use]
pub const fn encoded_table() -> &'static [u8] {
    &ENCODED_TABLE
}

#[cfg(test)]
mod tests {
    use super::{
        encoded_table, spec_for, AbiType, ENCODED_TABLE, ENCODED_TABLE_LEN, SYSCALLS,
        SYSCALL_ENCODED_RECORD_LEN, SYSCALL_MAX_ARGS, SYSCALL_NAME_MAX,
    };
    use crate::{CapabilityId, SyscallNumber};

    #[test]
    fn table_is_dense_and_ordered() {
        for (idx, spec) in SYSCALLS.iter().enumerate() {
            assert_eq!(spec.number.as_u16() as usize, idx, "{}", spec.name);
        }
    }

    #[test]
    fn arg_counts_are_within_bounds_and_trailing_slots_are_unit() {
        for spec in SYSCALLS {
            assert!(
                (spec.arg_count as usize) <= SYSCALL_MAX_ARGS,
                "{} arg_count out of range",
                spec.name
            );
            for slot in spec.args.iter().skip(spec.arg_count as usize) {
                assert_eq!(
                    *slot,
                    AbiType::Unit,
                    "{} has non-Unit trailing arg slot",
                    spec.name
                );
            }
        }
    }

    #[test]
    fn names_are_ascii_and_fit() {
        for spec in SYSCALLS {
            assert!(spec.name.is_ascii(), "{} non-ASCII name", spec.name);
            assert!(
                spec.name.len() <= SYSCALL_NAME_MAX,
                "{} exceeds SYSCALL_NAME_MAX",
                spec.name
            );
        }
    }

    #[test]
    fn spec_for_lookup_matches_table() {
        for spec in SYSCALLS {
            let found = spec_for(spec.number).expect("present");
            assert_eq!(found.number, spec.number);
            assert_eq!(found.name, spec.name);
        }
        // One past the populated range.
        let past = SyscallNumber::from_raw(u16::try_from(SYSCALLS.len()).unwrap()).unwrap();
        assert!(spec_for(past).is_none());
    }

    #[test]
    fn capability_requirements_are_frozen() {
        // The cap_revoke gate is part of abi-v1; locking it down here so
        // a refactor cannot loosen the requirement.
        let revoke = spec_for(SyscallNumber::CAP_REVOKE).unwrap();
        assert_eq!(revoke.required_capability, Some(CapabilityId::USER_ADMIN));
        // The IRQ pair is gated by CAP_IRQ_BIND on both ends — there
        // is no asymmetry between bind and wait (a task that may
        // bind a line must be able to wait on it, and a task that
        // may wait on a handle must have been authorised to mint
        // it). Lock that down so a refactor cannot split the gate.
        let bind = spec_for(SyscallNumber::IRQ_BIND).unwrap();
        assert_eq!(bind.required_capability, Some(CapabilityId::IRQ_BIND));
        assert!(bind.audit, "irq_bind must be audited");
        let wait = spec_for(SyscallNumber::IRQ_WAIT).unwrap();
        assert_eq!(wait.required_capability, Some(CapabilityId::IRQ_BIND));
        // stream_write / stream_read carry no dispatcher gate: a standard
        // stream may be backed by a pipe or wired file needing no console
        // authority. The handler checks CAP_CONSOLE_WRITE /
        // CAP_CONSOLE_READ exactly when the descriptor resolves to a
        // console backing, so the hardware console stays non-ambient.
        let console = spec_for(SyscallNumber::STREAM_WRITE).unwrap();
        assert_eq!(console.required_capability, None);
        assert!(!console.audit, "console_write must not audit per call");
        let console_read = spec_for(SyscallNumber::STREAM_READ).unwrap();
        assert_eq!(console_read.required_capability, None);
        assert!(!console_read.audit, "console_read must not audit per call");
        // pipe_create mints two descriptors of the caller's OWN open
        // table — the unprivileged, unaudited baseline (the mem_map
        // precedent); the spawn that transfers an end is the audited
        // decision.
        let pipe_create = spec_for(SyscallNumber::PIPE_CREATE).unwrap();
        assert_eq!(pipe_create.required_capability, None);
        assert!(!pipe_create.audit, "pipe_create must not audit per call");
        // spawn is gated on CAP_PROC_SPAWN and audited per call — a new
        // process is a security-relevant state change.
        let spawn = spec_for(SyscallNumber::SPAWN).unwrap();
        assert_eq!(spawn.required_capability, Some(CapabilityId::PROC_SPAWN));
        assert!(spawn.audit, "spawn must be audited");
        // mem_map / mem_unmap grow and shrink the caller's OWN
        // hardware-isolated address space, so they are the unprivileged
        // baseline and are not audited per call. Lock
        // that down so a refactor cannot accidentally gate or audit them.
        let mem_map = spec_for(SyscallNumber::MEM_MAP).unwrap();
        assert_eq!(mem_map.required_capability, None);
        assert!(!mem_map.audit, "mem_map must not audit per call");
        let mem_unmap = spec_for(SyscallNumber::MEM_UNMAP).unwrap();
        assert_eq!(mem_unmap.required_capability, None);
        assert!(!mem_unmap.audit, "mem_unmap must not audit per call");
        // file_map reads a filesystem object into the caller's OWN address
        // space, so it carries the coarse filesystem gate at dispatch (the
        // fs_set_mode precedent) but is not audited per call; file_unmap
        // only shrinks the caller's own space (the mem_unmap posture).
        let file_map = spec_for(SyscallNumber::FILE_MAP).unwrap();
        assert_eq!(file_map.required_capability, Some(CapabilityId::FS_ACCESS));
        assert!(!file_map.audit, "file_map must not audit per call");
        let file_unmap = spec_for(SyscallNumber::FILE_UNMAP).unwrap();
        assert_eq!(file_unmap.required_capability, None);
        assert!(!file_unmap.audit, "file_unmap must not audit per call");
        // rlimit_get reads the caller's own effective limit, so it is the
        // unprivileged baseline and is not audited per call. rlimit_set is ungated at the dispatcher (lowering a bound
        // needs no capability; the `CAP_RLIMIT_RAISE` check is fine-grained
        // in the handler) but IS audited — it changes enforced policy.
        let rlimit_get = spec_for(SyscallNumber::RLIMIT_GET).unwrap();
        assert_eq!(rlimit_get.required_capability, None);
        assert!(!rlimit_get.audit, "rlimit_get must not audit per call");
        let rlimit_set = spec_for(SyscallNumber::RLIMIT_SET).unwrap();
        assert_eq!(rlimit_set.required_capability, None);
        assert!(rlimit_set.audit, "rlimit_set must be audited");
        // console_count reports console topology to the principals that
        // drive consoles, so it shares stream_write's CAP_CONSOLE_WRITE
        // gate and, as a pure observer, is not audited.
        let console_count = spec_for(SyscallNumber::CONSOLE_COUNT).unwrap();
        assert_eq!(
            console_count.required_capability,
            Some(CapabilityId::CONSOLE_WRITE)
        );
        assert!(!console_count.audit, "console_count must not audit");
        // stream_input_mode controls the read line discipline on the
        // console the reader holds, so it shares stream_read's
        // CAP_CONSOLE_READ gate and, as low-volume terminal configuration,
        // is not audited.
        let stream_input_mode = spec_for(SyscallNumber::STREAM_INPUT_MODE).unwrap();
        assert_eq!(
            stream_input_mode.required_capability,
            Some(CapabilityId::CONSOLE_READ)
        );
        assert!(!stream_input_mode.audit, "stream_input_mode must not audit");
        // hw_tree_read / hw_tree_wait expose the privileged *global*
        // hardware inventory and its change notifications, gated on
        // CAP_SYSINFO_HW (never the ambient
        // own-process baseline) and, as the high-volume reactive
        // device-manager path, not audited per call: the audited security
        // decision is the subsequent driver load.
        for n in [SyscallNumber::HW_TREE_READ, SyscallNumber::HW_TREE_WAIT] {
            let spec = spec_for(n).unwrap();
            assert_eq!(spec.required_capability, Some(CapabilityId::SYSINFO_HW));
            assert!(!spec.audit, "hw-tree observation must not audit per call");
        }
        // ipc_call carries no dispatcher capability gate (the call endpoint
        // enforces its own required send capability against the caller, like
        // ipc_send over a port) but IS audited per call,
        // matching ipc_send (a synchronous system-service call is
        // security-relevant IPC).
        let ipc_call = spec_for(SyscallNumber::IPC_CALL).unwrap();
        assert_eq!(ipc_call.required_capability, None);
        assert!(ipc_call.audit, "ipc_call must be audited");
        // log_emit is gated on the privileged CAP_LOG_EMIT — the system
        // console log is never ambient — and, as a
        // high-volume diagnostic channel (not the hash-chained audit log),
        // is NOT audited per call.
        let log_emit = spec_for(SyscallNumber::LOG_EMIT).unwrap();
        assert_eq!(log_emit.required_capability, Some(CapabilityId::LOG_EMIT));
        assert!(!log_emit.audit, "log_emit must not audit per call");
        // hw_emit_node publishes a discovered child into the global hardware
        // tree, gated on the privileged CAP_HW_EMIT (never ambient) and IS audited per call: admitting a node that drives
        // an autoload and carries device-resource grants is a low-volume,
        // security-relevant event.
        let hw_emit_node = spec_for(SyscallNumber::HW_EMIT_NODE).unwrap();
        assert_eq!(
            hw_emit_node.required_capability,
            Some(CapabilityId::HW_EMIT)
        );
        assert!(hw_emit_node.audit, "hw_emit_node must be audited");
        // hw_remove_node is the exact mirror of hw_emit_node: the same
        // privileged CAP_HW_EMIT gate and the same per-call audit (retiring a
        // node drives an unload, a low-volume security-relevant event).
        let hw_remove_node = spec_for(SyscallNumber::HW_REMOVE_NODE).unwrap();
        assert_eq!(
            hw_remove_node.required_capability,
            Some(CapabilityId::HW_EMIT)
        );
        assert!(hw_remove_node.audit, "hw_remove_node must be audited");
        // Pure observers must remain ungated.
        for n in [
            SyscallNumber::YIELD,
            SyscallNumber::CAP_QUERY,
            SyscallNumber::CLOCK_GET,
            SyscallNumber::IPC_RECV,
        ] {
            assert!(spec_for(n).unwrap().required_capability.is_none());
        }
    }

    #[test]
    fn input_capability_requirements_are_frozen() {
        // key_inject feeds one decoded key edge into the input-focus
        // arbiter, so it is gated on the privileged CAP_INPUT_INJECT — the
        // system keyboard stream is never ambient — and,
        // like the per-event stream operations, is not audited per call.
        // pointer_inject is its pointer analogue: the same gate and the
        // same unaudited per-event posture.
        for n in [SyscallNumber::KEY_INJECT, SyscallNumber::POINTER_INJECT] {
            let spec = spec_for(n).unwrap();
            assert_eq!(spec.required_capability, Some(CapabilityId::INPUT_INJECT));
            assert!(!spec.audit, "input injection must not audit per event");
        }
        // display_acquire / display_release own the display and keyboard
        // focus, gated on CAP_DISPLAY and audited per call — re-routing the
        // keyboard stream is a security-relevant ownership change.
        for n in [
            SyscallNumber::DISPLAY_ACQUIRE,
            SyscallNumber::DISPLAY_RELEASE,
        ] {
            let spec = spec_for(n).unwrap();
            assert_eq!(spec.required_capability, Some(CapabilityId::DISPLAY));
            assert!(spec.audit, "display ownership must be audited");
        }
        // keyboard_read drains the kernel keyboard channel for the seat
        // owner, gated on CAP_INPUT_READ (the kernel additionally
        // owner-gates the drain against the seat's live lease) and — like
        // stream_read — not audited per call. pointer_read is its pointer
        // analogue with the identical double gate.
        for n in [SyscallNumber::KEYBOARD_READ, SyscallNumber::POINTER_READ] {
            let spec = spec_for(n).unwrap();
            assert_eq!(spec.required_capability, Some(CapabilityId::INPUT_READ));
            assert!(!spec.audit, "input drains must not audit per event");
        }
    }

    #[test]
    fn fs_capability_requirements_are_frozen() {
        // The *path-taking* filesystem syscalls share the single coarse
        // CAP_FS_ACCESS entry gate (the per-path authority is the VFS inode
        // model under the caller's real credentials, not this capability).
        // The *descriptor-operating* calls (close, read, write) are ungated
        // at the dispatcher: a descriptor may be backed by a filesystem path
        // (opened under CAP_FS_ACCESS) or a resource reference (opened under
        // its namespace's own authority), so the handler applies the
        // backing-specific check rather than a blanket filesystem gate — a
        // path-backed descriptor still requires CAP_FS_ACCESS there. State-
        // mutating calls (open — which may create — write, truncate, mkdir,
        // unlink, rename) are audited; the pure reads (read, readdir, stat)
        // and the own-handle lifecycle calls (close, sync) are high-volume
        // and not audited per call. Lock this down so a refactor cannot
        // loosen a path gate or drop the audit on a mutator.
        for n in [
            SyscallNumber::FS_OPEN,
            SyscallNumber::FS_READDIR,
            SyscallNumber::FS_STAT,
            SyscallNumber::FS_TRUNCATE,
            SyscallNumber::FS_SYNC,
            SyscallNumber::FS_MKDIR,
            SyscallNumber::FS_UNLINK,
            SyscallNumber::FS_RENAME,
            SyscallNumber::FS_SET_MODE,
            SyscallNumber::FS_ATTR_GET,
            SyscallNumber::FS_ATTR_SET,
            SyscallNumber::FS_ATTR_LIST,
            SyscallNumber::FS_ATTR_REMOVE,
        ] {
            assert_eq!(
                spec_for(n).unwrap().required_capability,
                Some(CapabilityId::FS_ACCESS),
                "{} must be gated on CAP_FS_ACCESS",
                spec_for(n).unwrap().name
            );
        }
        // The descriptor-operating calls carry no blanket dispatcher gate;
        // the handler enforces the backing-specific authority (a path-backed
        // descriptor still requires CAP_FS_ACCESS). Lock that down so a
        // refactor cannot silently re-impose or drop the coarse gate.
        for n in [
            SyscallNumber::FS_CLOSE,
            SyscallNumber::FS_READ,
            SyscallNumber::FS_WRITE,
        ] {
            assert_eq!(
                spec_for(n).unwrap().required_capability,
                None,
                "{} must be ungated at the dispatcher (backing-specific check in handler)",
                spec_for(n).unwrap().name
            );
        }
        for n in [
            SyscallNumber::FS_OPEN,
            SyscallNumber::FS_WRITE,
            SyscallNumber::FS_TRUNCATE,
            SyscallNumber::FS_MKDIR,
            SyscallNumber::FS_UNLINK,
            SyscallNumber::FS_RENAME,
            SyscallNumber::FS_SET_MODE,
            SyscallNumber::FS_ATTR_SET,
            SyscallNumber::FS_ATTR_REMOVE,
        ] {
            assert!(
                spec_for(n).unwrap().audit,
                "{} must be audited",
                spec_for(n).unwrap().name
            );
        }
        for n in [
            SyscallNumber::FS_CLOSE,
            SyscallNumber::FS_READ,
            SyscallNumber::FS_READDIR,
            SyscallNumber::FS_STAT,
            SyscallNumber::FS_SYNC,
            SyscallNumber::FS_ATTR_GET,
            SyscallNumber::FS_ATTR_LIST,
        ] {
            assert!(
                !spec_for(n).unwrap().audit,
                "{} must not audit per call",
                spec_for(n).unwrap().name
            );
        }
    }

    #[test]
    fn resource_open_capability_requirements_are_frozen() {
        // resource_open carries no blanket dispatcher gate: authorisation is
        // per namespace and selector inside the resolver, so an unprivileged
        // resource (sys:random, sys:null) needs none and a privileged
        // namespace is checked in the handler and fails closed. It IS audited
        // per call — resolving a resource to a descriptor is a
        // security-relevant decision, like fs_open. Lock this down so a
        // refactor cannot impose a coarse gate or drop the audit.
        let spec = spec_for(SyscallNumber::RESOURCE_OPEN).unwrap();
        assert_eq!(spec.required_capability, None);
        assert!(spec.audit, "resource_open must be audited per call");
        assert_eq!(spec.name, "resource_open");
        assert_eq!(spec.arg_count, 3);
    }

    #[test]
    fn wall_time_capability_requirements_are_frozen() {
        // wall_time_get is a pure, unprivileged observer (like clock_get):
        // any task may read the wall clock, and it is not audited per call.
        let get = spec_for(SyscallNumber::WALL_TIME_GET).unwrap();
        assert_eq!(get.required_capability, None);
        assert!(!get.audit, "wall_time_get must not audit per call");
        // wall_time_set drives the system clock, so it is gated on
        // CAP_TIME_SET and audited per call.
        let set = spec_for(SyscallNumber::WALL_TIME_SET).unwrap();
        assert_eq!(set.required_capability, Some(CapabilityId::TIME_SET));
        assert!(set.audit, "wall_time_set must be audited");
    }

    #[test]
    fn boot_id_get_capability_requirements_are_frozen() {
        // The boot id is a public per-boot nonce, not a secret, so reading it
        // is a pure, unprivileged observer (like clock_get / wall_time_get)
        // and is not audited per call. Lock this down so a refactor cannot
        // gate or audit it.
        let get = spec_for(SyscallNumber::BOOT_ID_GET).unwrap();
        assert_eq!(get.required_capability, None);
        assert!(!get.audit, "boot_id_get must not audit per call");
    }

    #[test]
    fn sysinfo_introspect_capability_requirements_are_frozen() {
        // The unfiltered global system view is privileged and held only by
        // the sysinfod broker; it is gated on CAP_SYSINFO_INTROSPECT and, like
        // hw_tree_read, is not audited per call (the broker records the
        // client-facing query). Lock this down so a refactor cannot loosen
        // the gate or start auditing the high-volume observation.
        let spec = spec_for(SyscallNumber::SYSINFO_INTROSPECT).unwrap();
        assert_eq!(
            spec.required_capability,
            Some(CapabilityId::SYSINFO_INTROSPECT)
        );
        assert!(!spec.audit, "sysinfo_introspect must not audit per call");
        assert_eq!(spec.name, "sysinfo_introspect");
    }

    #[test]
    fn mem_pin_capability_requirements_are_frozen() {
        // Pinning exempts memory from pressure management system-wide, so
        // the pin carries the dedicated capability; the unpin only narrows
        // the caller's own footprint and is ungated. Both edges are
        // audited. Lock this down so a refactor cannot loosen the gate or
        // drop either audit record.
        let pin = spec_for(SyscallNumber::MEM_PIN).unwrap();
        assert_eq!(pin.required_capability, Some(CapabilityId::MEM_PIN));
        assert!(pin.audit, "mem_pin must be audited");
        assert_eq!(pin.name, "mem_pin");
        assert_eq!(pin.arg_count, 0);
        let unpin = spec_for(SyscallNumber::MEM_UNPIN).unwrap();
        assert_eq!(unpin.required_capability, None);
        assert!(unpin.audit, "mem_unpin must be audited");
        assert_eq!(unpin.name, "mem_unpin");
        assert_eq!(unpin.arg_count, 0);
    }

    #[test]
    fn signal_intake_capability_requirements_are_frozen() {
        // Own-process signal disposition grants no authority over any other
        // principal, so the call is ungated; it is audited per call like
        // `signal` itself so the trail carries the opt-in, the opt-out, and
        // each observed delivery's drain. Lock this down so a refactor
        // cannot gate the unprivileged tier or drop the audit.
        let spec = spec_for(SyscallNumber::SIGNAL_INTAKE).unwrap();
        assert_eq!(spec.required_capability, None);
        assert!(spec.audit, "signal_intake must be audited");
        assert_eq!(spec.name, "signal_intake");
        assert_eq!(spec.arg_count, 1);
    }

    #[test]
    fn volume_capability_requirements_are_frozen() {
        // Attaching and detaching runtime volumes reshapes the storage
        // namespace for every principal: both carry the mount authority
        // and both are audited per call. Lock this down so a refactor
        // cannot loosen the gate or drop the audit.
        for number in [SyscallNumber::VOLUME_ATTACH, SyscallNumber::VOLUME_DETACH] {
            let spec = spec_for(number).unwrap();
            assert_eq!(spec.required_capability, Some(CapabilityId::FS_MOUNT));
            assert!(spec.audit, "{} must be audited", spec.name);
        }
    }

    #[test]
    fn encoded_table_has_expected_length() {
        assert_eq!(
            ENCODED_TABLE_LEN,
            SYSCALL_ENCODED_RECORD_LEN * SYSCALLS.len()
        );
        assert_eq!(encoded_table().len(), ENCODED_TABLE_LEN);
    }

    #[test]
    fn encoded_table_first_record_is_yield() {
        // number == 0, arg_count == 0, ret == Unit, all args Unit,
        // required cap absent, no audit, name "yield" + 7 NUL padding.
        let rec = &ENCODED_TABLE[..SYSCALL_ENCODED_RECORD_LEN];
        assert_eq!(&rec[0..2], &[0, 0]);
        assert_eq!(rec[2], 0);
        assert_eq!(rec[3], AbiType::Unit.as_u8());
        for slot in &rec[4..10] {
            assert_eq!(*slot, AbiType::Unit.as_u8());
        }
        assert_eq!(rec[10], 0); // required-cap absent
        assert_eq!(&rec[11..13], &[0, 0]);
        assert_eq!(rec[13], 0); // audit off
        assert_eq!(&rec[14..19], b"yield");
        for pad in &rec[19..SYSCALL_ENCODED_RECORD_LEN] {
            assert_eq!(*pad, 0);
        }
    }

    #[test]
    fn encoded_table_cap_revoke_records_required_capability() {
        let idx = SyscallNumber::CAP_REVOKE.as_u16() as usize;
        let base = idx * SYSCALL_ENCODED_RECORD_LEN;
        let rec = &ENCODED_TABLE[base..base + SYSCALL_ENCODED_RECORD_LEN];
        assert_eq!(rec[10], 1, "required-capability flag");
        let cap_le = u16::from_le_bytes([rec[11], rec[12]]);
        assert_eq!(cap_le, CapabilityId::USER_ADMIN.as_u16());
        assert_eq!(rec[13], 1, "audit flag");
    }
}
