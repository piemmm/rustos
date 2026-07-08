//! System Information API (`sysinfo`) — the ABI surface.
//!
//! RustOS has no `/proc` and no `/sys`. Every piece of live system
//! information that would have lived under those trees is exposed through
//! this single, versioned, capability-checked API. Each query is a *typed*
//! request returning a *typed* response — there is no free-form text
//! scraping interface — and every query declares the capability a caller
//! must hold to invoke it.
//!
//! Adding a query carries the same discipline as adding a syscall: the registry is versioned, its canonical encoding is
//! hashed, and it is frozen on release. Existing [`SysinfoQueryId`] numbers
//! and [`SysinfoQuerySpec`] rows must never be re-numbered or removed; new
//! queries take the next free identifier and ship in `sysinfo-v2`.
//!
//! This module defines only the wire types and the frozen registry. The
//! user-space service that answers the queries lives at
//! `/System/Services/sysinfod.app/Run` (`userland/system/sysinfod`); the kernel has
//! no privileged path that bypasses the capability check.

use crate::driver::filesystem::{MountFlags, VolumeStats};
use crate::le::{put_u16, put_u32, put_u64, read_u16, read_u32, read_u64};
use crate::origin::ProcId;
use crate::rlimit::{LimitKind, ResourceLimit};
use crate::time::{Duration64, Time64};
use crate::{CapabilityId, Errno};

/// Version tag for the frozen `sysinfo-v1` request/response surface.
///
/// Carried in every [`SysinfoRequestHeader`]; a service receiving a header
/// whose version it does not understand refuses the request rather than
/// guessing at the layout.
pub const SYSINFO_VERSION_V1: u16 = 1;

/// The current `sysinfo` version served by this ABI revision.
///
/// Equal to [`SYSINFO_VERSION_V1`] today; when `sysinfo-v2` is introduced
/// this constant is re-pointed and `sysinfo-v1` moves to a compatibility
/// path rather than mutating these types in place.
pub const SYSINFO_VERSION_CURRENT: u16 = SYSINFO_VERSION_V1;

/// Stable identifier for one System Information query.
///
/// Wraps a `u16` so it cannot be confused with raw integer arguments at
/// call sites. Identifiers are dense; the registry [`SYSINFO_QUERIES`] is
/// indexed directly by this value.
#[repr(transparent)]
#[derive(Copy, Clone, Debug, Eq, PartialEq, Hash, Ord, PartialOrd)]
pub struct SysinfoQueryId(u16);

impl SysinfoQueryId {
    /// List the calling principal's own processes. Requires no capability.
    pub const SELF_PROCESS_LIST: Self = Self(0);
    /// List every process on the system. Requires `CAP_SYSINFO_GLOBAL`.
    pub const GLOBAL_PROCESS_LIST: Self = Self(1);
    /// Read kernel memory statistics. Requires `CAP_SYSINFO_KERNEL`.
    pub const KERNEL_MEMORY_STATS: Self = Self(2);
    /// Read the detected hardware tree. Requires `CAP_SYSINFO_HW`.
    pub const HARDWARE_TREE: Self = Self(3);
    /// Read machine identity (machine ID, OS version). Requires none.
    pub const SYSTEM_IDENTITY: Self = Self(4);
    /// Read system uptime and boot wall-clock time. Requires none.
    pub const UPTIME: Self = Self(5);
    /// List the currently-mounted filesystems. Requires no capability:
    /// the mount table is system-wide rather than scoped to a principal,
    /// and exposes no per-process secret, so — like [`Self::UPTIME`] and
    /// [`Self::SYSTEM_IDENTITY`] — any task may read it. The privileged *act* of mounting is gated separately by
    /// `CAP_FS_MOUNT`; this query only reports.
    pub const MOUNT_LIST: Self = Self(6);
    /// Read the calling principal's own effective resource limits together
    /// with its current live usage of each.
    ///
    /// Requires no capability: the answer is scoped to the caller's own
    /// task, exposing no other principal's state — like
    /// [`Self::SELF_PROCESS_LIST`]. Observing *another* principal's limits
    /// would be a separate, capability-gated query.
    pub const RESOURCE_LIMITS: Self = Self(7);
    /// Read the calling principal's own kernel-attested [`Origin`](crate::Origin).
    ///
    /// Self-scoped and ungated: the answer describes only the caller — its
    /// trust domain, uid, pid, unforgeable [`ProcId`], and a
    /// non-secret capability summary — all filled by the kernel from the
    /// caller's own task state, never from the request payload. A principal
    /// observing *its own* attested identity exposes no other principal's
    /// state, so — like [`Self::SELF_PROCESS_LIST`] and
    /// [`Self::RESOURCE_LIMITS`] — it carries no capability gate. Reading
    /// *another* principal's origin would be a separate, capability-gated
    /// query.
    pub const PROCESS_IDENTITY: Self = Self(8);

    /// Read the scheduler load averages and the logged-in-user census.
    ///
    /// Ungated: the answer is the classic `uptime(1)` line — three
    /// exponentially-damped load averages, the runnable/total task counts,
    /// and the number of distinct logged-in users — system-wide figures
    /// that expose no per-process secret, exactly like [`Self::UPTIME`].
    pub const LOAD_AVERAGE: Self = Self(9);

    /// List the account directory: every account's uid and username, one
    /// [`UserDirectoryRecord`] per account.
    ///
    /// Ungated: the pairing of a numeric uid with its account name is the
    /// `/etc/passwd`-class public directory every `ls -l`- or `top`-style
    /// display needs, and it carries **no** credential material — password
    /// records stay behind the capability-gated `users_db_read` syscall.
    /// A system whose user database is not loaded answers with an empty
    /// directory, never a fabricated account.
    pub const USER_DIRECTORY: Self = Self(10);

    /// List per-CPU execution-time accounting: one [`CpuTimeRecord`] per
    /// online CPU, paged by a [`CpuTimeListRequest`].
    ///
    /// Ungated: the aggregate busy/idle split is the `top`/`uptime`-class
    /// utilisation figure every user may see, and it exposes strictly less
    /// than the ungated [`SysinfoQueryId::LOAD_AVERAGE`] census — no
    /// per-task, per-user, or kernel-internal detail crosses it.
    pub const CPU_TIME_STATS: Self = Self(11);

    /// List the kernel's seats: one [`SeatRecord`] per seat (seat id, live
    /// owner, lease generation, foreground console), paged by a
    /// [`SeatListRequest`].
    ///
    /// Requires `CAP_SYSINFO_HW` and is audited: like
    /// [`Self::HARDWARE_TREE`], the seat inventory names which task owns
    /// each physical display — cross-principal, security-relevant surface
    /// topology, not a self-scoped observer (`plans/DISPLAY.md` D3).
    pub const SEAT_LIST: Self = Self(12);

    /// Inclusive upper bound on the query identifier space in `sysinfo-v1`.
    ///
    /// Sized identically to the syscall table so a future query explosion
    /// is accommodated without an ABI break.
    pub const MAX: u16 = 1023;

    /// Wrap a raw value, validating that it falls inside the identifier
    /// space.
    ///
    /// Returns [`Errno::OutOfRange`] if `raw` exceeds [`SysinfoQueryId::MAX`].
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

/// The slice of the **unfiltered, global** system view a
/// [`sysinfo_introspect`](crate::SyscallNumber::SYSINFO_INTROSPECT) syscall
/// selects.
///
/// This is the *kernel primitive's* vocabulary, distinct from the userland
/// [`SysinfoQueryId`] registry: the kernel answers each domain with the whole
/// system's state and never narrows by principal — the `sysinfod` broker maps
/// its clients' [`SysinfoQueryId`] queries onto these domains and enforces all
/// per-client scoping. The set is closed and fail-closed: an unknown
/// discriminant is rejected rather than guessed.
#[repr(u32)]
#[derive(Copy, Clone, Debug, Eq, PartialEq)]
pub enum IntrospectDomain {
    /// The live process table: every task, one packed [`ProcessRecord`], with
    /// the syscall's `arg` naming the record offset to page from.
    Processes = 0,
    /// Kernel memory accounting: a single [`KernelMemoryStats`].
    KernelMemory = 1,
    /// The mount table: every mount, one packed [`MountRecord`], with the
    /// syscall's `arg` naming the record offset to page from.
    Mounts = 2,
    /// Machine identity: a single [`SystemIdentity`].
    Identity = 3,
    /// System uptime and boot wall-clock time: a single [`Uptime`].
    Uptime = 4,
    /// One task's effective resource limits and live usage: the
    /// [`RESOURCE_LIMITS_REPORT_LEN`]-byte positional
    /// `[ResourceLimitRecord; LimitKind::COUNT]` array. The target task is
    /// named by the 128-bit [`ProcId`] the caller writes into the output
    /// buffer on entry (a `u64` `arg` cannot carry it), which the kernel
    /// resolves against the capability table so the answer survives PID reuse.
    TaskLimits = 5,
    /// Scheduler load averages and the logged-in-user census: a single
    /// [`LoadAverage`].
    LoadAverage = 6,
    /// The account directory: every account, one packed
    /// [`UserDirectoryRecord`] (uid + username, no credential material),
    /// with the syscall's `arg` naming the record offset to page from.
    UserDirectory = 7,
    /// Per-CPU execution-time accounting: every online CPU, one packed
    /// [`CpuTimeRecord`], with the syscall's `arg` naming the record offset
    /// to page from.
    CpuTimes = 8,
    /// The seat registry: every seat, one packed [`SeatRecord`], with the
    /// syscall's `arg` naming the record offset to page from.
    Seats = 9,
}

impl IntrospectDomain {
    /// Raw on-wire discriminant.
    #[must_use]
    pub const fn as_u32(self) -> u32 {
        self as u32
    }

    /// Decode a raw discriminant, failing closed on an unknown value.
    ///
    /// Returns [`Errno::OutOfRange`] for any value outside the closed set.
    pub const fn from_u32(raw: u32) -> Result<Self, Errno> {
        match raw {
            0 => Ok(Self::Processes),
            1 => Ok(Self::KernelMemory),
            2 => Ok(Self::Mounts),
            3 => Ok(Self::Identity),
            4 => Ok(Self::Uptime),
            5 => Ok(Self::TaskLimits),
            6 => Ok(Self::LoadAverage),
            7 => Ok(Self::UserDirectory),
            8 => Ok(Self::CpuTimes),
            9 => Ok(Self::Seats),
            _ => Err(Errno::OutOfRange),
        }
    }
}

/// Maximum length, in bytes, of the ASCII `name` of any [`SysinfoQuerySpec`].
///
/// Pinned so that [`ENCODED_QUERY_TABLE`] uses a fixed stride per record and
/// the encoding is computable in a `const fn` without an allocator.
pub const SYSINFO_QUERY_NAME_MAX: usize = 20;

/// Stride, in bytes, of one record inside [`ENCODED_QUERY_TABLE`].
///
/// Layout per record, in [`SysinfoQueryId`] ascending order:
///
/// | Offset | Size | Field |
/// |-------:|-----:|-------|
/// |   0    |  2   | `id` as little-endian `u16` |
/// |   2    |  1   | `required_capability.is_some()` (`0` or `1`) |
/// |   3    |  2   | `required_capability` as little-endian `u16` (`0` when absent) |
/// |   5    |  1   | `audit` (`0` or `1`) |
/// |   6    | 20   | `name`, ASCII, right-padded with `0x00` to 20 bytes |
pub const SYSINFO_QUERY_RECORD_LEN: usize = 6 + SYSINFO_QUERY_NAME_MAX;

/// One row of the frozen `sysinfo-v1` query registry.
///
/// Fields are public and `const`-constructible so the registry can be a
/// `&'static [SysinfoQuerySpec]`. Existing rows must never change; see the
/// module-level frozen-ABI note.
#[derive(Copy, Clone, Debug)]
pub struct SysinfoQuerySpec {
    /// Stable identifier.
    pub id: SysinfoQueryId,
    /// ASCII name. `len <= SYSINFO_QUERY_NAME_MAX`.
    pub name: &'static str,
    /// Capability required to invoke this query, if any.
    ///
    /// `None` means any task may issue the query (its answer is scoped to
    /// the caller's own principal); `Some(cap)` means the serving service
    /// refuses with [`Errno::PermissionDenied`] unless the caller's
    /// effective set contains `cap`.
    pub required_capability: Option<CapabilityId>,
    /// Whether the service must emit an audit record for every invocation.
    ///
    /// Privileged, cross-principal queries are audited; self-scoped
    /// observers are not, to avoid drowning the audit log.
    pub audit: bool,
}

/// The frozen `sysinfo-v1` query registry.
///
/// Indexed by [`SysinfoQueryId::as_u16`]; every entry's array index equals
/// its `id` field (verified by `registry_is_dense_and_ordered`).
pub const SYSINFO_QUERIES: &[SysinfoQuerySpec] = &[
    SysinfoQuerySpec {
        id: SysinfoQueryId::SELF_PROCESS_LIST,
        name: "self_process_list",
        required_capability: None,
        audit: false,
    },
    SysinfoQuerySpec {
        id: SysinfoQueryId::GLOBAL_PROCESS_LIST,
        name: "global_process_list",
        required_capability: Some(CapabilityId::SYSINFO_GLOBAL),
        audit: true,
    },
    SysinfoQuerySpec {
        id: SysinfoQueryId::KERNEL_MEMORY_STATS,
        name: "kernel_memory_stats",
        required_capability: Some(CapabilityId::SYSINFO_KERNEL),
        audit: true,
    },
    SysinfoQuerySpec {
        id: SysinfoQueryId::HARDWARE_TREE,
        name: "hardware_tree",
        required_capability: Some(CapabilityId::SYSINFO_HW),
        audit: true,
    },
    SysinfoQuerySpec {
        id: SysinfoQueryId::SYSTEM_IDENTITY,
        name: "system_identity",
        required_capability: None,
        audit: false,
    },
    SysinfoQuerySpec {
        id: SysinfoQueryId::UPTIME,
        name: "uptime",
        required_capability: None,
        audit: false,
    },
    SysinfoQuerySpec {
        id: SysinfoQueryId::MOUNT_LIST,
        name: "mount_list",
        required_capability: None,
        audit: false,
    },
    SysinfoQuerySpec {
        id: SysinfoQueryId::RESOURCE_LIMITS,
        name: "resource_limits",
        required_capability: None,
        audit: false,
    },
    SysinfoQuerySpec {
        id: SysinfoQueryId::PROCESS_IDENTITY,
        name: "process_identity",
        required_capability: None,
        audit: false,
    },
    SysinfoQuerySpec {
        id: SysinfoQueryId::LOAD_AVERAGE,
        name: "load_average",
        required_capability: None,
        audit: false,
    },
    SysinfoQuerySpec {
        id: SysinfoQueryId::USER_DIRECTORY,
        name: "user_directory",
        required_capability: None,
        audit: false,
    },
    SysinfoQuerySpec {
        id: SysinfoQueryId::CPU_TIME_STATS,
        name: "cpu_time_stats",
        required_capability: None,
        audit: false,
    },
    SysinfoQuerySpec {
        id: SysinfoQueryId::SEAT_LIST,
        name: "seat_list",
        required_capability: Some(CapabilityId::SYSINFO_HW),
        audit: true,
    },
];

/// Length, in bytes, of the canonical encoding in [`ENCODED_QUERY_TABLE`].
///
/// Derived from the registry length so adding a query updates it
/// automatically; the encoder asserts every name fits the fixed stride.
pub const ENCODED_QUERY_TABLE_LEN: usize = SYSINFO_QUERY_RECORD_LEN * SYSINFO_QUERIES.len();

/// Canonical byte representation of [`SYSINFO_QUERIES`].
///
/// Computed in a `const fn` so the encoding is fully determined at compile
/// time. It is the hashable image that pins the `sysinfo-v1` registry: a
/// service and a client built against different registries produce
/// different digests over this buffer. See the [`SYSINFO_QUERY_RECORD_LEN`]
/// layout table.
pub const ENCODED_QUERY_TABLE: [u8; ENCODED_QUERY_TABLE_LEN] = encode_query_table();

const fn encode_query_table() -> [u8; ENCODED_QUERY_TABLE_LEN] {
    let mut out = [0u8; ENCODED_QUERY_TABLE_LEN];
    let mut i = 0;
    while i < SYSINFO_QUERIES.len() {
        let spec = &SYSINFO_QUERIES[i];
        let base = i * SYSINFO_QUERY_RECORD_LEN;
        let [id_lo, id_hi] = spec.id.as_u16().to_le_bytes();
        out[base] = id_lo;
        out[base + 1] = id_hi;
        let (present, cap_id) = match spec.required_capability {
            Some(c) => (1u8, c.as_u16()),
            None => (0u8, 0u16),
        };
        out[base + 2] = present;
        let [c_lo, c_hi] = cap_id.to_le_bytes();
        out[base + 3] = c_lo;
        out[base + 4] = c_hi;
        out[base + 5] = spec.audit as u8;
        let name = spec.name.as_bytes();
        // Reject overlong names at compile time rather than truncate.
        assert!(
            name.len() <= SYSINFO_QUERY_NAME_MAX,
            "sysinfo query name exceeds SYSINFO_QUERY_NAME_MAX"
        );
        let mut n = 0;
        while n < name.len() {
            out[base + 6 + n] = name[n];
            n += 1;
        }
        i += 1;
    }
    out
}

/// Look up the [`SysinfoQuerySpec`] for a given identifier.
///
/// Returns `None` if `id` is not assigned in `sysinfo-v1`.
#[must_use]
pub const fn spec_for(id: SysinfoQueryId) -> Option<&'static SysinfoQuerySpec> {
    let raw = id.as_u16() as usize;
    if raw < SYSINFO_QUERIES.len() {
        let spec = &SYSINFO_QUERIES[raw];
        if spec.id.as_u16() as usize == raw {
            return Some(spec);
        }
    }
    None
}

