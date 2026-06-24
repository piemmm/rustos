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
/// the longest `abi-v1` name (`display_acquire` / `display_release`, 15
/// bytes).
pub const SYSCALL_NAME_MAX: usize = 15;

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
        // the spawner established — never an ambient device. In this
        // bootstrap phase every backing is the discovered console, so the
        // coarse `CAP_CONSOLE_WRITE` still gates use of a console-backed
        // output stream; the descriptor table is the fine, fd-level gate.
        // Like the other high-volume data movers (`ipc_recv`,
        // `random_get`) it is not audited per call, to avoid drowning the
        // audit log.
        required_capability: Some(CapabilityId::CONSOLE_WRITE),
        audit: false,
    },
    SyscallSpec {
        number: SyscallNumber::SPAWN,
        name: "spawn",
        arg_count: 3,
        args: [
            AbiType::UserPtr,
            AbiType::Len,
            // The console selector: `CONSOLE_INHERIT` (the all-ones
            // sentinel) attaches the child to the caller's own
            // descriptor table, any other value names an installed
            // console index (the spawner decides the
            // child's stream backing). `U64` so the sentinel is
            // representable; the handler validates the range.
            AbiType::U64,
            AbiType::Unit,
            AbiType::Unit,
            AbiType::Unit,
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
        // Reading one of the calling process's inherited standard streams routes to that descriptor's kernel stream
        // backing. Authority is the per-process descriptor table the
        // spawner established — never an ambient device. In this
        // bootstrap phase every backing is the discovered console, so the
        // coarse `CAP_CONSOLE_READ` still gates use of a console-backed
        // input stream; the descriptor table is the fine, fd-level gate.
        // Like the other high-volume data movers (`stream_write`,
        // `ipc_recv`, `random_get`) it is not audited per call, to avoid
        // drowning the audit log.
        required_capability: Some(CapabilityId::CONSOLE_READ),
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
        arg_count: 2,
        args: [
            AbiType::I32,
            AbiType::UserPtr,
            AbiType::Unit,
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
        // audited; `wait` blocks rather than polls, so
        // the per-call record does not drown the log.
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
        number: SyscallNumber::STREAM_ECHO,
        name: "stream_echo",
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
        // Terminal echo is a property of the console the reader holds, so
        // the control shares `stream_read`'s `CAP_CONSOLE_READ` gate —
        // never ambient. The kernel performs the echo
        // itself as part of the read line discipline, so toggling it
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
        arg_count: 2,
        args: [
            AbiType::UserPtr,
            AbiType::Len,
            AbiType::Unit,
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
        arg_count: 0,
        args: [AbiType::Unit; SYSCALL_MAX_ARGS],
        ret: AbiType::Errno,
        // Owning the display (and, with it, keyboard input focus) is
        // privileged, never ambient: only a session's
        // window manager holds `CAP_DISPLAY`. Taking the screen and
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
        arg_count: 0,
        args: [AbiType::Unit; SYSCALL_MAX_ARGS],
        ret: AbiType::Errno,
        // The release half of `display_acquire`; same `CAP_DISPLAY` gate
        // and same audited posture — returning focus to the text console
        // is the matching security-relevant ownership change.
        required_capability: Some(CapabilityId::DISPLAY),
        audit: true,
    },
    SyscallSpec {
        number: SyscallNumber::KEYBOARD_READ,
        name: "keyboard_read",
        arg_count: 2,
        args: [
            AbiType::UserPtr,
            AbiType::Len,
            AbiType::Unit,
            AbiType::Unit,
            AbiType::Unit,
            AbiType::Unit,
        ],
        // `U64` so the C view carries the bytes-read-or-`-errno` register
        // convention `stream_read` uses.
        ret: AbiType::U64,
        // Reading the keyboard channel is privileged, never ambient: only the display owner (the window manager)
        // holds `CAP_INPUT_READ`, so the keyboard stream is delivered only
        // to whoever owns the surface. Like the other
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
        arg_count: 4,
        args: [
            // endpoint id, request buffer ptr, request buffer cap,
            // ticket-out ptr.
            AbiType::IpcEndpoint,
            AbiType::UserPtr,
            AbiType::Len,
            AbiType::UserPtr,
            AbiType::Unit,
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
        // stream_write is gated on CAP_CONSOLE_WRITE — the privileged
        // hardware console is never ambient.
        let console = spec_for(SyscallNumber::STREAM_WRITE).unwrap();
        assert_eq!(
            console.required_capability,
            Some(CapabilityId::CONSOLE_WRITE)
        );
        assert!(!console.audit, "console_write must not audit per call");
        // stream_read is gated on CAP_CONSOLE_READ — the privileged
        // hardware console input is never ambient.
        let console_read = spec_for(SyscallNumber::STREAM_READ).unwrap();
        assert_eq!(
            console_read.required_capability,
            Some(CapabilityId::CONSOLE_READ)
        );
        assert!(!console_read.audit, "console_read must not audit per call");
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
        // stream_echo controls terminal echo on the console the reader
        // holds, so it shares stream_read's CAP_CONSOLE_READ gate and, as
        // low-volume terminal configuration, is not audited.
        let stream_echo = spec_for(SyscallNumber::STREAM_ECHO).unwrap();
        assert_eq!(
            stream_echo.required_capability,
            Some(CapabilityId::CONSOLE_READ)
        );
        assert!(!stream_echo.audit, "stream_echo must not audit");
        // key_inject feeds one decoded key edge into the input-focus
        // arbiter, so it is gated on the privileged CAP_INPUT_INJECT — the
        // system keyboard stream is never ambient — and,
        // like the per-event stream operations, is not audited per call.
        let key_inject = spec_for(SyscallNumber::KEY_INJECT).unwrap();
        assert_eq!(
            key_inject.required_capability,
            Some(CapabilityId::INPUT_INJECT)
        );
        assert!(!key_inject.audit, "key_inject must not audit");
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
        // keyboard_read drains the kernel keyboard channel for the display
        // owner, gated on CAP_INPUT_READ and — like stream_read — not
        // audited per call.
        let keyboard_read = spec_for(SyscallNumber::KEYBOARD_READ).unwrap();
        assert_eq!(
            keyboard_read.required_capability,
            Some(CapabilityId::INPUT_READ)
        );
        assert!(!keyboard_read.audit, "keyboard_read must not audit");
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