/// Borrow the canonical registry encoding as a byte slice.
#[must_use]
pub const fn encoded_query_table() -> &'static [u8] {
    &ENCODED_QUERY_TABLE
}

/// Magic word identifying a `sysinfo-v1` request (`"SYI1"` little-endian).
pub const SYSINFO_REQUEST_MAGIC: u32 = u32::from_le_bytes(*b"SYI1");

/// Maximum request/response payload length (bytes) advertised by a
/// [`SysinfoRequestHeader`].
///
/// Bounded so a caller cannot trick a service into expecting a payload it
/// cannot represent; far larger than any sensible query argument block.
pub const SYSINFO_MAX_PAYLOAD_LEN: u32 = 1 << 20;

/// Well-known synchronous call-endpoint id the `sysinfod` service binds and
/// clients name in [`crate::SyscallNumber::IPC_CALL`].
///
/// One OS-wide contract, like [`crate::driver_store::DRIVER_STORE_ENDPOINT`]:
/// `sysinfod` publishes this endpoint at startup (an unrestricted-sender
/// endpoint — any process may query, and per-query scoping is enforced by
/// `sysinfod` against each caller's attested origin), and every `sysinfo`
/// client (`lib/procinfo`, the `sysinfo` CLI, `ps`, `top`) posts its
/// framed request here.
pub const SYSINFO_ENDPOINT: u64 = 0x5953_1001;

/// Maximum request payload, in bytes, the [`SYSINFO_ENDPOINT`] accepts: a
/// [`SysinfoRequestHeader`] plus the largest typed argument block any query
/// carries.
///
/// One OS-wide contract shared by the `sysinfod` server (which sizes its
/// endpoint's per-call request capacity by it) and every client (which frames
/// its request within it), so neither carries a private copy that could drift
/// from the other.
pub const SYSINFO_MAX_REQUEST: usize = 64;

/// Maximum reply, in bytes, the [`SYSINFO_ENDPOINT`] delivers: the framed
/// status word ([`SYSINFO_REPLY_STATUS_LEN`]) plus one page of records.
///
/// A client sizes its reply buffer by this bound so a served answer always
/// fits; the server pages a larger list across successive requests (a query
/// that would exceed it fails closed with [`Errno::BufferTooSmall`] so the
/// client shrinks its `limit`/advances its `offset`). One OS-wide contract
/// shared by the server and every client, so neither keeps a private copy.
pub const SYSINFO_MAX_REPLY: usize = 8192;

/// Length, in bytes, of the status word every `sysinfo` reply is prefixed
/// with (see [`encode_reply_ok`] / [`decode_reply`]).
pub const SYSINFO_REPLY_STATUS_LEN: usize = 4;

// The endpoint's message bounds are self-consistent: a framed reply leaves
// room past its status word for a payload, the request bound holds a full
// request header, and both stay within the header's advertised payload
// ceiling — the one contract the `sysinfod` server and every client size
// buffers by. Checked at compile time so a future edit that breaks it fails
// the build rather than a running system.
const _: () = assert!(SYSINFO_MAX_REPLY > SYSINFO_REPLY_STATUS_LEN);
const _: () = assert!(SYSINFO_MAX_REQUEST >= SysinfoRequestHeader::WIRE_LEN);
const _: () = assert!(SYSINFO_MAX_REPLY <= SYSINFO_MAX_PAYLOAD_LEN as usize);

/// Frame a **successful** `sysinfo` reply: a zero status word followed by
/// `payload`, written into `out`.
///
/// The server (`sysinfod`) frames every reply this way so a client can tell a
/// served answer from a per-query refusal (e.g. a missing
/// `CAP_SYSINFO_GLOBAL`), which the synchronous call transport itself cannot
/// convey — `call_reply` always succeeds at the transport level.
///
/// # Errors
///
/// [`Errno::BufferTooSmall`] if `out` cannot hold the status word plus
/// `payload`.
pub fn encode_reply_ok(payload: &[u8], out: &mut [u8]) -> Result<usize, Errno> {
    let total = SYSINFO_REPLY_STATUS_LEN
        .checked_add(payload.len())
        .ok_or(Errno::LengthOutOfRange)?;
    if out.len() < total {
        return Err(Errno::BufferTooSmall);
    }
    put_u32(out, 0, 0);
    out[SYSINFO_REPLY_STATUS_LEN..total].copy_from_slice(payload);
    Ok(total)
}

/// Frame an **error** `sysinfo` reply: the non-zero [`Errno`] code as the
/// status word, no payload, written into `out`.
///
/// # Errors
///
/// [`Errno::BufferTooSmall`] if `out` is shorter than
/// [`SYSINFO_REPLY_STATUS_LEN`].
pub fn encode_reply_err(err: Errno, out: &mut [u8]) -> Result<usize, Errno> {
    if out.len() < SYSINFO_REPLY_STATUS_LEN {
        return Err(Errno::BufferTooSmall);
    }
    // Every `Errno` code is a positive `i32`, so it round-trips through the
    // unsigned status word; `0` is reserved for success and is never an
    // `Errno` code.
    #[allow(clippy::cast_sign_loss)]
    put_u32(out, 0, err.as_i32() as u32);
    Ok(SYSINFO_REPLY_STATUS_LEN)
}

/// Decode a framed `sysinfo` reply: the payload slice on success, or the
/// server's per-query [`Errno`] on a non-zero status word.
///
/// # Errors
///
/// * [`Errno::BufferTooSmall`] if `bytes` is shorter than the status word.
/// * [`Errno::OutOfRange`] if the status word is a non-zero value that is not
///   a defined [`Errno`] code (wire corruption — fail closed).
/// * the server's reported [`Errno`] when the status word names one.
pub fn decode_reply(bytes: &[u8]) -> Result<&[u8], Errno> {
    if bytes.len() < SYSINFO_REPLY_STATUS_LEN {
        return Err(Errno::BufferTooSmall);
    }
    let status = read_u32(bytes, 0);
    if status == 0 {
        return Ok(&bytes[SYSINFO_REPLY_STATUS_LEN..]);
    }
    #[allow(clippy::cast_possible_wrap)]
    Err(Errno::from_i32(status as i32).unwrap_or(Errno::OutOfRange))
}

/// Envelope carried in front of every System Information request.
///
/// The header frames a typed request payload travelling over the IPC
/// transport to `sysinfod`. Total wire size is exactly
/// [`SysinfoRequestHeader::WIRE_LEN`] bytes, encoded little-endian
/// regardless of host architecture.
#[repr(C)]
#[derive(Copy, Clone, Debug, Eq, PartialEq)]
pub struct SysinfoRequestHeader {
    /// Must equal [`SYSINFO_REQUEST_MAGIC`].
    pub magic: u32,
    /// `sysinfo` protocol version; see [`SYSINFO_VERSION_CURRENT`].
    pub version: u16,
    /// Implementation-defined flag bits; reserved bits must be zero.
    pub flags: u16,
    /// Identifies which query the payload addresses.
    pub query: SysinfoQueryId,
    /// Reserved; must be zero in `sysinfo-v1`.
    pub reserved: u16,
    /// Length of the typed request payload that follows the header.
    pub payload_len: u32,
    /// Caller-chosen correlation token echoed in the response.
    pub request_id: u64,
}

impl SysinfoRequestHeader {
    /// Encoded size of a [`SysinfoRequestHeader`] on the wire.
    pub const WIRE_LEN: usize = 24;

    /// Encode `self` into its little-endian wire representation.
    #[must_use]
    pub fn to_le_bytes(&self) -> [u8; Self::WIRE_LEN] {
        let mut out = [0u8; Self::WIRE_LEN];
        put_u32(&mut out, 0, self.magic);
        put_u16(&mut out, 4, self.version);
        put_u16(&mut out, 6, self.flags);
        put_u16(&mut out, 8, self.query.as_u16());
        put_u16(&mut out, 10, self.reserved);
        put_u32(&mut out, 12, self.payload_len);
        put_u64(&mut out, 16, self.request_id);
        out
    }

    /// Decode `bytes` into a [`SysinfoRequestHeader`].
    ///
    /// Returns:
    /// * [`Errno::BufferTooSmall`] if `bytes.len() < WIRE_LEN`.
    /// * [`Errno::BadMagic`] if the magic word does not match, or the
    ///   reserved field is non-zero (reserved-must-be-zero violations are
    ///   wire corruption).
    /// * [`Errno::AbiVersionUnsupported`] if `version` is not
    ///   [`SYSINFO_VERSION_CURRENT`].
    /// * [`Errno::OutOfRange`] if `query` exceeds [`SysinfoQueryId::MAX`].
    /// * [`Errno::LengthOutOfRange`] if `payload_len` exceeds
    ///   [`SYSINFO_MAX_PAYLOAD_LEN`].
    pub fn from_bytes(bytes: &[u8]) -> Result<Self, Errno> {
        if bytes.len() < Self::WIRE_LEN {
            return Err(Errno::BufferTooSmall);
        }
        let magic = read_u32(bytes, 0);
        if magic != SYSINFO_REQUEST_MAGIC {
            return Err(Errno::BadMagic);
        }
        let version = read_u16(bytes, 4);
        if version != SYSINFO_VERSION_CURRENT {
            return Err(Errno::AbiVersionUnsupported);
        }
        let flags = read_u16(bytes, 6);
        let query = SysinfoQueryId::from_raw(read_u16(bytes, 8))?;
        let reserved = read_u16(bytes, 10);
        if reserved != 0 {
            return Err(Errno::BadMagic);
        }
        let payload_len = read_u32(bytes, 12);
        if payload_len > SYSINFO_MAX_PAYLOAD_LEN {
            return Err(Errno::LengthOutOfRange);
        }
        let request_id = read_u64(bytes, 16);
        Ok(Self {
            magic,
            version,
            flags,
            query,
            reserved,
            payload_len,
            request_id,
        })
    }
}

/// Request payload for the process-list queries
/// ([`SysinfoQueryId::SELF_PROCESS_LIST`] and
/// [`SysinfoQueryId::GLOBAL_PROCESS_LIST`]).
///
/// The response is a sequence of [`ProcessRecord`]s; the client pages
/// through it with `offset`/`limit` so a fixed-size transport buffer never
/// has to hold every process at once.
#[repr(C)]
#[derive(Copy, Clone, Debug, Eq, PartialEq, Default)]
pub struct ProcessListRequest {
    /// Number of leading records to skip.
    pub offset: u32,
    /// Maximum number of records the caller will accept in the response.
    pub limit: u16,
    /// Reserved flag bits; must be zero in `sysinfo-v1`.
    pub flags: u16,
}

impl ProcessListRequest {
    /// Encoded size on the wire.
    pub const WIRE_LEN: usize = 8;

    /// Encode `self` little-endian.
    #[must_use]
    pub fn to_le_bytes(&self) -> [u8; Self::WIRE_LEN] {
        let mut out = [0u8; Self::WIRE_LEN];
        put_u32(&mut out, 0, self.offset);
        put_u16(&mut out, 4, self.limit);
        put_u16(&mut out, 6, self.flags);
        out
    }

    /// Decode from `bytes`.
    ///
    /// Returns [`Errno::BufferTooSmall`] if the slice is short, or
    /// [`Errno::BadMagic`] if a reserved flag bit is set.
    pub fn from_bytes(bytes: &[u8]) -> Result<Self, Errno> {
        if bytes.len() < Self::WIRE_LEN {
            return Err(Errno::BufferTooSmall);
        }
        let flags = read_u16(bytes, 6);
        if flags != 0 {
            return Err(Errno::BadMagic);
        }
        Ok(Self {
            offset: read_u32(bytes, 0),
            limit: read_u16(bytes, 4),
            flags,
        })
    }
}

/// Maximum bytes of a process name carried in a [`ProcessRecord`].
pub const PROCESS_NAME_MAX: usize = 32;

/// [`ProcessRecord::cpu`] sentinel: the process is not currently executing
/// on any CPU (it is runnable-but-waiting, blocked, or a zombie).
///
/// A truthful record never reports a fabricated CPU for a task that is not
/// running; it reports this sentinel instead.
pub const PROCESS_CPU_NONE: u8 = 0xFF;

/// Lifecycle state of a process as reported by [`ProcessRecord`].
///
/// Discriminants are part of `sysinfo-v1` and must not be re-numbered.
#[repr(u8)]
#[derive(Copy, Clone, Debug, Eq, PartialEq, Hash)]
pub enum ProcessState {
    /// Eligible to run, waiting for a CPU.
    Runnable = 0,
    /// Currently executing on a CPU.
    Running = 1,
    /// Blocked awaiting an event (IPC, IRQ, timer).
    Blocked = 2,
    /// Exited; its exit status has not yet been reaped by its parent.
    Zombie = 3,
    /// Stopped by a job-control signal.
    Stopped = 4,
}

impl ProcessState {
    /// Numeric value carried on the wire.
    #[must_use]
    pub const fn as_u8(self) -> u8 {
        self as u8
    }

    /// Decode a wire byte into a [`ProcessState`].
    ///
    /// Returns [`Errno::OutOfRange`] for an unknown discriminant.
    pub const fn from_u8(raw: u8) -> Result<Self, Errno> {
        match raw {
            0 => Ok(Self::Runnable),
            1 => Ok(Self::Running),
            2 => Ok(Self::Blocked),
            3 => Ok(Self::Zombie),
            4 => Ok(Self::Stopped),
            _ => Err(Errno::OutOfRange),
        }
    }
}

/// One process entry in a process-list response.
///
/// Allocation-free: the name is stored inline in a fixed buffer and the
/// valid length is carried alongside it.
///
/// Identity is carried on two axes: the numeric [`pid`](Self::pid) /
/// [`parent_pid`](Self::parent_pid) (the scheduler task ids, familiar and
/// convenient for a `ps`-style display but *reused* across process
/// lifetimes) and the kernel-attested, never-reused
/// [`proc_id`](Self::proc_id) / [`parent_proc_id`](Self::parent_proc_id). A
/// consumer that must correlate a process across time — or distinguish two
/// lifetimes that reused a numeric id — keys on the `proc_id` pair, never the
/// numeric ids. Both axes are attested by the kernel from the task's own
/// state, never from a caller's claim.
#[repr(C)]
#[derive(Copy, Clone, Debug, Eq, PartialEq)]
pub struct ProcessRecord {
    /// Numeric process identifier (the scheduler task id).
    ///
    /// For human display and convenience only; numeric ids are reused, so
    /// [`proc_id`](Self::proc_id) is the stable identity.
    pub pid: u64,
    /// Numeric parent process identifier (`0` for a kernel-parented process
    /// such as PID 1).
    ///
    /// Display convenience only; [`parent_proc_id`](Self::parent_proc_id) is
    /// the stable parent link.
    pub parent_pid: u64,
    /// Kernel-attested, unforgeable process-instance identity.
    ///
    /// Never reused, so two lifetimes that reused a numeric [`pid`](Self::pid)
    /// remain distinguishable.
    pub proc_id: ProcId,
    /// Process-instance identity of the parent, or [`ProcId::KERNEL`] for a
    /// kernel-parented process (PID 1, storage-floor drivers).
    ///
    /// Like [`proc_id`](Self::proc_id) it survives numeric PID reuse, so the
    /// parent link is unambiguous even after the parent's numeric id has been
    /// recycled.
    pub parent_proc_id: ProcId,
    /// Owning user identifier.
    pub uid: u32,
    /// Owning (primary) group identifier.
    pub gid: u32,
    /// Lifecycle state.
    pub state: ProcessState,
    /// CPU the process is currently executing on, or [`PROCESS_CPU_NONE`]
    /// when it is not presently scheduled on any CPU.
    pub cpu: u8,
    /// Cumulative on-CPU time of the process, in nanoseconds.
    ///
    /// Accounted by the scheduler as the task is dispatched (kernel and
    /// user execution in the task's context alike) and reported through the
    /// architecture's monotonic clock; a task that has never run reports
    /// zero. Consumers derive `%CPU` from the delta between two samples
    /// and `TIME+` from the value directly.
    pub cpu_time_ns: u64,
    /// Bytes of memory currently mapped in the process's address space:
    /// its image, stack, and every anonymous region it has mapped, in
    /// whole pages.
    ///
    /// Zero when the process has no registered user address space (a pure
    /// kernel task) — a truthful "nothing mapped", never a guess.
    pub mem_bytes: u64,
    /// Valid byte count in the inline name buffer (`<= PROCESS_NAME_MAX`);
    /// read the bytes through [`ProcessRecord::name_bytes`].
    pub name_len: u8,
    name: [u8; PROCESS_NAME_MAX],
}

impl ProcessRecord {
    /// Encoded size on the wire.
    pub const WIRE_LEN: usize = 76 + PROCESS_NAME_MAX;

    /// Construct a record, copying up to [`PROCESS_NAME_MAX`] bytes of
    /// `name`.
    ///
    /// Returns [`Errno::LengthOutOfRange`] if `name` is longer than
    /// [`PROCESS_NAME_MAX`]; the name is never silently truncated.
    #[allow(clippy::too_many_arguments)]
    pub fn new(
        pid: u64,
        parent_pid: u64,
        proc_id: ProcId,
        parent_proc_id: ProcId,
        uid: u32,
        gid: u32,
        state: ProcessState,
        cpu: u8,
        cpu_time_ns: u64,
        mem_bytes: u64,
        name: &[u8],
    ) -> Result<Self, Errno> {
        if name.len() > PROCESS_NAME_MAX {
            return Err(Errno::LengthOutOfRange);
        }
        let mut buf = [0u8; PROCESS_NAME_MAX];
        buf[..name.len()].copy_from_slice(name);
        let name_len = u8::try_from(name.len()).map_err(|_| Errno::LengthOutOfRange)?;
        Ok(Self {
            pid,
            parent_pid,
            proc_id,
            parent_proc_id,
            uid,
            gid,
            state,
            cpu,
            cpu_time_ns,
            mem_bytes,
            name_len,
            name: buf,
        })
    }

    /// Borrow the valid prefix of the name buffer.
    #[must_use]
    pub fn name_bytes(&self) -> &[u8] {
        &self.name[..self.name_len as usize]
    }

    /// Encode `self` little-endian.
    #[must_use]
    pub fn to_le_bytes(&self) -> [u8; Self::WIRE_LEN] {
        let mut out = [0u8; Self::WIRE_LEN];
        put_u64(&mut out, 0, self.pid);
        put_u64(&mut out, 8, self.parent_pid);
        out[16..32].copy_from_slice(&self.proc_id.to_le_bytes());
        out[32..48].copy_from_slice(&self.parent_proc_id.to_le_bytes());
        put_u32(&mut out, 48, self.uid);
        put_u32(&mut out, 52, self.gid);
        out[56] = self.state.as_u8();
        out[57] = self.cpu;
        out[58] = self.name_len;
        // out[59] reserved, already zero.
        put_u64(&mut out, 60, self.cpu_time_ns);
        put_u64(&mut out, 68, self.mem_bytes);
        out[76..76 + PROCESS_NAME_MAX].copy_from_slice(&self.name);
        out
    }

    /// Decode from `bytes`.
    ///
    /// Returns [`Errno::BufferTooSmall`] if the slice is short,
    /// [`Errno::OutOfRange`] for an unknown [`ProcessState`], or
    /// [`Errno::LengthOutOfRange`] if `name_len` exceeds
    /// [`PROCESS_NAME_MAX`].
    pub fn from_bytes(bytes: &[u8]) -> Result<Self, Errno> {
        if bytes.len() < Self::WIRE_LEN {
            return Err(Errno::BufferTooSmall);
        }
        let proc_id = ProcId::from_bytes(&bytes[16..32])?;
        let parent_proc_id = ProcId::from_bytes(&bytes[32..48])?;
        let state = ProcessState::from_u8(bytes[56])?;
        let name_len = bytes[58];
        if name_len as usize > PROCESS_NAME_MAX {
            return Err(Errno::LengthOutOfRange);
        }
        let mut name = [0u8; PROCESS_NAME_MAX];
        name.copy_from_slice(&bytes[76..76 + PROCESS_NAME_MAX]);
        Ok(Self {
            pid: read_u64(bytes, 0),
            parent_pid: read_u64(bytes, 8),
            proc_id,
            parent_proc_id,
            uid: read_u32(bytes, 48),
            gid: read_u32(bytes, 52),
            state,
            cpu: bytes[57],
            cpu_time_ns: read_u64(bytes, 60),
            mem_bytes: read_u64(bytes, 68),
            name_len,
            name,
        })
    }
}

/// Response payload for [`SysinfoQueryId::KERNEL_MEMORY_STATS`].
#[repr(C)]
#[derive(Copy, Clone, Debug, Eq, PartialEq, Default)]
pub struct KernelMemoryStats {
    /// Total usable physical memory (RAM) managed by the kernel, in bytes.
    ///
    /// Counts only frames the kernel can ever allocate: firmware-reserved
    /// regions and physical-address holes (MMIO windows, space below the
    /// RAM base) are excluded, so `total_bytes - free_bytes` is memory
    /// genuinely in use.
    pub total_bytes: u64,
    /// Currently free physical memory, in bytes.
    pub free_bytes: u64,
    /// Memory committed to the kernel's own heaps and slabs, in bytes.
    pub kernel_heap_bytes: u64,
    /// Memory resident in user address spaces, in bytes.
    pub user_resident_bytes: u64,
    /// Page size in bytes for the reporting architecture.
    pub page_size: u32,
    /// Reserved; must be zero in `sysinfo-v1`.
    pub reserved: u32,
}

impl KernelMemoryStats {
    /// Encoded size on the wire.
    pub const WIRE_LEN: usize = 40;

    /// Encode `self` little-endian.
    #[must_use]
    pub fn to_le_bytes(&self) -> [u8; Self::WIRE_LEN] {
        let mut out = [0u8; Self::WIRE_LEN];
        put_u64(&mut out, 0, self.total_bytes);
        put_u64(&mut out, 8, self.free_bytes);
        put_u64(&mut out, 16, self.kernel_heap_bytes);
        put_u64(&mut out, 24, self.user_resident_bytes);
        put_u32(&mut out, 32, self.page_size);
        put_u32(&mut out, 36, self.reserved);
        out
    }

    /// Decode from `bytes`.
    ///
    /// Returns [`Errno::BufferTooSmall`] if short, or [`Errno::BadMagic`]
    /// if the reserved field is non-zero.
    pub fn from_bytes(bytes: &[u8]) -> Result<Self, Errno> {
        if bytes.len() < Self::WIRE_LEN {
            return Err(Errno::BufferTooSmall);
        }
        let reserved = read_u32(bytes, 36);
        if reserved != 0 {
            return Err(Errno::BadMagic);
        }
        Ok(Self {
            total_bytes: read_u64(bytes, 0),
            free_bytes: read_u64(bytes, 8),
            kernel_heap_bytes: read_u64(bytes, 16),
            user_resident_bytes: read_u64(bytes, 24),
            page_size: read_u32(bytes, 32),
            reserved,
        })
    }
}

/// Response payload for [`SysinfoQueryId::UPTIME`].
///
/// Time is carried with the 64-bit-native ABI types: the
/// monotonic span since boot as a [`Duration64`] and the wall-clock boot
/// instant as a [`Time64`]. Absolute time is never a seconds-only scalar.
#[repr(C)]
#[derive(Copy, Clone, Debug, Eq, PartialEq, Default)]
pub struct Uptime {
    /// Monotonic span since boot.
    pub since_boot: Duration64,
    /// Wall-clock boot instant.
    pub boot_time: Time64,
}

impl Uptime {
    /// Encoded size on the wire.
    pub const WIRE_LEN: usize = Duration64::WIRE_LEN + Time64::WIRE_LEN;

    /// Encode `self` little-endian.
    #[must_use]
    pub fn to_le_bytes(&self) -> [u8; Self::WIRE_LEN] {
        let mut out = [0u8; Self::WIRE_LEN];
        out[0..Duration64::WIRE_LEN].copy_from_slice(&self.since_boot.to_le_bytes());
        out[Duration64::WIRE_LEN..].copy_from_slice(&self.boot_time.to_le_bytes());
        out
    }

    /// Decode from `bytes`.
    ///
    /// Returns [`Errno::BufferTooSmall`] if short, or
    /// [`Errno::TimestampOutOfRange`] if an encoded sub-second field is not
    /// canonical.
    pub fn from_bytes(bytes: &[u8]) -> Result<Self, Errno> {
        if bytes.len() < Self::WIRE_LEN {
            return Err(Errno::BufferTooSmall);
        }
        Ok(Self {
            since_boot: Duration64::from_bytes(&bytes[0..Duration64::WIRE_LEN])?,
            boot_time: Time64::from_bytes(&bytes[Duration64::WIRE_LEN..])?,
        })
    }
}

/// Binary point of the fixed-point load-average values in [`LoadAverage`]:
/// a load of 1.00 is `1 << LOAD_FIXED_SHIFT`.
pub const LOAD_FIXED_SHIFT: u32 = 11;

/// Response payload for [`SysinfoQueryId::LOAD_AVERAGE`] — the `uptime(1)`
/// figures: exponentially-damped 1/5/15-minute run-queue averages, the
/// live task census, and the number of distinct logged-in users.
///
/// The averages are fixed-point with [`LOAD_FIXED_SHIFT`] fractional bits;
/// [`LoadAverage::whole`] and [`LoadAverage::centis`] render the
/// conventional `W.CC` form from one shared definition.
#[repr(C)]
#[derive(Copy, Clone, Debug, Eq, PartialEq, Default)]
pub struct LoadAverage {
    /// One-minute damped average of the runnable-task count (fixed-point).
    pub load1: u32,
    /// Five-minute damped average (fixed-point).
    pub load5: u32,
    /// Fifteen-minute damped average (fixed-point).
    pub load15: u32,
    /// Tasks currently runnable or running.
    pub runnable: u32,
    /// Live (non-zombie) tasks in total.
    pub total_tasks: u32,
    /// Distinct non-system uids owning at least one live task — the
    /// logged-in-user census.
    pub users: u32,
}

impl LoadAverage {
    /// Encoded size on the wire.
    pub const WIRE_LEN: usize = 24;

    /// Whole part of a fixed-point load value.
    #[must_use]
    pub const fn whole(fixed: u32) -> u32 {
        fixed >> LOAD_FIXED_SHIFT
    }

    /// Hundredths part of a fixed-point load value, `0..=99`.
    #[must_use]
    pub const fn centis(fixed: u32) -> u32 {
        ((fixed & ((1 << LOAD_FIXED_SHIFT) - 1)) * 100) >> LOAD_FIXED_SHIFT
    }

    /// Encode `self` little-endian.
    #[must_use]
    pub fn to_le_bytes(&self) -> [u8; Self::WIRE_LEN] {
        let mut out = [0u8; Self::WIRE_LEN];
        out[0..4].copy_from_slice(&self.load1.to_le_bytes());
        out[4..8].copy_from_slice(&self.load5.to_le_bytes());
        out[8..12].copy_from_slice(&self.load15.to_le_bytes());
        out[12..16].copy_from_slice(&self.runnable.to_le_bytes());
        out[16..20].copy_from_slice(&self.total_tasks.to_le_bytes());
        out[20..24].copy_from_slice(&self.users.to_le_bytes());
        out
    }

    /// Decode from `bytes`.
    ///
    /// Returns [`Errno::BufferTooSmall`] if `bytes` is shorter than
    /// [`Self::WIRE_LEN`]. Every field value is representable, so no other
    /// shape check applies.
    pub fn from_bytes(bytes: &[u8]) -> Result<Self, Errno> {
        if bytes.len() < Self::WIRE_LEN {
            return Err(Errno::BufferTooSmall);
        }
        let word = |at: usize| {
            u32::from_le_bytes([bytes[at], bytes[at + 1], bytes[at + 2], bytes[at + 3]])
        };
        Ok(Self {
            load1: word(0),
            load5: word(4),
            load15: word(8),
            runnable: word(12),
            total_tasks: word(16),
            users: word(20),
        })
    }
}

/// Maximum bytes of a username carried in a [`UserDirectoryRecord`] — the
/// same bound the `users-v1` database enforces on account names.
pub const USER_DIRECTORY_NAME_MAX: usize = 32;

/// Request payload for [`SysinfoQueryId::USER_DIRECTORY`]: the record
/// window to return, mirroring the process-list paging shape.
#[repr(C)]
#[derive(Copy, Clone, Debug, Eq, PartialEq, Default)]
pub struct UserDirectoryRequest {
    /// Zero-based index of the first record to return.
    pub offset: u32,
    /// Maximum number of records to return.
    pub limit: u16,
    /// Reserved; must be zero in `sysinfo-v1`.
    pub flags: u16,
}

impl UserDirectoryRequest {
    /// Encoded size on the wire.
    pub const WIRE_LEN: usize = 8;

    /// Encode `self` little-endian.
    #[must_use]
    pub fn to_le_bytes(&self) -> [u8; Self::WIRE_LEN] {
        let mut out = [0u8; Self::WIRE_LEN];
        put_u32(&mut out, 0, self.offset);
        put_u16(&mut out, 4, self.limit);
        put_u16(&mut out, 6, self.flags);
        out
    }

    /// Decode from `bytes`.
    ///
    /// Returns [`Errno::BufferTooSmall`] if the slice is short, or
    /// [`Errno::BadMagic`] if a reserved flag bit is set.
    pub fn from_bytes(bytes: &[u8]) -> Result<Self, Errno> {
        if bytes.len() < Self::WIRE_LEN {
            return Err(Errno::BufferTooSmall);
        }
        let flags = read_u16(bytes, 6);
        if flags != 0 {
            return Err(Errno::BadMagic);
        }
        Ok(Self {
            offset: read_u32(bytes, 0),
            limit: read_u16(bytes, 4),
            flags,
        })
    }
}

/// One account entry in a [`SysinfoQueryId::USER_DIRECTORY`] response: the
/// numeric uid and the account's username, and **nothing else** — no
/// password material, home, shell, or grant set. The `/etc/passwd`-class
/// public pairing a `top`/`ls -l`-style display renders.
///
/// Allocation-free: the name is stored inline in a fixed buffer with its
/// valid length alongside.
#[repr(C)]
#[derive(Copy, Clone, Debug, Eq, PartialEq)]
pub struct UserDirectoryRecord {
    /// The account's numeric user identifier.
    pub uid: u32,
    /// Valid byte count in the inline name buffer
    /// (`<= USER_DIRECTORY_NAME_MAX`); read the bytes through
    /// [`UserDirectoryRecord::name_bytes`].
    pub name_len: u8,
    name: [u8; USER_DIRECTORY_NAME_MAX],
}

impl UserDirectoryRecord {
    /// Encoded size on the wire: uid + name length + three reserved bytes
    /// + the inline name buffer.
    pub const WIRE_LEN: usize = 8 + USER_DIRECTORY_NAME_MAX;

    /// Construct a record, copying up to [`USER_DIRECTORY_NAME_MAX`] bytes
    /// of `name`.
    ///
    /// Returns [`Errno::LengthOutOfRange`] if `name` is longer than
    /// [`USER_DIRECTORY_NAME_MAX`]; the name is never silently truncated.
    pub fn new(uid: u32, name: &[u8]) -> Result<Self, Errno> {
        if name.len() > USER_DIRECTORY_NAME_MAX {
            return Err(Errno::LengthOutOfRange);
        }
        let mut buf = [0u8; USER_DIRECTORY_NAME_MAX];
        buf[..name.len()].copy_from_slice(name);
        let name_len = u8::try_from(name.len()).map_err(|_| Errno::LengthOutOfRange)?;
        Ok(Self {
            uid,
            name_len,
            name: buf,
        })
    }

    /// Borrow the valid prefix of the name buffer.
    #[must_use]
    pub fn name_bytes(&self) -> &[u8] {
        &self.name[..self.name_len as usize]
    }

    /// Encode `self` little-endian.
    #[must_use]
    pub fn to_le_bytes(&self) -> [u8; Self::WIRE_LEN] {
        let mut out = [0u8; Self::WIRE_LEN];
        put_u32(&mut out, 0, self.uid);
        out[4] = self.name_len;
        // out[5..8] reserved, already zero.
        out[8..8 + USER_DIRECTORY_NAME_MAX].copy_from_slice(&self.name);
        out
    }

    /// Decode from `bytes`.
    ///
    /// Returns [`Errno::BufferTooSmall`] if the slice is short, or
    /// [`Errno::LengthOutOfRange`] if `name_len` exceeds
    /// [`USER_DIRECTORY_NAME_MAX`].
    pub fn from_bytes(bytes: &[u8]) -> Result<Self, Errno> {
        if bytes.len() < Self::WIRE_LEN {
            return Err(Errno::BufferTooSmall);
        }
        let name_len = bytes[4];
        if name_len as usize > USER_DIRECTORY_NAME_MAX {
            return Err(Errno::LengthOutOfRange);
        }
        let mut name = [0u8; USER_DIRECTORY_NAME_MAX];
        name.copy_from_slice(&bytes[8..8 + USER_DIRECTORY_NAME_MAX]);
        Ok(Self {
            uid: read_u32(bytes, 0),
            name_len,
            name,
        })
    }
}

/// Request payload for [`SysinfoQueryId::CPU_TIME_STATS`].
///
/// Structurally parallel to [`MountListRequest`] but a distinct frozen
/// payload: each `sysinfo-v1` query owns its argument type. The response is
/// a sequence of [`CpuTimeRecord`]s; the client pages through it with
/// `offset`/`limit` so a fixed-size transport buffer never bounds how many
/// CPUs the machine may have.
#[repr(C)]
#[derive(Copy, Clone, Debug, Eq, PartialEq, Default)]
pub struct CpuTimeListRequest {
    /// Index of the first CPU to return.
    pub offset: u32,
    /// Maximum number of [`CpuTimeRecord`]s the caller will accept.
    pub limit: u16,
    /// Reserved; must be zero in `sysinfo-v1`.
    pub flags: u16,
}

impl CpuTimeListRequest {
    /// Encoded size of a [`CpuTimeListRequest`] on the wire.
    pub const WIRE_LEN: usize = 8;

    /// Encode `self` into its little-endian wire representation.
    #[must_use]
    pub fn to_le_bytes(&self) -> [u8; Self::WIRE_LEN] {
        let mut out = [0u8; Self::WIRE_LEN];
        put_u32(&mut out, 0, self.offset);
        put_u16(&mut out, 4, self.limit);
        put_u16(&mut out, 6, self.flags);
        out
    }

    /// Decode `bytes` into a [`CpuTimeListRequest`].
    ///
    /// Returns:
    /// * [`Errno::BufferTooSmall`] if `bytes.len() < WIRE_LEN`.
    /// * [`Errno::BadMagic`] if the reserved `flags` field is non-zero
    ///   (reserved-must-be-zero violations are wire corruption).
    pub fn from_bytes(bytes: &[u8]) -> Result<Self, Errno> {
        if bytes.len() < Self::WIRE_LEN {
            return Err(Errno::BufferTooSmall);
        }
        let flags = read_u16(bytes, 6);
        if flags != 0 {
            return Err(Errno::BadMagic);
        }
        Ok(Self {
            offset: read_u32(bytes, 0),
            limit: read_u16(bytes, 4),
            flags,
        })
    }
}

/// One CPU's execution-time accounting inside a
/// [`SysinfoQueryId::CPU_TIME_STATS`] response.
///
/// `busy_ns` is the cumulative time this CPU has spent dispatching task
/// bodies since boot, accounted on the scheduler's dispatch path; `idle_ns`
/// is the remainder of the same monotonic sample instant, so
/// `busy_ns + idle_ns` is that CPU's share of uptime at the sample. A
/// consumer derives a utilisation percentage from the *deltas* of two
/// samples, exactly as `top` reads `/proc/stat` on Linux; RustOS does not
/// account a user/system/nice/iowait split, so the honest vocabulary is
/// busy and idle only.
#[repr(C)]
#[derive(Copy, Clone, Debug, Eq, PartialEq, Default)]
pub struct CpuTimeRecord {
    /// The CPU index this record describes.
    pub cpu: u32,
    /// Reserved; must be zero in `sysinfo-v1`.
    pub reserved: u32,
    /// Cumulative nanoseconds this CPU spent running tasks since boot.
    pub busy_ns: u64,
    /// Nanoseconds of the sample's uptime this CPU was not running tasks.
    pub idle_ns: u64,
}

impl CpuTimeRecord {
    /// Encoded size on the wire.
    pub const WIRE_LEN: usize = 24;

    /// Encode `self` little-endian.
    #[must_use]
    pub fn to_le_bytes(&self) -> [u8; Self::WIRE_LEN] {
        let mut out = [0u8; Self::WIRE_LEN];
        put_u32(&mut out, 0, self.cpu);
        put_u32(&mut out, 4, self.reserved);
        put_u64(&mut out, 8, self.busy_ns);
        put_u64(&mut out, 16, self.idle_ns);
        out
    }

    /// Decode from `bytes`.
    ///
    /// Returns [`Errno::BufferTooSmall`] if short, or [`Errno::BadMagic`]
    /// if the reserved field is non-zero.
    pub fn from_bytes(bytes: &[u8]) -> Result<Self, Errno> {
        if bytes.len() < Self::WIRE_LEN {
            return Err(Errno::BufferTooSmall);
        }
        let reserved = read_u32(bytes, 4);
        if reserved != 0 {
            return Err(Errno::BadMagic);
        }
        Ok(Self {
            cpu: read_u32(bytes, 0),
            reserved,
            busy_ns: read_u64(bytes, 8),
            idle_ns: read_u64(bytes, 16),
        })
    }
}

/// Request payload for [`SysinfoQueryId::SEAT_LIST`].
///
/// Identical paging shape to [`CpuTimeListRequest`]: `offset` names the
/// first seat-record index to return and `limit` bounds the page.
#[repr(C)]
#[derive(Copy, Clone, Debug, Eq, PartialEq, Default)]
pub struct SeatListRequest {
    /// Index of the first seat record to return.
    pub offset: u32,
    /// Maximum number of records to return.
    pub limit: u16,
    /// Reserved; must be zero in `sysinfo-v1`.
    pub flags: u16,
}

impl SeatListRequest {
    /// Encoded size on the wire.
    pub const WIRE_LEN: usize = 8;

    /// Encode `self` little-endian.
    #[must_use]
    pub fn to_le_bytes(&self) -> [u8; Self::WIRE_LEN] {
        let mut out = [0u8; Self::WIRE_LEN];
        put_u32(&mut out, 0, self.offset);
        put_u16(&mut out, 4, self.limit);
        put_u16(&mut out, 6, self.flags);
        out
    }

    /// Decode from `bytes`.
    ///
    /// Returns [`Errno::BufferTooSmall`] if short, or [`Errno::BadMagic`] if
    /// a reserved flag bit is set (fail closed on an unknown request shape).
    pub fn from_bytes(bytes: &[u8]) -> Result<Self, Errno> {
        if bytes.len() < Self::WIRE_LEN {
            return Err(Errno::BufferTooSmall);
        }
        let flags = read_u16(bytes, 6);
        if flags != 0 {
            return Err(Errno::BadMagic);
        }
        Ok(Self {
            offset: read_u32(bytes, 0),
            limit: read_u16(bytes, 4),
            flags,
        })
    }
}

/// [`SeatRecord::flags`] bit: the seat is currently held under a lease and
/// [`SeatRecord::owner_task`] names the owning task.
pub const SEAT_FLAG_OWNED: u32 = 1 << 0;

/// One seat's state inside a [`SysinfoQueryId::SEAT_LIST`] response
/// (`plans/DISPLAY.md` D3).
///
/// Every field is filled from the kernel's seat registry — the
/// kernel-attested owner task, never a caller claim. An unowned seat (which
/// includes one whose lease was just revoked) carries no owner: the
/// [`SEAT_FLAG_OWNED`] bit is clear and `owner_task` is zero.
#[repr(C)]
#[derive(Copy, Clone, Debug, Eq, PartialEq, Default)]
pub struct SeatRecord {
    /// The seat this record describes (the boot seat is id 0; further
    /// seats are minted per discovered display node).
    pub seat_id: u64,
    /// The task holding the seat's lease; valid only when
    /// [`SEAT_FLAG_OWNED`] is set, zero otherwise.
    pub owner_task: u64,
    /// The seat's monotonic lease-grant counter: the generation of the most
    /// recently minted lease, `0` if the seat has never been acquired.
    pub generation: u64,
    /// Index of the text console an unowned seat's input drains to.
    pub foreground_console: u32,
    /// [`SEAT_FLAG_OWNED`] plus reserved bits that must be zero.
    pub flags: u32,
}

impl SeatRecord {
    /// Encoded size on the wire.
    pub const WIRE_LEN: usize = 32;

    /// `true` when the seat is held under a live lease.
    #[must_use]
    pub const fn owned(&self) -> bool {
        self.flags & SEAT_FLAG_OWNED != 0
    }

    /// The owning task, if the seat is held.
    #[must_use]
    pub const fn owner(&self) -> Option<u64> {
        if self.owned() {
            Some(self.owner_task)
        } else {
            None
        }
    }

    /// Encode `self` little-endian.
    #[must_use]
    pub fn to_le_bytes(&self) -> [u8; Self::WIRE_LEN] {
        let mut out = [0u8; Self::WIRE_LEN];
        put_u64(&mut out, 0, self.seat_id);
        put_u64(&mut out, 8, self.owner_task);
        put_u64(&mut out, 16, self.generation);
        put_u32(&mut out, 24, self.foreground_console);
        put_u32(&mut out, 28, self.flags);
        out
    }

    /// Decode from `bytes`.
    ///
    /// Returns [`Errno::BufferTooSmall`] if short, or [`Errno::BadMagic`] if
    /// a reserved flag bit is set or an unowned record carries a non-zero
    /// owner (fail closed on wire corruption rather than fabricating an
    /// owner).
    pub fn from_bytes(bytes: &[u8]) -> Result<Self, Errno> {
        if bytes.len() < Self::WIRE_LEN {
            return Err(Errno::BufferTooSmall);
        }
        let flags = read_u32(bytes, 28);
        if flags & !SEAT_FLAG_OWNED != 0 {
            return Err(Errno::BadMagic);
        }
        let owner_task = read_u64(bytes, 8);
        if flags & SEAT_FLAG_OWNED == 0 && owner_task != 0 {
            return Err(Errno::BadMagic);
        }
        Ok(Self {
            seat_id: read_u64(bytes, 0),
            owner_task,
            generation: read_u64(bytes, 16),
            foreground_console: read_u32(bytes, 24),
            flags,
        })
    }
}

/// Bytes of the per-installation machine identifier.
pub const MACHINE_ID_LEN: usize = 16;

/// Maximum bytes of a hostname carried in a [`SystemIdentity`].
pub const HOSTNAME_MAX: usize = 64;

/// Response payload for [`SysinfoQueryId::SYSTEM_IDENTITY`].
///
/// Allocation-free: the hostname is stored inline in a fixed buffer with
/// its valid length alongside.
#[repr(C)]
#[derive(Copy, Clone, Debug, Eq, PartialEq)]
pub struct SystemIdentity {
    /// Per-installation machine identifier minted by the installer.
    pub machine_id: [u8; MACHINE_ID_LEN],
    /// OS major version.
    pub version_major: u16,
    /// OS minor version.
    pub version_minor: u16,
    /// OS patch version.
    pub version_patch: u16,
    /// Valid byte count in the inline hostname buffer (`<= HOSTNAME_MAX`);
    /// read the bytes through [`SystemIdentity::hostname_bytes`].
    pub hostname_len: u8,
    hostname: [u8; HOSTNAME_MAX],
}

impl SystemIdentity {
    /// Encoded size on the wire.
    pub const WIRE_LEN: usize = MACHINE_ID_LEN + 8 + HOSTNAME_MAX;

    /// Construct an identity, copying up to [`HOSTNAME_MAX`] bytes of
    /// `hostname`.
    ///
    /// Returns [`Errno::LengthOutOfRange`] if `hostname` is too long; the
    /// name is never silently truncated.
    pub fn new(
        machine_id: [u8; MACHINE_ID_LEN],
        version_major: u16,
        version_minor: u16,
        version_patch: u16,
        hostname: &[u8],
    ) -> Result<Self, Errno> {
        if hostname.len() > HOSTNAME_MAX {
            return Err(Errno::LengthOutOfRange);
        }
        let mut buf = [0u8; HOSTNAME_MAX];
        buf[..hostname.len()].copy_from_slice(hostname);
        let hostname_len = u8::try_from(hostname.len()).map_err(|_| Errno::LengthOutOfRange)?;
        Ok(Self {
            machine_id,
            version_major,
            version_minor,
            version_patch,
            hostname_len,
            hostname: buf,
        })
    }

    /// Borrow the valid prefix of the hostname buffer.
    #[must_use]
    pub fn hostname_bytes(&self) -> &[u8] {
        &self.hostname[..self.hostname_len as usize]
    }

    /// Encode `self` little-endian.
    #[must_use]
    pub fn to_le_bytes(&self) -> [u8; Self::WIRE_LEN] {
        let mut out = [0u8; Self::WIRE_LEN];
        out[0..MACHINE_ID_LEN].copy_from_slice(&self.machine_id);
        put_u16(&mut out, MACHINE_ID_LEN, self.version_major);
        put_u16(&mut out, MACHINE_ID_LEN + 2, self.version_minor);
        put_u16(&mut out, MACHINE_ID_LEN + 4, self.version_patch);
        out[MACHINE_ID_LEN + 6] = self.hostname_len;
        // byte MACHINE_ID_LEN + 7 reserved, already zero.
        out[MACHINE_ID_LEN + 8..].copy_from_slice(&self.hostname);
        out
    }

    /// Decode from `bytes`.
    ///
    /// Returns [`Errno::BufferTooSmall`] if short, or
    /// [`Errno::LengthOutOfRange`] if `hostname_len` exceeds
    /// [`HOSTNAME_MAX`].
    pub fn from_bytes(bytes: &[u8]) -> Result<Self, Errno> {
        if bytes.len() < Self::WIRE_LEN {
            return Err(Errno::BufferTooSmall);
        }
        let mut machine_id = [0u8; MACHINE_ID_LEN];
        machine_id.copy_from_slice(&bytes[0..MACHINE_ID_LEN]);
        let hostname_len = bytes[MACHINE_ID_LEN + 6];
        if hostname_len as usize > HOSTNAME_MAX {
            return Err(Errno::LengthOutOfRange);
        }
        let mut hostname = [0u8; HOSTNAME_MAX];
        hostname.copy_from_slice(&bytes[MACHINE_ID_LEN + 8..Self::WIRE_LEN]);
        Ok(Self {
            machine_id,
            version_major: read_u16(bytes, MACHINE_ID_LEN),
            version_minor: read_u16(bytes, MACHINE_ID_LEN + 2),
            version_patch: read_u16(bytes, MACHINE_ID_LEN + 4),
            hostname_len,
            hostname,
        })
    }
}

/// Maximum bytes of the source identifier carried in a [`MountRecord`].
///
/// The source is the backing volume or device name (e.g. a `/Storage`
/// volume label); it shares the path-length ceiling with the target.
pub const MOUNT_SOURCE_MAX: usize = 64;

/// Maximum bytes of the mount-point path carried in a [`MountRecord`].
pub const MOUNT_TARGET_MAX: usize = 64;

/// Maximum bytes of the filesystem-type name carried in a [`MountRecord`].
///
/// Driver type names (`rustfs`, `ext4`, `fat32`, …) are short; the bound
/// keeps a hostile reply from claiming an unbounded type string.
pub const MOUNT_FSTYPE_MAX: usize = 16;

/// Request payload for [`SysinfoQueryId::MOUNT_LIST`].
///
/// Structurally parallel to [`ProcessListRequest`] but a distinct frozen
/// payload: each `sysinfo-v1` query owns its argument type, exactly as each
/// syscall owns its argument shape. The response is a
/// sequence of [`MountRecord`]s; the client pages through it with
/// `offset`/`limit` so a fixed-size transport buffer never has to hold every
/// mount at once.
#[repr(C)]
#[derive(Copy, Clone, Debug, Eq, PartialEq, Default)]
pub struct MountListRequest {
    /// Index of the first mount to return.
    pub offset: u32,
    /// Maximum number of [`MountRecord`]s the caller will accept.
    pub limit: u16,
    /// Reserved; must be zero in `sysinfo-v1`.
    pub flags: u16,
}

impl MountListRequest {
    /// Encoded size of a [`MountListRequest`] on the wire.
    pub const WIRE_LEN: usize = 8;

    /// Encode `self` into its little-endian wire representation.
    #[must_use]
    pub fn to_le_bytes(&self) -> [u8; Self::WIRE_LEN] {
        let mut out = [0u8; Self::WIRE_LEN];
        put_u32(&mut out, 0, self.offset);
        put_u16(&mut out, 4, self.limit);
        put_u16(&mut out, 6, self.flags);
        out
    }

    /// Decode `bytes` into a [`MountListRequest`].
    ///
    /// Returns:
    /// * [`Errno::BufferTooSmall`] if `bytes.len() < WIRE_LEN`.
    /// * [`Errno::BadMagic`] if the reserved `flags` field is non-zero
    ///   (reserved-must-be-zero violations are wire corruption).
    pub fn from_bytes(bytes: &[u8]) -> Result<Self, Errno> {
        if bytes.len() < Self::WIRE_LEN {
            return Err(Errno::BufferTooSmall);
        }
        let flags = read_u16(bytes, 6);
        if flags != 0 {
            return Err(Errno::BadMagic);
        }
        Ok(Self {
            offset: read_u32(bytes, 0),
            limit: read_u16(bytes, 4),
            flags,
        })
    }
}

/// One entry in the system mount table, returned by
/// [`SysinfoQueryId::MOUNT_LIST`].
///
/// Each record names a mounted filesystem by its backing `source`, the
/// `target` path it is mounted at, the driver `fstype`, the
/// [`MountFlags`] policy in force (`ro`/`nosuid`/`nodev`/`noexec`), and the
/// volume's space accounting as a [`VolumeStats`] — the same type the
/// filesystem driver ABI defines, so the numbers cross the wire in the one
/// shape the driver reported them in.
/// The string fields are inline fixed-capacity buffers, so the whole record
/// is a flat, allocation-free `repr(C)` block of [`MountRecord::WIRE_LEN`]
/// bytes encoded little-endian. The policy bits reuse the same
/// [`MountFlags`] type the filesystem driver ABI defines rather than
/// re-declaring the flag algebra.
///
/// A mount with no live backing volume (or one whose driver reports no
/// accounting) carries an all-zero [`VolumeStats`]: zero total blocks is the
/// honest "no capacity known" answer a consumer skips, never a guess.
#[repr(C)]
#[derive(Copy, Clone, Debug, Eq, PartialEq)]
pub struct MountRecord {
    flags: MountFlags,
    source_len: u8,
    target_len: u8,
    fstype_len: u8,
    usage: VolumeStats,
    source: [u8; MOUNT_SOURCE_MAX],
    target: [u8; MOUNT_TARGET_MAX],
    fstype: [u8; MOUNT_FSTYPE_MAX],
}

impl MountRecord {
    /// Encoded size of a [`MountRecord`] on the wire.
    ///
    /// `4` bytes of flags, three length bytes plus one reserved pad, the
    /// usage block (`block_size(4)` + reserved pad `(4)` + five `u64`
    /// counts), then the three fixed-capacity string buffers.
    pub const WIRE_LEN: usize =
        Self::SOURCE_OFFSET + MOUNT_SOURCE_MAX + MOUNT_TARGET_MAX + MOUNT_FSTYPE_MAX;

    const USAGE_OFFSET: usize = 8;
    const SOURCE_OFFSET: usize = Self::USAGE_OFFSET + 48;
    const TARGET_OFFSET: usize = Self::SOURCE_OFFSET + MOUNT_SOURCE_MAX;
    const FSTYPE_OFFSET: usize = Self::TARGET_OFFSET + MOUNT_TARGET_MAX;

    /// Build a record from its parts.
    ///
    /// # Errors
    ///
    /// [`Errno::LengthOutOfRange`] if `source` exceeds [`MOUNT_SOURCE_MAX`],
    /// `target` exceeds [`MOUNT_TARGET_MAX`], or `fstype` exceeds
    /// [`MOUNT_FSTYPE_MAX`]; [`Errno::OutOfRange`] if `usage` is internally
    /// inconsistent (available exceeding free, or free exceeding total).
    pub fn new(
        source: &[u8],
        target: &[u8],
        fstype: &[u8],
        flags: MountFlags,
        usage: VolumeStats,
    ) -> Result<Self, Errno> {
        if source.len() > MOUNT_SOURCE_MAX
            || target.len() > MOUNT_TARGET_MAX
            || fstype.len() > MOUNT_FSTYPE_MAX
        {
            return Err(Errno::LengthOutOfRange);
        }
        if usage.avail_blocks > usage.free_blocks || usage.free_blocks > usage.total_blocks {
            return Err(Errno::OutOfRange);
        }
        let source_len = u8::try_from(source.len()).map_err(|_| Errno::LengthOutOfRange)?;
        let target_len = u8::try_from(target.len()).map_err(|_| Errno::LengthOutOfRange)?;
        let fstype_len = u8::try_from(fstype.len()).map_err(|_| Errno::LengthOutOfRange)?;
        let mut record = Self {
            flags,
            source_len,
            target_len,
            fstype_len,
            usage,
            source: [0u8; MOUNT_SOURCE_MAX],
            target: [0u8; MOUNT_TARGET_MAX],
            fstype: [0u8; MOUNT_FSTYPE_MAX],
        };
        record.source[..source.len()].copy_from_slice(source);
        record.target[..target.len()].copy_from_slice(target);
        record.fstype[..fstype.len()].copy_from_slice(fstype);
        Ok(record)
    }

    /// The mount policy flags in force on this filesystem.
    #[must_use]
    pub fn flags(&self) -> MountFlags {
        self.flags
    }

    /// The volume's space accounting (all-zero when no backing volume
    /// reported one).
    #[must_use]
    pub fn usage(&self) -> VolumeStats {
        self.usage
    }

    /// The backing source bytes (volume/device identifier).
    #[must_use]
    pub fn source_bytes(&self) -> &[u8] {
        &self.source[..self.source_len as usize]
    }

    /// The mount-point path bytes.
    #[must_use]
    pub fn target_bytes(&self) -> &[u8] {
        &self.target[..self.target_len as usize]
    }

    /// The filesystem-type name bytes.
    #[must_use]
    pub fn fstype_bytes(&self) -> &[u8] {
        &self.fstype[..self.fstype_len as usize]
    }

    /// Encode `self` into its little-endian wire representation.
    #[must_use]
    pub fn to_le_bytes(&self) -> [u8; Self::WIRE_LEN] {
        let mut out = [0u8; Self::WIRE_LEN];
        put_u32(&mut out, 0, self.flags.bits());
        out[4] = self.source_len;
        out[5] = self.target_len;
        out[6] = self.fstype_len;
        put_u32(&mut out, Self::USAGE_OFFSET, self.usage.block_size);
        put_u64(&mut out, Self::USAGE_OFFSET + 8, self.usage.total_blocks);
        put_u64(&mut out, Self::USAGE_OFFSET + 16, self.usage.free_blocks);
        put_u64(&mut out, Self::USAGE_OFFSET + 24, self.usage.avail_blocks);
        put_u64(&mut out, Self::USAGE_OFFSET + 32, self.usage.files);
        put_u64(&mut out, Self::USAGE_OFFSET + 40, self.usage.files_free);
        out[Self::SOURCE_OFFSET..Self::SOURCE_OFFSET + MOUNT_SOURCE_MAX]
            .copy_from_slice(&self.source);
        out[Self::TARGET_OFFSET..Self::TARGET_OFFSET + MOUNT_TARGET_MAX]
            .copy_from_slice(&self.target);
        out[Self::FSTYPE_OFFSET..Self::FSTYPE_OFFSET + MOUNT_FSTYPE_MAX]
            .copy_from_slice(&self.fstype);
        out
    }

    /// Decode `bytes` into a [`MountRecord`].
    ///
    /// Returns:
    /// * [`Errno::BufferTooSmall`] if `bytes.len() < WIRE_LEN`.
    /// * [`Errno::OutOfRange`] if the flags word sets a bit outside the
    ///   known [`MountFlags`] mask, the reserved pad bytes are non-zero, or
    ///   the usage block is internally inconsistent (available exceeding
    ///   free, or free exceeding total — a hostile reply, refused whole).
    /// * [`Errno::LengthOutOfRange`] if any length byte exceeds its buffer.
    pub fn from_bytes(bytes: &[u8]) -> Result<Self, Errno> {
        if bytes.len() < Self::WIRE_LEN {
            return Err(Errno::BufferTooSmall);
        }
        if bytes[7] != 0 || read_u32(bytes, Self::USAGE_OFFSET + 4) != 0 {
            return Err(Errno::OutOfRange);
        }
        let flags = MountFlags::from_bits(read_u32(bytes, 0)).map_err(|_| Errno::OutOfRange)?;
        let source_len = bytes[4];
        let target_len = bytes[5];
        let fstype_len = bytes[6];
        if source_len as usize > MOUNT_SOURCE_MAX
            || target_len as usize > MOUNT_TARGET_MAX
            || fstype_len as usize > MOUNT_FSTYPE_MAX
        {
            return Err(Errno::LengthOutOfRange);
        }
        let usage = VolumeStats {
            block_size: read_u32(bytes, Self::USAGE_OFFSET),
            total_blocks: read_u64(bytes, Self::USAGE_OFFSET + 8),
            free_blocks: read_u64(bytes, Self::USAGE_OFFSET + 16),
            avail_blocks: read_u64(bytes, Self::USAGE_OFFSET + 24),
            files: read_u64(bytes, Self::USAGE_OFFSET + 32),
            files_free: read_u64(bytes, Self::USAGE_OFFSET + 40),
        };
        if usage.avail_blocks > usage.free_blocks || usage.free_blocks > usage.total_blocks {
            return Err(Errno::OutOfRange);
        }
        let mut source = [0u8; MOUNT_SOURCE_MAX];
        let mut target = [0u8; MOUNT_TARGET_MAX];
        let mut fstype = [0u8; MOUNT_FSTYPE_MAX];
        source.copy_from_slice(&bytes[Self::SOURCE_OFFSET..Self::SOURCE_OFFSET + MOUNT_SOURCE_MAX]);
        target.copy_from_slice(&bytes[Self::TARGET_OFFSET..Self::TARGET_OFFSET + MOUNT_TARGET_MAX]);
        fstype.copy_from_slice(&bytes[Self::FSTYPE_OFFSET..Self::FSTYPE_OFFSET + MOUNT_FSTYPE_MAX]);
        Ok(Self {
            flags,
            source_len,
            target_len,
            fstype_len,
            usage,
            source,
            target,
            fstype,
        })
    }
}

/// One row of the [`SysinfoQueryId::RESOURCE_LIMITS`] response: a resource's
/// effective soft/hard bound and the caller's current live usage of it.
///
/// The full response is exactly [`LimitKind::COUNT`] records packed
/// back-to-back in [`LimitKind`] discriminant order — its byte length is
/// [`RESOURCE_LIMITS_REPORT_LEN`] — so a client reads them positionally and
/// the `kind` field is a self-describing cross-check rather than a sort key.
///
/// `usage` is expressed in the resource's natural unit: bytes for the
/// `*Bytes` kinds and a plain count otherwise. It is informational; unlike
/// `limit` it carries no well-formedness invariant.
#[repr(C)]
#[derive(Copy, Clone, Debug, Eq, PartialEq)]
pub struct ResourceLimitRecord {
    /// Which resource this row describes.
    pub kind: LimitKind,
    /// Reserved; must be zero in `sysinfo-v1`.
    pub reserved: u32,
    /// The effective limit (soft/hard) currently in force for the caller.
    pub limit: ResourceLimit,
    /// Current live usage of the resource, in its natural unit (see the
    /// type-level note). Informational; carries no invariant.
    pub usage: u64,
}

impl ResourceLimitRecord {
    /// Encoded size on the wire.
    ///
    /// Layout, little-endian: `kind` (`u32`, offset 0), `reserved` (`u32`,
    /// offset 4), `limit` ([`ResourceLimit`], offset 8), `usage` (`u64`,
    /// offset 24).
    pub const WIRE_LEN: usize = 8 + ResourceLimit::WIRE_LEN + 8;

    /// Construct a record for `kind` with effective `limit` and live `usage`.
    #[must_use]
    pub const fn new(kind: LimitKind, limit: ResourceLimit, usage: u64) -> Self {
        Self {
            kind,
            reserved: 0,
            limit,
            usage,
        }
    }

    /// Encode `self` little-endian.
    #[must_use]
    pub fn to_le_bytes(&self) -> [u8; Self::WIRE_LEN] {
        let mut out = [0u8; Self::WIRE_LEN];
        put_u32(&mut out, 0, self.kind.as_u32());
        put_u32(&mut out, 4, self.reserved);
        out[8..8 + ResourceLimit::WIRE_LEN].copy_from_slice(&self.limit.encode());
        put_u64(&mut out, 24, self.usage);
        out
    }

    /// Decode from `bytes`, failing closed on a malformed record.
    ///
    /// # Errors
    ///
    /// * [`Errno::BufferTooSmall`] if `bytes` is shorter than
    ///   [`WIRE_LEN`](Self::WIRE_LEN).
    /// * [`Errno::BadMagic`] if the reserved field is non-zero (a
    ///   reserved-must-be-zero violation is wire corruption).
    /// * [`Errno::OutOfRange`] if `kind` is not an `abi-v1` [`LimitKind`]
    ///   discriminant, or the embedded [`ResourceLimit`] is not well-formed
    ///   (`soft > hard`).
    pub fn from_bytes(bytes: &[u8]) -> Result<Self, Errno> {
        if bytes.len() < Self::WIRE_LEN {
            return Err(Errno::BufferTooSmall);
        }
        let reserved = read_u32(bytes, 4);
        if reserved != 0 {
            return Err(Errno::BadMagic);
        }
        let kind = LimitKind::from_u32(read_u32(bytes, 0))?;
        let limit = ResourceLimit::decode(&bytes[8..8 + ResourceLimit::WIRE_LEN])?;
        Ok(Self {
            kind,
            reserved,
            limit,
            usage: read_u64(bytes, 24),
        })
    }
}

/// Byte length of a full [`SysinfoQueryId::RESOURCE_LIMITS`] response: one
/// [`ResourceLimitRecord`] per [`LimitKind`], in discriminant order.
pub const RESOURCE_LIMITS_REPORT_LEN: usize = ResourceLimitRecord::WIRE_LEN * LimitKind::COUNT;

#[cfg(test)]
mod tests {
    use super::{
        encoded_query_table, spec_for, CpuTimeListRequest, CpuTimeRecord, KernelMemoryStats,
        LoadAverage, MountListRequest, MountRecord, ProcessListRequest, ProcessRecord,
        ProcessState, ResourceLimitRecord, SeatListRequest, SeatRecord, SysinfoQueryId,
        SysinfoRequestHeader, SystemIdentity, Uptime, UserDirectoryRecord, UserDirectoryRequest,
        VolumeStats, ENCODED_QUERY_TABLE, ENCODED_QUERY_TABLE_LEN, HOSTNAME_MAX, LOAD_FIXED_SHIFT,
        MACHINE_ID_LEN, MOUNT_FSTYPE_MAX, MOUNT_SOURCE_MAX, MOUNT_TARGET_MAX, PROCESS_CPU_NONE,
        PROCESS_NAME_MAX, RESOURCE_LIMITS_REPORT_LEN, SYSINFO_MAX_PAYLOAD_LEN, SYSINFO_QUERIES,
        SYSINFO_QUERY_NAME_MAX, SYSINFO_QUERY_RECORD_LEN, SYSINFO_REQUEST_MAGIC,
        SYSINFO_VERSION_CURRENT, SYSINFO_VERSION_V1, USER_DIRECTORY_NAME_MAX,
    };
    use crate::driver::filesystem::MountFlags;
    use crate::origin::ProcId;
    use crate::rlimit::{LimitKind, ResourceLimit};
    use crate::time::{Duration64, Time64};
    use crate::{CapabilityId, Errno};

    #[test]
    fn well_known_query_ids_are_frozen() {
        // Numeric assignments are part of sysinfo-v1; do not renumber.
        assert_eq!(SysinfoQueryId::SELF_PROCESS_LIST.as_u16(), 0);
        assert_eq!(SysinfoQueryId::GLOBAL_PROCESS_LIST.as_u16(), 1);
        assert_eq!(SysinfoQueryId::KERNEL_MEMORY_STATS.as_u16(), 2);
        assert_eq!(SysinfoQueryId::HARDWARE_TREE.as_u16(), 3);
        assert_eq!(SysinfoQueryId::SYSTEM_IDENTITY.as_u16(), 4);
        assert_eq!(SysinfoQueryId::UPTIME.as_u16(), 5);
        assert_eq!(SysinfoQueryId::MOUNT_LIST.as_u16(), 6);
        assert_eq!(SysinfoQueryId::RESOURCE_LIMITS.as_u16(), 7);
        assert_eq!(SysinfoQueryId::PROCESS_IDENTITY.as_u16(), 8);
        assert_eq!(SysinfoQueryId::SEAT_LIST.as_u16(), 12);
        assert_eq!(SYSINFO_VERSION_CURRENT, SYSINFO_VERSION_V1);
    }

    #[test]
    fn reply_frame_round_trips_ok_and_error() {
        use super::{decode_reply, encode_reply_err, encode_reply_ok, SYSINFO_REPLY_STATUS_LEN};
        // OK frame: zero status word then the payload verbatim.
        let payload = [0xAA, 0xBB, 0xCC];
        let mut buf = [0u8; 16];
        let n = encode_reply_ok(&payload, &mut buf).unwrap();
        assert_eq!(n, SYSINFO_REPLY_STATUS_LEN + payload.len());
        assert_eq!(decode_reply(&buf[..n]), Ok(&payload[..]));

        // Error frame: the errno status word, no payload.
        let n = encode_reply_err(Errno::PermissionDenied, &mut buf).unwrap();
        assert_eq!(n, SYSINFO_REPLY_STATUS_LEN);
        assert_eq!(decode_reply(&buf[..n]), Err(Errno::PermissionDenied));

        // Fail closed: a reply shorter than the status word, and a status
        // word that is not a defined errno.
        assert_eq!(decode_reply(&[0u8; 2]), Err(Errno::BufferTooSmall));
        let mut bogus = [0u8; SYSINFO_REPLY_STATUS_LEN];
        super::put_u32(&mut bogus, 0, 0xFFFF_FFFF);
        assert_eq!(decode_reply(&bogus), Err(Errno::OutOfRange));

        // Encoders fail closed on a short output buffer.
        assert_eq!(
            encode_reply_ok(&payload, &mut [0u8; 2]),
            Err(Errno::BufferTooSmall)
        );
        assert_eq!(
            encode_reply_err(Errno::NotFound, &mut [0u8; 2]),
            Err(Errno::BufferTooSmall)
        );
    }

    #[test]
    fn introspect_domain_round_trips_and_fails_closed() {
        use super::IntrospectDomain;
        // Discriminants are part of abi-v1; do not renumber.
        for (raw, domain) in [
            (0u32, IntrospectDomain::Processes),
            (1, IntrospectDomain::KernelMemory),
            (2, IntrospectDomain::Mounts),
            (3, IntrospectDomain::Identity),
            (4, IntrospectDomain::Uptime),
            (5, IntrospectDomain::TaskLimits),
            (6, IntrospectDomain::LoadAverage),
            (7, IntrospectDomain::UserDirectory),
            (8, IntrospectDomain::CpuTimes),
            (9, IntrospectDomain::Seats),
        ] {
            assert_eq!(domain.as_u32(), raw);
            assert_eq!(IntrospectDomain::from_u32(raw), Ok(domain));
        }
        // Any value outside the closed set is rejected, not guessed.
        assert_eq!(IntrospectDomain::from_u32(10), Err(Errno::OutOfRange));
        assert_eq!(IntrospectDomain::from_u32(u32::MAX), Err(Errno::OutOfRange));
    }

    #[test]
    fn load_average_round_trips_and_renders_fixed_point() {
        let load = LoadAverage {
            load1: (3 << LOAD_FIXED_SHIFT) + (1 << LOAD_FIXED_SHIFT) / 2,
            load5: 1 << LOAD_FIXED_SHIFT,
            load15: 0,
            runnable: 4,
            total_tasks: 17,
            users: 2,
        };
        let decoded = LoadAverage::from_bytes(&load.to_le_bytes()).expect("round trip");
        assert_eq!(decoded, load);
        assert_eq!(LoadAverage::whole(load.load1), 3);
        assert_eq!(LoadAverage::centis(load.load1), 50);
        assert_eq!(LoadAverage::whole(load.load15), 0);
        assert_eq!(LoadAverage::centis(load.load15), 0);
        assert_eq!(
            LoadAverage::from_bytes(&[0u8; LoadAverage::WIRE_LEN - 1]),
            Err(Errno::BufferTooSmall)
        );
    }

    #[test]
    fn user_directory_request_and_record_round_trip_and_fail_closed() {
        let req = UserDirectoryRequest {
            offset: 3,
            limit: 16,
            flags: 0,
        };
        assert_eq!(
            UserDirectoryRequest::from_bytes(&req.to_le_bytes()),
            Ok(req)
        );
        let mut bytes = req.to_le_bytes();
        bytes[6] = 1; // reserved flag set
        assert_eq!(
            UserDirectoryRequest::from_bytes(&bytes),
            Err(Errno::BadMagic)
        );
        assert_eq!(
            UserDirectoryRequest::from_bytes(&[0u8; 4]),
            Err(Errno::BufferTooSmall)
        );

        let rec = UserDirectoryRecord::new(1000, b"alice").expect("record");
        assert_eq!(rec.name_bytes(), b"alice");
        let decoded = UserDirectoryRecord::from_bytes(&rec.to_le_bytes()).expect("round trip");
        assert_eq!(decoded, rec);
        assert_eq!(decoded.uid, 1000);

        let too_long = [b'x'; USER_DIRECTORY_NAME_MAX + 1];
        assert_eq!(
            UserDirectoryRecord::new(0, &too_long),
            Err(Errno::LengthOutOfRange)
        );
        let mut bytes = rec.to_le_bytes();
        bytes[4] = u8::try_from(USER_DIRECTORY_NAME_MAX + 1).unwrap();
        assert_eq!(
            UserDirectoryRecord::from_bytes(&bytes),
            Err(Errno::LengthOutOfRange)
        );
        assert_eq!(
            UserDirectoryRecord::from_bytes(&[0u8; UserDirectoryRecord::WIRE_LEN - 1]),
            Err(Errno::BufferTooSmall)
        );
    }

    #[test]
    fn query_id_from_raw_enforces_bounds() {
        assert_eq!(
            SysinfoQueryId::from_raw(SysinfoQueryId::MAX).map(SysinfoQueryId::as_u16),
            Ok(1023)
        );
        assert_eq!(
            SysinfoQueryId::from_raw(SysinfoQueryId::MAX + 1),
            Err(Errno::OutOfRange)
        );
    }

    #[test]
    fn registry_is_dense_and_ordered() {
        for (idx, spec) in SYSINFO_QUERIES.iter().enumerate() {
            assert_eq!(spec.id.as_u16() as usize, idx, "{}", spec.name);
            assert!(spec.name.is_ascii(), "{} non-ASCII name", spec.name);
            assert!(
                spec.name.len() <= SYSINFO_QUERY_NAME_MAX,
                "{} exceeds name max",
                spec.name
            );
        }
    }

    #[test]
    fn capability_gates_are_frozen() {
        // Self-scoped observers are ungated; cross-principal / kernel /
        // hardware queries each carry their declared capability.
        assert_eq!(
            spec_for(SysinfoQueryId::SELF_PROCESS_LIST)
                .unwrap()
                .required_capability,
            None
        );
        assert_eq!(
            spec_for(SysinfoQueryId::GLOBAL_PROCESS_LIST)
                .unwrap()
                .required_capability,
            Some(CapabilityId::SYSINFO_GLOBAL)
        );
        assert_eq!(
            spec_for(SysinfoQueryId::KERNEL_MEMORY_STATS)
                .unwrap()
                .required_capability,
            Some(CapabilityId::SYSINFO_KERNEL)
        );
        assert_eq!(
            spec_for(SysinfoQueryId::HARDWARE_TREE)
                .unwrap()
                .required_capability,
            Some(CapabilityId::SYSINFO_HW)
        );
        // The mount table is system-wide but secret-free, so it is ungated
        // like uptime and identity.
        assert_eq!(
            spec_for(SysinfoQueryId::MOUNT_LIST)
                .unwrap()
                .required_capability,
            None
        );
        // Privileged queries are audited; self-scoped observers are not.
        assert!(spec_for(SysinfoQueryId::GLOBAL_PROCESS_LIST).unwrap().audit);
        assert!(!spec_for(SysinfoQueryId::SELF_PROCESS_LIST).unwrap().audit);
        assert!(!spec_for(SysinfoQueryId::UPTIME).unwrap().audit);
        assert!(!spec_for(SysinfoQueryId::MOUNT_LIST).unwrap().audit);
        // A principal reads its own limits + usage; self-scoped, so ungated
        // and unaudited like the other self-scoped observers.
        assert_eq!(
            spec_for(SysinfoQueryId::RESOURCE_LIMITS)
                .unwrap()
                .required_capability,
            None
        );
        assert!(!spec_for(SysinfoQueryId::RESOURCE_LIMITS).unwrap().audit);
        // A principal reads its own attested origin; self-scoped, so ungated
        // and unaudited.
        assert_eq!(
            spec_for(SysinfoQueryId::PROCESS_IDENTITY)
                .unwrap()
                .required_capability,
            None
        );
        assert!(!spec_for(SysinfoQueryId::PROCESS_IDENTITY).unwrap().audit);
        // The seat inventory names which task owns each display: gated like
        // the hardware tree, and audited.
        assert_eq!(
            spec_for(SysinfoQueryId::SEAT_LIST)
                .unwrap()
                .required_capability,
            Some(CapabilityId::SYSINFO_HW)
        );
        assert!(spec_for(SysinfoQueryId::SEAT_LIST).unwrap().audit);
    }

    #[test]
    fn seat_list_request_round_trips_and_rejects_reserved() {
        let req = SeatListRequest {
            offset: 1,
            limit: 4,
            flags: 0,
        };
        assert_eq!(SeatListRequest::from_bytes(&req.to_le_bytes()), Ok(req));
        let mut bytes = req.to_le_bytes();
        bytes[6] = 1;
        assert_eq!(SeatListRequest::from_bytes(&bytes), Err(Errno::BadMagic));
        assert_eq!(
            SeatListRequest::from_bytes(&[0u8; SeatListRequest::WIRE_LEN - 1]),
            Err(Errno::BufferTooSmall)
        );
    }

    #[test]
    fn seat_record_round_trips_owned_and_unowned() {
        let held = SeatRecord {
            seat_id: 0,
            owner_task: 7,
            generation: 3,
            foreground_console: 1,
            flags: super::SEAT_FLAG_OWNED,
        };
        assert_eq!(SeatRecord::WIRE_LEN, 32);
        assert_eq!(SeatRecord::from_bytes(&held.to_le_bytes()), Ok(held));
        assert!(held.owned());
        assert_eq!(held.owner(), Some(7));

        let unowned = SeatRecord {
            seat_id: 0,
            owner_task: 0,
            generation: 4,
            foreground_console: 0,
            flags: 0,
        };
        assert_eq!(SeatRecord::from_bytes(&unowned.to_le_bytes()), Ok(unowned));
        assert!(!unowned.owned());
        assert_eq!(unowned.owner(), None);
    }

    #[test]
    fn seat_record_fails_closed_on_corrupt_wire() {
        let good = SeatRecord {
            seat_id: 0,
            owner_task: 7,
            generation: 1,
            foreground_console: 0,
            flags: super::SEAT_FLAG_OWNED,
        }
        .to_le_bytes();
        // A reserved flag bit is wire corruption.
        let mut reserved = good;
        reserved[29] = 0x80;
        assert_eq!(SeatRecord::from_bytes(&reserved), Err(Errno::BadMagic));
        // An unowned record must not smuggle an owner.
        let mut phantom_owner = good;
        phantom_owner[28] = 0;
        assert_eq!(SeatRecord::from_bytes(&phantom_owner), Err(Errno::BadMagic));
        // Short buffer.
        assert_eq!(
            SeatRecord::from_bytes(&good[..SeatRecord::WIRE_LEN - 1]),
            Err(Errno::BufferTooSmall)
        );
    }

    #[test]
    fn resource_limit_record_round_trips() {
        let limit = ResourceLimit::new(4096, 1 << 20).expect("well-formed");
        let rec = ResourceLimitRecord::new(LimitKind::AddressSpaceBytes, limit, 2048);
        assert_eq!(ResourceLimitRecord::WIRE_LEN, 32);
        assert_eq!(RESOURCE_LIMITS_REPORT_LEN, 32 * LimitKind::COUNT);
        let bytes = rec.to_le_bytes();
        assert_eq!(bytes.len(), ResourceLimitRecord::WIRE_LEN);
        assert_eq!(ResourceLimitRecord::from_bytes(&bytes), Ok(rec));
    }

    #[test]
    fn resource_limit_record_fails_closed() {
        let rec = ResourceLimitRecord::new(LimitKind::Processes, ResourceLimit::UNLIMITED, 7);
        let good = rec.to_le_bytes();
        // Short buffer.
        assert_eq!(
            ResourceLimitRecord::from_bytes(&good[..ResourceLimitRecord::WIRE_LEN - 1]),
            Err(Errno::BufferTooSmall)
        );
        // Non-zero reserved word is wire corruption.
        let mut reserved = good;
        reserved[4] = 1;
        assert_eq!(
            ResourceLimitRecord::from_bytes(&reserved),
            Err(Errno::BadMagic)
        );
        // Unassigned LimitKind discriminant.
        let mut bad_kind = good;
        bad_kind[0] = 0xFF;
        assert_eq!(
            ResourceLimitRecord::from_bytes(&bad_kind),
            Err(Errno::OutOfRange)
        );
        // Malformed embedded limit (soft > hard).
        let mut bad_limit = good;
        bad_limit[8] = 10; // soft low byte
        bad_limit[16] = 5; // hard low byte
        assert_eq!(
            ResourceLimitRecord::from_bytes(&bad_limit),
            Err(Errno::OutOfRange)
        );
    }

    #[test]
    fn spec_for_rejects_unassigned_id() {
        let past = SysinfoQueryId::from_raw(u16::try_from(SYSINFO_QUERIES.len()).unwrap()).unwrap();
        assert!(spec_for(past).is_none());
    }

    #[test]
    fn encoded_table_length_and_first_record() {
        assert_eq!(
            ENCODED_QUERY_TABLE_LEN,
            SYSINFO_QUERY_RECORD_LEN * SYSINFO_QUERIES.len()
        );
        assert_eq!(encoded_query_table().len(), ENCODED_QUERY_TABLE_LEN);
        // First record: id 0, no capability, no audit, name padded.
        let rec = &ENCODED_QUERY_TABLE[..SYSINFO_QUERY_RECORD_LEN];
        assert_eq!(&rec[0..2], &[0, 0]);
        assert_eq!(rec[2], 0); // capability absent
        assert_eq!(&rec[3..5], &[0, 0]);
        assert_eq!(rec[5], 0); // audit off
        assert_eq!(
            &rec[6..6 + b"self_process_list".len()],
            b"self_process_list"
        );
    }

    #[test]
    fn encoded_table_records_capability_and_audit() {
        let idx = SysinfoQueryId::GLOBAL_PROCESS_LIST.as_u16() as usize;
        let base = idx * SYSINFO_QUERY_RECORD_LEN;
        let rec = &ENCODED_QUERY_TABLE[base..base + SYSINFO_QUERY_RECORD_LEN];
        assert_eq!(rec[2], 1, "capability present flag");
        let cap = u16::from_le_bytes([rec[3], rec[4]]);
        assert_eq!(cap, CapabilityId::SYSINFO_GLOBAL.as_u16());
        assert_eq!(rec[5], 1, "audit flag");
    }

    fn sample_header() -> SysinfoRequestHeader {
        SysinfoRequestHeader {
            magic: SYSINFO_REQUEST_MAGIC,
            version: SYSINFO_VERSION_CURRENT,
            flags: 0,
            query: SysinfoQueryId::GLOBAL_PROCESS_LIST,
            reserved: 0,
            payload_len: u32::try_from(ProcessListRequest::WIRE_LEN).unwrap(),
            request_id: 0xDEAD_BEEF_0000_0001,
        }
    }

    #[test]
    fn request_header_round_trips() {
        let h = sample_header();
        assert_eq!(SysinfoRequestHeader::WIRE_LEN, 24);
        let bytes = h.to_le_bytes();
        assert_eq!(SysinfoRequestHeader::from_bytes(&bytes), Ok(h));
    }

    #[test]
    fn request_header_rejection_paths() {
        assert_eq!(
            SysinfoRequestHeader::from_bytes(&[0u8; 8]),
            Err(Errno::BufferTooSmall)
        );
        let mut bytes = sample_header().to_le_bytes();
        bytes[0] ^= 0xFF;
        assert_eq!(
            SysinfoRequestHeader::from_bytes(&bytes),
            Err(Errno::BadMagic)
        );

        let mut header = sample_header();
        header.version = 99;
        assert_eq!(
            SysinfoRequestHeader::from_bytes(&header.to_le_bytes()),
            Err(Errno::AbiVersionUnsupported)
        );

        // Query id out of range: write a raw oversize id into the buffer.
        let mut bytes = sample_header().to_le_bytes();
        bytes[8] = 0xFF;
        bytes[9] = 0xFF;
        assert_eq!(
            SysinfoRequestHeader::from_bytes(&bytes),
            Err(Errno::OutOfRange)
        );

        let mut header = sample_header();
        header.reserved = 1;
        assert_eq!(
            SysinfoRequestHeader::from_bytes(&header.to_le_bytes()),
            Err(Errno::BadMagic)
        );

        let mut header = sample_header();
        header.payload_len = SYSINFO_MAX_PAYLOAD_LEN + 1;
        assert_eq!(
            SysinfoRequestHeader::from_bytes(&header.to_le_bytes()),
            Err(Errno::LengthOutOfRange)
        );
    }

    #[test]
    fn process_list_request_round_trips_and_rejects_reserved() {
        let req = ProcessListRequest {
            offset: 10,
            limit: 64,
            flags: 0,
        };
        assert_eq!(ProcessListRequest::from_bytes(&req.to_le_bytes()), Ok(req));
        let mut bytes = req.to_le_bytes();
        bytes[6] = 1; // reserved flag set
        assert_eq!(ProcessListRequest::from_bytes(&bytes), Err(Errno::BadMagic));
        assert_eq!(
            ProcessListRequest::from_bytes(&[0u8; 4]),
            Err(Errno::BufferTooSmall)
        );
    }

    #[test]
    fn process_state_round_trips_and_rejects_unknown() {
        for s in [
            ProcessState::Runnable,
            ProcessState::Running,
            ProcessState::Blocked,
            ProcessState::Zombie,
            ProcessState::Stopped,
        ] {
            assert_eq!(ProcessState::from_u8(s.as_u8()), Ok(s));
        }
        assert_eq!(ProcessState::from_u8(5), Err(Errno::OutOfRange));
    }

    #[test]
    fn process_record_round_trips() {
        let rec = ProcessRecord::new(
            7,
            1,
            ProcId::from_raw([0x11; 16]),
            ProcId::from_raw([0x22; 16]),
            1000,
            1000,
            ProcessState::Running,
            2,
            1_234_567_890,
            5 * 4096,
            b"init",
        )
        .unwrap();
        assert_eq!(rec.name_bytes(), b"init");
        let decoded = ProcessRecord::from_bytes(&rec.to_le_bytes()).unwrap();
        assert_eq!(decoded, rec);
        assert_eq!(decoded.name_bytes(), b"init");
        assert_eq!(decoded.proc_id, ProcId::from_raw([0x11; 16]));
        assert_eq!(decoded.parent_proc_id, ProcId::from_raw([0x22; 16]));
        assert_eq!(decoded.cpu_time_ns, 1_234_567_890);
        assert_eq!(decoded.mem_bytes, 5 * 4096);
    }

    #[test]
    fn process_record_rejects_overlong_name_and_bad_state() {
        let too_long = [b'x'; PROCESS_NAME_MAX + 1];
        assert_eq!(
            ProcessRecord::new(
                1,
                0,
                ProcId::KERNEL,
                ProcId::KERNEL,
                0,
                0,
                ProcessState::Runnable,
                PROCESS_CPU_NONE,
                0,
                0,
                &too_long,
            ),
            Err(Errno::LengthOutOfRange)
        );

        let mut bytes = ProcessRecord::new(
            1,
            0,
            ProcId::KERNEL,
            ProcId::KERNEL,
            0,
            0,
            ProcessState::Runnable,
            PROCESS_CPU_NONE,
            0,
            0,
            b"a",
        )
        .unwrap()
        .to_le_bytes();
        bytes[56] = 0xFF; // invalid state discriminant
        assert_eq!(ProcessRecord::from_bytes(&bytes), Err(Errno::OutOfRange));

        let mut bytes = ProcessRecord::new(
            1,
            0,
            ProcId::KERNEL,
            ProcId::KERNEL,
            0,
            0,
            ProcessState::Runnable,
            PROCESS_CPU_NONE,
            0,
            0,
            b"a",
        )
        .unwrap()
        .to_le_bytes();
        bytes[58] = u8::try_from(PROCESS_NAME_MAX + 1).unwrap(); // name_len out of range
        assert_eq!(
            ProcessRecord::from_bytes(&bytes),
            Err(Errno::LengthOutOfRange)
        );
    }

    #[test]
    fn kernel_memory_stats_round_trips_and_rejects_reserved() {
        let stats = KernelMemoryStats {
            total_bytes: 1 << 32,
            free_bytes: 1 << 30,
            kernel_heap_bytes: 4096,
            user_resident_bytes: 1 << 20,
            page_size: 4096,
            reserved: 0,
        };
        assert_eq!(KernelMemoryStats::WIRE_LEN, 40);
        assert_eq!(
            KernelMemoryStats::from_bytes(&stats.to_le_bytes()),
            Ok(stats)
        );
        let mut bytes = stats.to_le_bytes();
        bytes[36] = 1; // reserved non-zero
        assert_eq!(KernelMemoryStats::from_bytes(&bytes), Err(Errno::BadMagic));
    }

    #[test]
    fn uptime_round_trips() {
        let up = Uptime {
            since_boot: Duration64::from_nanos(123_456_789),
            boot_time: Time64::from_secs(1_700_000_000),
        };
        assert_eq!(Uptime::WIRE_LEN, 24);
        assert_eq!(Uptime::from_bytes(&up.to_le_bytes()), Ok(up));
        assert_eq!(Uptime::from_bytes(&[0u8; 4]), Err(Errno::BufferTooSmall));
    }

    #[test]
    fn system_identity_round_trips() {
        let machine_id = [7u8; MACHINE_ID_LEN];
        let id = SystemIdentity::new(machine_id, 1, 2, 3, b"rustos-box").unwrap();
        assert_eq!(id.hostname_bytes(), b"rustos-box");
        // Decode tolerates a buffer longer than WIRE_LEN.
        let mut bytes = [0xAAu8; SystemIdentity::WIRE_LEN + 9];
        bytes[..SystemIdentity::WIRE_LEN].copy_from_slice(&id.to_le_bytes());
        let decoded = SystemIdentity::from_bytes(&bytes).unwrap();
        assert_eq!(decoded, id);
        assert_eq!(decoded.hostname_bytes(), b"rustos-box");
    }

    #[test]
    fn cpu_time_list_request_round_trips_and_rejects_reserved() {
        let req = CpuTimeListRequest {
            offset: 2,
            limit: 64,
            flags: 0,
        };
        assert_eq!(CpuTimeListRequest::WIRE_LEN, 8);
        assert_eq!(CpuTimeListRequest::from_bytes(&req.to_le_bytes()), Ok(req));
        let mut bytes = req.to_le_bytes();
        bytes[6] = 1;
        assert_eq!(CpuTimeListRequest::from_bytes(&bytes), Err(Errno::BadMagic));
        assert_eq!(
            CpuTimeListRequest::from_bytes(&[0u8; 4]),
            Err(Errno::BufferTooSmall)
        );
    }

    #[test]
    fn cpu_time_record_round_trips_and_rejects_reserved() {
        let record = CpuTimeRecord {
            cpu: 3,
            reserved: 0,
            busy_ns: 5_000_000_007,
            idle_ns: 12_345_678_901,
        };
        assert_eq!(CpuTimeRecord::WIRE_LEN, 24);
        assert_eq!(CpuTimeRecord::from_bytes(&record.to_le_bytes()), Ok(record));
        let mut bytes = record.to_le_bytes();
        bytes[4] = 1;
        assert_eq!(CpuTimeRecord::from_bytes(&bytes), Err(Errno::BadMagic));
        assert_eq!(
            CpuTimeRecord::from_bytes(&[0u8; 8]),
            Err(Errno::BufferTooSmall)
        );
    }

    #[test]
    fn mount_list_request_round_trips_and_rejects_reserved() {
        let req = MountListRequest {
            offset: 3,
            limit: 32,
            flags: 0,
        };
        assert_eq!(MountListRequest::WIRE_LEN, 8);
        assert_eq!(MountListRequest::from_bytes(&req.to_le_bytes()), Ok(req));
        let mut bytes = req.to_le_bytes();
        bytes[6] = 1; // reserved flag set
        assert_eq!(MountListRequest::from_bytes(&bytes), Err(Errno::BadMagic));
        assert_eq!(
            MountListRequest::from_bytes(&[0u8; 4]),
            Err(Errno::BufferTooSmall)
        );
    }

    #[test]
    fn mount_record_round_trips() {
        let flags = MountFlags::READ_ONLY
            .union(MountFlags::NOSUID)
            .union(MountFlags::NODEV)
            .union(MountFlags::NOEXEC);
        let usage = VolumeStats {
            block_size: 4096,
            total_blocks: 1 << 40,
            free_blocks: 1 << 39,
            avail_blocks: (1 << 39) - 32,
            files: 0,
            files_free: 0,
        };
        let rec =
            MountRecord::new(b"/Storage/data", b"/Storage/data", b"rustfs", flags, usage).unwrap();
        assert_eq!(rec.source_bytes(), b"/Storage/data");
        assert_eq!(rec.target_bytes(), b"/Storage/data");
        assert_eq!(rec.fstype_bytes(), b"rustfs");
        assert_eq!(rec.flags(), flags);
        assert_eq!(rec.usage(), usage);
        let decoded = MountRecord::from_bytes(&rec.to_le_bytes()).unwrap();
        assert_eq!(decoded, rec);
    }

    #[test]
    fn mount_record_rejects_overlong_fields() {
        let long_source = [b's'; MOUNT_SOURCE_MAX + 1];
        assert_eq!(
            MountRecord::new(
                &long_source,
                b"/",
                b"rustfs",
                MountFlags::default(),
                VolumeStats::default()
            ),
            Err(Errno::LengthOutOfRange)
        );
        let long_target = [b't'; MOUNT_TARGET_MAX + 1];
        assert_eq!(
            MountRecord::new(
                b"src",
                &long_target,
                b"rustfs",
                MountFlags::default(),
                VolumeStats::default()
            ),
            Err(Errno::LengthOutOfRange)
        );
        let long_type = [b'x'; MOUNT_FSTYPE_MAX + 1];
        assert_eq!(
            MountRecord::new(
                b"src",
                b"/",
                &long_type,
                MountFlags::default(),
                VolumeStats::default()
            ),
            Err(Errno::LengthOutOfRange)
        );
    }

    #[test]
    fn mount_record_rejects_inconsistent_usage() {
        // Free exceeding total is refused at construction…
        let free_over_total = VolumeStats {
            block_size: 512,
            total_blocks: 10,
            free_blocks: 11,
            avail_blocks: 0,
            files: 0,
            files_free: 0,
        };
        assert_eq!(
            MountRecord::new(
                b"src",
                b"/",
                b"rustfs",
                MountFlags::default(),
                free_over_total
            ),
            Err(Errno::OutOfRange)
        );
        // …as is available exceeding free…
        let avail_over_free = VolumeStats {
            block_size: 512,
            total_blocks: 10,
            free_blocks: 5,
            avail_blocks: 6,
            files: 0,
            files_free: 0,
        };
        assert_eq!(
            MountRecord::new(
                b"src",
                b"/",
                b"rustfs",
                MountFlags::default(),
                avail_over_free
            ),
            Err(Errno::OutOfRange)
        );
        // …and a hostile wire image claiming either is refused whole.
        let ok = MountRecord::new(
            b"src",
            b"/",
            b"rustfs",
            MountFlags::default(),
            VolumeStats::default(),
        )
        .unwrap();
        let mut bytes = ok.to_le_bytes();
        // free_blocks = 1 while total_blocks stays 0.
        bytes[24] = 1;
        assert_eq!(MountRecord::from_bytes(&bytes), Err(Errno::OutOfRange));
        // The reserved usage pad must be zero.
        let mut bytes = ok.to_le_bytes();
        bytes[12] = 1;
        assert_eq!(MountRecord::from_bytes(&bytes), Err(Errno::OutOfRange));
    }

    #[test]
    fn mount_record_rejects_corrupt_wire() {
        let rec = MountRecord::new(
            b"src",
            b"/",
            b"rustfs",
            MountFlags::default(),
            VolumeStats::default(),
        )
        .unwrap();
        assert_eq!(
            MountRecord::from_bytes(&[0u8; 8]),
            Err(Errno::BufferTooSmall)
        );
        // Unknown flag bit outside KNOWN_MASK.
        let mut bytes = rec.to_le_bytes();
        bytes[0] = 0xFF;
        assert_eq!(MountRecord::from_bytes(&bytes), Err(Errno::OutOfRange));
        // Reserved pad byte non-zero.
        let mut bytes = rec.to_le_bytes();
        bytes[7] = 1;
        assert_eq!(MountRecord::from_bytes(&bytes), Err(Errno::OutOfRange));
        // A length byte beyond its buffer.
        let mut bytes = rec.to_le_bytes();
        bytes[4] = u8::try_from(MOUNT_SOURCE_MAX + 1).unwrap();
        assert_eq!(
            MountRecord::from_bytes(&bytes),
            Err(Errno::LengthOutOfRange)
        );
    }

    #[test]
    fn system_identity_rejects_overlong_hostname_and_short_buffer() {
        let too_long = [b'h'; HOSTNAME_MAX + 1];
        assert_eq!(
            SystemIdentity::new([0u8; MACHINE_ID_LEN], 0, 0, 0, &too_long),
            Err(Errno::LengthOutOfRange)
        );
        let mut bytes = SystemIdentity::new([0u8; MACHINE_ID_LEN], 0, 0, 0, b"h")
            .unwrap()
            .to_le_bytes();
        bytes[MACHINE_ID_LEN + 6] = u8::try_from(HOSTNAME_MAX + 1).unwrap();
        assert_eq!(
            SystemIdentity::from_bytes(&bytes),
            Err(Errno::LengthOutOfRange)
        );
        assert_eq!(
            SystemIdentity::from_bytes(&[0u8; 8]),
            Err(Errno::BufferTooSmall)
        );
    }
}
