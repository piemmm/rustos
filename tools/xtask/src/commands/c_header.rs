//! `cargo xtask c-header` implementation.
//!
//! TAIRiX is written entirely in Rust, but its kernel/user interface
//! (`abi-v1`) is a stable binary contract that programs
//! written in other languages — C in particular — must be able to call.
//! Those programs need a C-language *view* of the ABI: the syscall numbers,
//! the error codes, the capability identifiers, the `#[repr(C)]` types, and a
//! prototype for each syscall entry point.
//!
//! That view is the C development header set. It is **generated** from the one
//! source of truth in `lib/abi` (no duplication — the
//! ABI is versioned and a C surface is a view of the existing definition,
//! never a hand-maintained parallel one). The committed headers live in their
//! own top-level `include/` folder so they can be handed to developers
//! building non-Rust programs without shipping the whole workspace.
//!
//! The surface is split into **one header per `lib/abi` module** under
//! `include/tairix/` (`tairix_error.h`, `tairix_capability.h`,
//! `tairix_time.h`, `tairix_syscall.h`, …) plus the umbrella `tairix_abi.h`
//! (in [`DEFAULT_INCLUDE_DIR`]) that `#include`s them all, so a developer can
//! pull in exactly what they need (`plans/CCOMPAT.md` CC1).
//!
//! Like `abi-check` (`commands/abi_check.rs`), the generator doubles as a
//! drift guard:
//!
//! - `cargo xtask c-header` (no arguments) regenerates every header in memory
//!   and compares each byte for byte with the committed copy, failing closed
//!   on any mismatch. It runs as part of `cargo xtask ci`.
//! - `cargo xtask c-header --write` regenerates the committed copies (reviewed
//!   by diff, exactly like the kernel syscall table the abi-check watches).
//!
//! ## Stable export-symbol convention
//!
//! Each syscall is exposed to C as a function named `tairix_sys_<name>`
//! (for example `tairix_sys_ipc_send`). The names use the short `tairix_` /
//! `TAIRIX_` C-ABI prefix and are namespaced and frozen
//! alongside the rest of `abi-v1`. The future user-space stub crate that
//! issues the actual trap implements each one with an explicit
//! `#[export_name = "tairix_sys_<name>"]` so the Rust compiler does not
//! mangle it; this header is the contract those exports satisfy.

use std::path::Path;

use tairix_abi::blkio::BlkDeviceClass;
use tairix_abi::field::{
    TAG_BOOL, TAG_BYTES, TAG_CAP, TAG_DECIMAL, TAG_DURATION, TAG_ERROR, TAG_IP, TAG_LIST, TAG_MAC,
    TAG_NULL, TAG_SIGNED, TAG_STR, TAG_TIME, TAG_UNSIGNED, TAG_UUID,
};
use tairix_abi::sysinfo::SYSINFO_QUERIES;
use tairix_abi::{
    AbiType, AppInfoHeader, BufferClass, BundleEntry, CallRecvFlags, CapabilityId, DriverBindKey,
    DriverError, DriverHandle, DriverKind, DriverManifest, DriverRegisterReply, Duration64, Errno,
    HwDeviceClass, HwMatchKey, HwMatchKind, HwNode, HwResource, HwResourceKind, IpcMessageHeader,
    KernelMemoryStats, KeyInput, LibraryCategory, LibraryScope, LimitKind, LinkFlags, LoadAverage,
    LoadHeader, ManifestHeader, MapFlags, MountAvailability, MountListRequest, MountRecord,
    NamedKeyCode, NeededLibrary, OpenFlags, PointerButtonCode, PointerInput, PortName, PowerAction,
    ProcessListRequest, ProcessRecord, ProcessStartHeader, ProcessState, RandomFlags, RealpathMode,
    ResourceLimit, ResourceLimitRecord, RxePermission, SchedPriority, Segment, Severity, Signal,
    SignalIntakeOp, StdInfoKind, StringSlot, SysinfoQueryId, SysinfoRequestHeader, SystemIdentity,
    Time64, UnlinkFlags, Uptime, UserDirectoryRecord, UserDirectoryRequest, WaitFlags, WaitSetOp,
    WaitSourceKind, ABI_VERSION_V1, APPINFO_MAGIC, APPINFO_MAX_CAPABILITIES, APPINFO_MAX_MIME,
    BUNDLE_AUTHOR_MAX, BUNDLE_ID_MAX, BUNDLE_NAME_MAX, BUNDLE_PURPOSE_MAX, BUNDLE_VERSION_MAX,
    BUTTON_NONE, CAPABILITY_ID_MAX, COARSE_CLOCK_GRANULARITY_NS, CONSOLE_INHERIT,
    DRIVER_MANIFEST_MAGIC, DRIVER_MANIFEST_MAX_BIND_KEYS, DRIVER_MANIFEST_MAX_CAPABILITIES,
    DRIVER_REGISTER_REPLY_MAGIC, DRIVER_REGISTER_STATUS_OK, DRIVER_SIGNATURE_LEN,
    DRIVER_SIGNER_PUBKEY_LEN, ENCODED_QUERY_TABLE_LEN, FS_ATTR_KEY_MAX, FS_ATTR_VALUE_MAX,
    FS_MODE_MASK, HOSTNAME_MAX, HWTREE_VERSION_V1, HW_COMPATIBLE_MAX, HW_NODE_HEADER_LEN,
    HW_NODE_MAX_MATCH_KEYS, HW_NODE_MAX_RESOURCES, HW_NODE_ROOT, IPC_MESSAGE_HEADER_MAGIC,
    KEY_CLASS_CHAR, KEY_CLASS_NAMED, KEY_INPUT_MAGIC, KIND_KEY_PRESSED, KIND_KEY_RELEASED,
    KIND_MOVED_BY, KIND_PRESSED, KIND_RELEASED, KIND_SCROLLED, LIBRARY_ICON_MAX, LIBREF_MAX,
    LOAD_FLAG_PIE, LOAD_MAGIC, LOAD_MAX_NEEDED, LOAD_MAX_SEGMENTS, LOG_FIELDS_MAX,
    LOG_FIELDS_PAYLOAD_MAX, LOG_FIELD_KEY_MAX, LOG_FIELD_VALUE_MAX, LOG_LEVEL_MAX, LOG_MESSAGE_MAX,
    LOG_RECORD_HEADER_LEN, LOG_RECORD_MAX, MACHINE_ID_LEN, MANIFEST_MAGIC,
    MANIFEST_MAX_CAPABILITIES, MIME_ENTRY_LEN, MIME_TYPE_MAX, MOD_ALT, MOD_CTRL, MOD_MASK,
    MOD_META, MOD_SHIFT, MOUNT_FSTYPE_MAX, MOUNT_SOURCE_MAX, MOUNT_TARGET_MAX, MOUNT_VOLUME_ID_LEN,
    NANOS_PER_SEC, PAGE_SIZE, PLAUSIBLE_FUTURE_SECS, POINTER_INPUT_MAGIC, PORT_NAME_MAX_LEN,
    PROCESS_CPU_NONE, PROCESS_NAME_MAX, PROCESS_START_MAGIC, PROCESS_START_MAX_STRINGS,
    PROCESS_START_MAX_STRING_LEN, PROCESS_START_MAX_TOTAL_LEN, RANDOM_REQUEST_MAX_BYTES,
    RANDOM_RESERVE_DEFAULT_BYTES, RELEASE_EPOCH_SECS, RESOURCE_LIMITS_REPORT_LEN, RLIMIT_INFINITY,
    RXE_PAGE_SIZE, SEG_FLAG_EXEC, SEG_FLAG_READ, SEG_FLAG_WRITE, SPAWN_UID_INHERIT, STDINFO_FD,
    STDINFO_VERSION_CURRENT, STDINFO_VERSION_V1, SYSCALLS, SYSCALL_MAX_ARGS,
    SYSCALL_TABLE_HASH_LEN, SYSINFO_MAX_PAYLOAD_LEN, SYSINFO_QUERY_NAME_MAX,
    SYSINFO_QUERY_RECORD_LEN, SYSINFO_REQUEST_MAGIC, SYSINFO_VERSION_CURRENT, SYSINFO_VERSION_V1,
    SYSTEM_LIBRARIES_DIR, THREAD_STACK_DEFAULT, USER_DIRECTORY_NAME_MAX,
};

/// Default on-disk location of the generated C ABI header set, relative to
/// the workspace root. The umbrella header is `tairix_abi.h` inside it.
pub const DEFAULT_INCLUDE_DIR: &str = "include/tairix";

/// The `abi-v1` error codes, paired with the `TAIRIX_E_*` suffix each is
/// emitted under.
///
/// The numeric value of every entry is read straight from the
/// [`Errno`] enum, so this table can never disagree with the frozen
/// discriminants: only the C spelling lives here, because
/// Rust offers no way to enumerate an enum's variants at run time. The
/// in-module `errno_table_matches_the_frozen_enum` test pins the dense
/// `1..=N` numbering *and* that no `Errno` discriminant exists past the last
/// entry, so a newly appended variant fails the test instead of being
/// silently dropped from the C view.
const ERRNO_NAMES: &[(&str, Errno)] = &[
    ("BUFFER_TOO_SMALL", Errno::BufferTooSmall),
    ("BAD_ALIGNMENT", Errno::BadAlignment),
    ("BAD_MAGIC", Errno::BadMagic),
    ("LENGTH_OUT_OF_RANGE", Errno::LengthOutOfRange),
    ("OUT_OF_RANGE", Errno::OutOfRange),
    ("PERMISSION_DENIED", Errno::PermissionDenied),
    ("NOT_FOUND", Errno::NotFound),
    ("DELEGATION_WIDEN", Errno::DelegationWiden),
    ("SIGNATURE_INVALID", Errno::SignatureInvalid),
    ("ABI_VERSION_UNSUPPORTED", Errno::AbiVersionUnsupported),
    ("MESSAGE_TOO_LARGE", Errno::MessageTooLarge),
    ("NOT_IMPLEMENTED", Errno::NotImplemented),
    ("TIMED_OUT", Errno::TimedOut),
    ("TIMESTAMP_OUT_OF_RANGE", Errno::TimestampOutOfRange),
    ("NO_SPACE", Errno::NoSpace),
    ("ENTROPY_NOT_READY", Errno::EntropyNotReady),
    ("ALREADY_EXISTS", Errno::AlreadyExists),
    ("BAD_ADDRESS", Errno::BadAddress),
    ("WOULD_BLOCK", Errno::WouldBlock),
    ("OUT_OF_MEMORY", Errno::OutOfMemory),
    ("CROSS_VOLUME", Errno::CrossVolume),
    ("NOT_A_DIRECTORY", Errno::NotADirectory),
    ("NOT_EMPTY", Errno::NotEmpty),
    ("SEAT_BUSY", Errno::SeatBusy),
    ("SEAT_NOT_OWNER", Errno::SeatNotOwner),
    ("SEAT_REVOKED", Errno::SeatRevoked),
    ("NOT_FOREGROUND", Errno::NotForeground),
    ("BROKEN_PIPE", Errno::BrokenPipe),
    ("ENDPOINT_STALLED", Errno::EndpointStalled),
    ("DEVICE_FAULT", Errno::DeviceFault),
    ("NO_DATA", Errno::NoData),
    ("NOT_SUPPORTED", Errno::NotSupported),
    ("INTERRUPTED", Errno::Interrupted),
    ("ADDRESS_IN_USE", Errno::AddressInUse),
    ("ADDRESS_UNAVAILABLE", Errno::AddressUnavailable),
    ("NETWORK_UNREACHABLE", Errno::NetworkUnreachable),
    ("NOT_CONNECTED", Errno::NotConnected),
    ("LIMIT_EXCEEDED", Errno::LimitExceeded),
    ("MEDIUM_ERROR", Errno::MediumError),
    ("DEVICE_OFFLINE", Errno::DeviceOffline),
    ("BUSY", Errno::Busy),
    ("LINK_LOOP", Errno::LinkLoop),
    ("IS_A_DIRECTORY", Errno::IsADirectory),
    ("TOO_MANY_LINKS", Errno::TooManyLinks),
    ("NOT_ATTACHED", Errno::NotAttached),
];

/// The `abi-v1` driver-ABI error codes, paired with the
/// `TAIRIX_DRIVER_ERROR_*` suffix each is emitted under.
///
/// Numbering is read from [`DriverError`] exactly as [`ERRNO_NAMES`] reads
/// it from [`Errno`], and the in-module `driver_error_table_matches_the_enum`
/// test pins the dense `1..=N` numbering *and* that no discriminant exists
/// past the last entry — a newly appended variant fails the test instead of
/// being silently dropped from the C view.
const DRIVER_ERROR_NAMES: &[(&str, DriverError)] = &[
    ("BUFFER_TOO_SMALL", DriverError::BufferTooSmall),
    ("BAD_MAGIC", DriverError::BadMagic),
    (
        "ABI_VERSION_UNSUPPORTED",
        DriverError::AbiVersionUnsupported,
    ),
    ("LENGTH_OUT_OF_RANGE", DriverError::LengthOutOfRange),
    ("OUT_OF_RANGE", DriverError::OutOfRange),
    ("PERMISSION_DENIED", DriverError::PermissionDenied),
    ("NOT_FOUND", DriverError::NotFound),
    ("SIGNATURE_INVALID", DriverError::SignatureInvalid),
    ("UNSUPPORTED", DriverError::Unsupported),
    ("DEVICE_FAULT", DriverError::DeviceFault),
    ("BUSY", DriverError::Busy),
    ("NOT_IMPLEMENTED", DriverError::NotImplemented),
    ("NO_SPACE", DriverError::NoSpace),
    ("SEAT_REVOKED", DriverError::SeatRevoked),
    ("ENDPOINT_STALLED", DriverError::EndpointStalled),
    ("MEDIUM_ERROR", DriverError::MediumError),
    ("DEVICE_OFFLINE", DriverError::DeviceOffline),
    ("TOO_MANY_LINKS", DriverError::TooManyLinks),
    ("ALREADY_EXISTS", DriverError::AlreadyExists),
    ("DIRECTORY_NOT_EMPTY", DriverError::DirectoryNotEmpty),
    ("DIRECTORY_CYCLE", DriverError::DirectoryCycle),
];

/// One generated C header: its file name (relative to the include directory)
/// and its full text.
pub struct GeneratedHeader {
    /// File name relative to [`DEFAULT_INCLUDE_DIR`], e.g. `tairix_time.h`.
    pub file_name: &'static str,
    /// Complete header text, including its include guard.
    pub body: String,
}

/// The C type a syscall argument or return [`AbiType`] is rendered as in the
/// generated header.
///
/// The widths are fixed by `<stdint.h>` so the header means the same thing on
/// every Tier-1 target. `Len` maps to `uintptr_t` because a byte count is a
/// register-width quantity (it must address the caller's whole address
/// space), and `UserPtr` to `void *`; this is the only place the
/// Rust→C type mapping is defined.
fn c_type(ty: AbiType) -> &'static str {
    match ty {
        AbiType::Unit => "void",
        // `Errno` is an `i32` discriminant; both render as `int32_t`.
        AbiType::I32 | AbiType::Errno => "int32_t",
        AbiType::U32 => "uint32_t",
        AbiType::Cap => "uint16_t",
        // Opaque kernel handles and IPC endpoints are 64-bit values.
        AbiType::U64 | AbiType::IpcEndpoint | AbiType::Handle => "uint64_t",
        AbiType::UserPtr => "void *",
        AbiType::Len => "uintptr_t",
    }
}

/// Render one syscall's C prototype, e.g. `int32_t tairix_sys_ipc_send(...)`.
fn prototype(spec: &tairix_abi::SyscallSpec) -> String {
    let ret = c_type(spec.ret);
    let arg_count = usize::from(spec.arg_count);
    let params = if arg_count == 0 {
        "void".to_string()
    } else {
        let mut parts = Vec::with_capacity(arg_count);
        for (i, ty) in spec.args.iter().take(arg_count).enumerate() {
            parts.push(format!("{} a{i}", c_type(*ty)));
        }
        parts.join(", ")
    };
    format!("{ret} tairix_sys_{}({params});", spec.name)
}

/// Shared `GENERATED FILE` banner for one module header.
///
/// `purpose` is a one-line description of what the header declares.
fn banner(purpose: &str) -> String {
    format!(
        "/*\n\
         * TAIRiX abi-v1 C development header.\n\
         *\n\
         * GENERATED FILE - DO NOT EDIT BY HAND.\n\
         *\n\
         * {purpose}\n\
         *\n\
         * This is part of the C-language view of the TAIRiX kernel/user ABI.\n\
         * It is generated from the single source of truth in `lib/abi` by\n\
         * `cargo xtask c-header --write` and verified on every CI run by\n\
         * `cargo xtask c-header`. Edit `lib/abi` and regenerate; never edit\n\
         * this file directly (AGENTS.md sec.2.2, sec.9).\n\
         */\n\n"
    )
}

/// `tairix_error.h` — the stable `abi-v1` error codes.
fn generate_error() -> String {
    use std::fmt::Write as _;
    let mut out = banner("Stable abi-v1 error codes (Errno discriminants).");
    out.push_str("#ifndef TAIRIX_ERROR_H\n#define TAIRIX_ERROR_H\n\n");
    out.push_str("/* Stable abi-v1 error codes (int32_t). */\n");
    for (name, errno) in ERRNO_NAMES {
        let _ = writeln!(out, "#define TAIRIX_E_{name} {}", errno.as_i32());
    }
    out.push_str("\n#endif /* TAIRIX_ERROR_H */\n");
    out
}

/// `tairix_capability.h` — the capability identifiers.
fn generate_capability() -> String {
    use std::fmt::Write as _;
    let mut out = banner("Capability identifiers (AGENTS.md sec.5.2).");
    out.push_str("#ifndef TAIRIX_CAPABILITY_H\n#define TAIRIX_CAPABILITY_H\n\n");
    out.push_str("#include <stdint.h>\n\n");
    out.push_str(
        "/* Capability identifiers (uint16_t, the canonical CapabilityId width;\n   AGENTS.md sec.5.2). Each id carries its type so call sites need no cast. */\n",
    );
    let _ = writeln!(
        out,
        "#define TAIRIX_CAPABILITY_ID_MAX ((uint16_t){CAPABILITY_ID_MAX}u)"
    );
    for raw in 1..=CAPABILITY_ID_MAX {
        if let Some(name) = CapabilityId::from_raw(raw)
            .ok()
            .and_then(CapabilityId::name)
        {
            let _ = writeln!(out, "#define TAIRIX_{name} ((uint16_t){raw}u)");
        }
    }
    out.push_str("\n#endif /* TAIRIX_CAPABILITY_H */\n");
    out
}

/// `tairix_time.h` — the 64-bit-native time types.
///
/// `tairix_time64_t` / `tairix_duration64_t` mirror the `#[repr(C)]` layout of
/// [`Time64`] / [`Duration64`] (8-byte signed seconds + a 4-byte canonical
/// nanosecond field). Their packed little-endian *wire* size is the separate
/// `*_WIRE_LEN` macro (12 bytes); the in-memory struct is naturally aligned.
fn generate_time() -> String {
    use std::fmt::Write as _;
    let mut out = banner("64-bit-native time types (AGENTS.md sec.21).");
    out.push_str("#ifndef TAIRIX_TIME_H\n#define TAIRIX_TIME_H\n\n");
    out.push_str("#include <stdint.h>\n\n");

    out.push_str("/* Nanoseconds in one second; the sub-second field stays in 0..this. */\n");
    let _ = writeln!(out, "#define TAIRIX_NANOS_PER_SEC {NANOS_PER_SEC}u");
    out.push_str(
        "/* Coarse monotonic-clock granularity, ns, for callers without CAP_TIME_HIRES. */\n",
    );
    let _ = writeln!(
        out,
        "#define TAIRIX_COARSE_CLOCK_GRANULARITY_NS {COARSE_CLOCK_GRANULARITY_NS}ull"
    );
    out.push_str("/* Packed little-endian wire size of each time value, in bytes. */\n");
    let _ = writeln!(out, "#define TAIRIX_TIME64_WIRE_LEN {}u", Time64::WIRE_LEN);
    let _ = writeln!(
        out,
        "#define TAIRIX_DURATION64_WIRE_LEN {}u",
        Duration64::WIRE_LEN
    );
    out.push_str(
        "/* Plausibility window a time source's reading is checked against:\n\
         \x20* this release's epoch, and the width of the window above it.\n\
         \x20* Fixed validation bounds, not capacities. */\n",
    );
    let _ = writeln!(
        out,
        "#define TAIRIX_RELEASE_EPOCH_SECS INT64_C({RELEASE_EPOCH_SECS})"
    );
    let _ = writeln!(
        out,
        "#define TAIRIX_PLAUSIBLE_FUTURE_SECS INT64_C({PLAUSIBLE_FUTURE_SECS})"
    );
    out.push('\n');

    out.push_str(
        "/* Absolute instant: signed seconds since the Unix epoch + canonical nanos. */\n\
         typedef struct tairix_time64 {\n\
         \x20   int64_t secs;\n\
         \x20   uint32_t nanos;\n\
         } tairix_time64_t;\n\n",
    );
    out.push_str(
        "/* Span of time: signed seconds + canonical nanos (companion to tairix_time64). */\n\
         typedef struct tairix_duration64 {\n\
         \x20   int64_t secs;\n\
         \x20   uint32_t nanos;\n\
         } tairix_duration64_t;\n\n",
    );

    out.push_str("#endif /* TAIRIX_TIME_H */\n");
    out
}

/// `tairix_random.h` — the canonical random-number ABI.
///
/// Declares the single defined request flag bit (`TAIRIX_RANDOM_FLAG_*`, read
/// from [`RandomFlags`]) and the byte-count limits of a single request. The
/// flag register is a `uint32_t`; the byte counts are register-width
/// quantities (`uintptr_t`), matching the `Len` mapping in [`c_type`].
fn generate_random() -> String {
    use std::fmt::Write as _;
    let mut out = banner("Canonical random-number ABI (AGENTS.md sec.22).");
    out.push_str("#ifndef TAIRIX_RANDOM_H\n#define TAIRIX_RANDOM_H\n\n");
    out.push_str("#include <stdint.h>\n\n");

    out.push_str(
        "/* Request flags (uint32_t). Every undefined bit is reserved and must be zero. */\n",
    );
    let _ = writeln!(
        out,
        "#define TAIRIX_RANDOM_FLAG_NON_BLOCKING {:#x}u",
        RandomFlags::NON_BLOCKING.bits()
    );
    out.push('\n');

    out.push_str("/* Default per-CPU random output reserve, in bytes. */\n");
    let _ = writeln!(
        out,
        "#define TAIRIX_RANDOM_RESERVE_DEFAULT_BYTES ((uintptr_t){RANDOM_RESERVE_DEFAULT_BYTES}u)"
    );
    out.push_str("/* Maximum number of bytes a single random request may ask for. */\n");
    let _ = writeln!(
        out,
        "#define TAIRIX_RANDOM_REQUEST_MAX_BYTES ((uintptr_t){RANDOM_REQUEST_MAX_BYTES}u)"
    );
    out.push('\n');

    out.push_str("#endif /* TAIRIX_RANDOM_H */\n");
    out
}

/// `tairix_log.h` — the `log_emit` diagnostic-record ABI.
///
/// Declares the bounds of a `log_emit` record (the wire image
/// `tairix_sys_log_emit` consumes) so a non-Rust program can build one: the
/// highest valid level byte, the message and field-count caps, the per-field
/// key/value caps, the fixed header length, and the maximum encoded size.
/// The byte caps are register-width quantities (`uintptr_t`), matching the
/// `Len` mapping in [`c_type`]; the level cap is a single byte. The wire
/// layout itself is documented inline.
fn generate_log() -> String {
    use std::fmt::Write as _;
    let mut out = banner("log_emit diagnostic-record ABI (AGENTS.md sec.19.4 / sec.20).");
    out.push_str("#ifndef TAIRIX_LOG_H\n#define TAIRIX_LOG_H\n\n");
    out.push_str("#include <stdint.h>\n\n");

    out.push_str(
        "/*\n\
         \x20* Wire layout of a log_emit record (all scalars little-endian):\n\
         \x20*   offset 0: uint8_t  level        (0..=TAIRIX_LOG_LEVEL_MAX)\n\
         \x20*   offset 1: uint8_t  field_count  (<= TAIRIX_LOG_FIELDS_MAX)\n\
         \x20*   offset 2: uint16_t message_len  (<= TAIRIX_LOG_MESSAGE_MAX)\n\
         \x20*   offset 4: uint32_t event_id\n\
         \x20*   offset 8: message bytes (message_len, UTF-8)\n\
         \x20*   then field_count records, each:\n\
         \x20*     uint8_t key_len   (<= TAIRIX_LOG_FIELD_KEY_MAX)\n\
         \x20*     key bytes         (key_len, UTF-8)\n\
         \x20*     a typed field value: a 1-byte TAIRIX_FIELD_TAG_* discriminant\n\
         \x20*       followed by its little-endian payload. The whole encoded\n\
         \x20*       value is <= TAIRIX_LOG_FIELD_VALUE_MAX bytes. Payloads:\n\
         \x20*         NULL: none.  BOOL: 1 byte (0|1).\n\
         \x20*         SIGNED/UNSIGNED: 8 bytes.  TIME/DURATION: 12 bytes.\n\
         \x20*         DECIMAL: int64 mantissa + uint8 scale (9 bytes).\n\
         \x20*         STR/BYTES: uint16 len then len bytes.\n\
         \x20*         UUID: 16 bytes.  MAC: 6 bytes.\n\
         \x20*         IP: uint8 family (4|6) then 4 or 16 bytes.\n\
         \x20*         ERROR: int32.  CAP: uint16.\n\
         \x20*         LIST: uint8 elem-tag, uint16 count, then count payloads.\n\
         \x20*/\n",
    );

    out.push_str("/* Highest valid level byte (the Critical discriminant). */\n");
    let _ = writeln!(
        out,
        "#define TAIRIX_LOG_LEVEL_MAX ((uint8_t){LOG_LEVEL_MAX}u)"
    );
    out.push_str("/* Maximum message length, in bytes. */\n");
    let _ = writeln!(
        out,
        "#define TAIRIX_LOG_MESSAGE_MAX ((uintptr_t){LOG_MESSAGE_MAX}u)"
    );
    out.push_str("/* Maximum number of structured key/value fields. */\n");
    let _ = writeln!(
        out,
        "#define TAIRIX_LOG_FIELDS_MAX ((uintptr_t){LOG_FIELDS_MAX}u)"
    );
    out.push_str("/* Maximum field key length, in bytes. */\n");
    let _ = writeln!(
        out,
        "#define TAIRIX_LOG_FIELD_KEY_MAX ((uintptr_t){LOG_FIELD_KEY_MAX}u)"
    );
    out.push_str("/* Maximum encoded field-value length, in bytes (tag + payload). */\n");
    let _ = writeln!(
        out,
        "#define TAIRIX_LOG_FIELD_VALUE_MAX ((uintptr_t){LOG_FIELD_VALUE_MAX}u)"
    );
    out.push_str(
        "/* Byte budget for all of a record's encoded fields together: a\n\
         \x20* record whose fields exceed it is refused even when it is within\n\
         \x20* TAIRIX_LOG_FIELDS_MAX. */\n",
    );
    let _ = writeln!(
        out,
        "#define TAIRIX_LOG_FIELDS_PAYLOAD_MAX ((uintptr_t){LOG_FIELDS_PAYLOAD_MAX}u)"
    );
    out.push_str("/* Fixed record header length, in bytes. */\n");
    let _ = writeln!(
        out,
        "#define TAIRIX_LOG_RECORD_HEADER_LEN ((uintptr_t){LOG_RECORD_HEADER_LEN}u)"
    );
    out.push_str("/* Maximum encoded record length, in bytes. */\n");
    let _ = writeln!(
        out,
        "#define TAIRIX_LOG_RECORD_MAX ((uintptr_t){LOG_RECORD_MAX}u)"
    );
    out.push('\n');

    out.push_str("/* Field-value type tags: the first byte of an encoded field value. */\n");
    for (name, tag) in [
        ("NULL", TAG_NULL),
        ("BOOL", TAG_BOOL),
        ("SIGNED", TAG_SIGNED),
        ("UNSIGNED", TAG_UNSIGNED),
        ("DECIMAL", TAG_DECIMAL),
        ("TIME", TAG_TIME),
        ("DURATION", TAG_DURATION),
        ("STR", TAG_STR),
        ("BYTES", TAG_BYTES),
        ("UUID", TAG_UUID),
        ("IP", TAG_IP),
        ("MAC", TAG_MAC),
        ("ERROR", TAG_ERROR),
        ("CAP", TAG_CAP),
        ("LIST", TAG_LIST),
    ] {
        let _ = writeln!(out, "#define TAIRIX_FIELD_TAG_{name} ((uint8_t){tag}u)");
    }
    out.push('\n');

    out.push_str("#endif /* TAIRIX_LOG_H */\n");
    out
}

/// `tairix_rlimit.h` — the resource-limit ABI.
///
/// Declares the closed [`LimitKind`] discriminants as `TAIRIX_LIMIT_KIND_*`
/// macros, the no-limit sentinel `TAIRIX_RLIMIT_INFINITY`, the wire length, and
/// the `#[repr(C)]` [`ResourceLimit`] pair as a typedef. Every numeric value
/// is read from `lib/abi`; only the C spelling lives here.
fn generate_rlimit() -> String {
    use std::fmt::Write as _;
    let mut out = banner("Resource-limit ABI (AGENTS.md sec.24).");
    out.push_str("#ifndef TAIRIX_RLIMIT_H\n#define TAIRIX_RLIMIT_H\n\n");
    out.push_str("#include <stdint.h>\n\n");

    out.push_str("/* A bound value meaning \"no limit imposed\" (AGENTS.md sec.24.3). */\n");
    let _ = writeln!(
        out,
        "#define TAIRIX_RLIMIT_INFINITY ((uint64_t){RLIMIT_INFINITY}u)"
    );
    out.push('\n');

    out.push_str(
        "/* Resource kinds a tairix_resource_limit_t can govern (uint32_t; AGENTS.md sec.24.3). */\n",
    );
    for kind in LimitKind::ALL {
        let raw = kind.as_u32();
        let suffix = kind.name().to_ascii_uppercase().replace('-', "_");
        let _ = writeln!(out, "#define TAIRIX_LIMIT_KIND_{suffix} ((uint32_t){raw}u)");
    }
    let _ = writeln!(
        out,
        "#define TAIRIX_LIMIT_KIND_COUNT ((uint32_t){}u)",
        LimitKind::COUNT
    );
    out.push('\n');

    out.push_str(
        "/* Length, in bytes, of the little-endian tairix_resource_limit_t encoding. */\n",
    );
    let _ = writeln!(
        out,
        "#define TAIRIX_RESOURCE_LIMIT_WIRE_LEN {}u",
        ResourceLimit::WIRE_LEN
    );
    out.push('\n');

    out.push_str("/* A soft/hard resource-limit pair (AGENTS.md sec.24.3). */\n");
    out.push_str(
        "typedef struct tairix_resource_limit {\n\
         \x20   uint64_t soft;\n\
         \x20   uint64_t hard;\n\
         } tairix_resource_limit_t;\n\n",
    );

    out.push_str("#endif /* TAIRIX_RLIMIT_H */\n");
    out
}

/// `tairix_memory.h` — the page granule and the anonymous-memory `mem_map`
/// flag bits (`plans/SPAWN.md` SP5).
///
/// Declares the granule a mapping length rounds up to (`TAIRIX_PAGE_SIZE`) and
/// the single defined `mem_map` flag bit (`TAIRIX_MAP_FLAG_*`, read from
/// [`MapFlags`]). The flag register is a `uint32_t`, matching the `U32`
/// argument the `tairix_sys_mem_map` prototype carries.
fn generate_memory() -> String {
    use std::fmt::Write as _;
    let mut out = banner("Page granule and mem_map flag bits (plans/SPAWN.md SP5).");
    out.push_str("#ifndef TAIRIX_MEMORY_H\n#define TAIRIX_MEMORY_H\n\n");
    out.push_str("#include <stdint.h>\n\n");

    out.push_str("/* Page granule: mem_map rounds a mapping length up to this. */\n");
    let _ = writeln!(out, "#define TAIRIX_PAGE_SIZE ((uintptr_t){PAGE_SIZE}u)");
    out.push('\n');

    out.push_str(
        "/* mem_map flags (uint32_t). Every undefined bit is reserved and must be zero. */\n",
    );
    let _ = writeln!(
        out,
        "#define TAIRIX_MAP_FLAG_FIXED {:#x}u",
        MapFlags::FIXED.bits()
    );
    out.push('\n');

    out.push_str("#endif /* TAIRIX_MEMORY_H */\n");
    out
}

/// `tairix_hwtree.h` — the architecture-neutral hardware tree.
///
/// Declares the hardware-tree version, the root-parent sentinel, the array
/// bounds, the packed little-endian `*_WIRE_LEN` of each record, the
/// closed device-class / match-kind / resource-kind enumerations as
/// `TAIRIX_HW_*` macros, and the `#[repr(C)]` record layouts as typedefs.
/// Every numeric value is read from `lib/abi`; only the C spelling lives
/// here.
fn generate_hwtree() -> String {
    use std::fmt::Write as _;
    use tairix_abi::driver::net::MAC_ADDRESS_LEN;
    let mut out = banner("Architecture-neutral hardware tree (AGENTS.md sec.18.1).");
    out.push_str("#ifndef TAIRIX_HWTREE_H\n#define TAIRIX_HWTREE_H\n\n");
    out.push_str("#include <stdint.h>\n\n");

    out.push_str("/* Hardware-tree ABI version. */\n");
    let _ = writeln!(out, "#define TAIRIX_HWTREE_VERSION {HWTREE_VERSION_V1}u");
    out.push_str("/* Parent id marking a node with no parent (a tree root). */\n");
    let _ = writeln!(out, "#define TAIRIX_HW_NODE_ROOT {HW_NODE_ROOT}u");
    out.push('\n');

    out.push_str("/* Array bounds. */\n");
    let _ = writeln!(
        out,
        "#define TAIRIX_HW_COMPATIBLE_MAX ((uintptr_t){HW_COMPATIBLE_MAX}u)"
    );
    let _ = writeln!(
        out,
        "#define TAIRIX_HW_NODE_MAX_MATCH_KEYS ((uintptr_t){HW_NODE_MAX_MATCH_KEYS}u)"
    );
    let _ = writeln!(
        out,
        "#define TAIRIX_HW_NODE_MAX_RESOURCES ((uintptr_t){HW_NODE_MAX_RESOURCES}u)"
    );
    out.push_str("/* Length, in bytes, of an Ethernet MAC address. */\n");
    let _ = writeln!(out, "#define TAIRIX_MAC_ADDRESS_LEN {MAC_ADDRESS_LEN}u");
    out.push('\n');

    out.push_str("/* Packed little-endian wire sizes, in bytes. */\n");
    let _ = writeln!(
        out,
        "#define TAIRIX_HW_MATCH_KEY_WIRE_LEN {}u",
        HwMatchKey::WIRE_LEN
    );
    let _ = writeln!(
        out,
        "#define TAIRIX_HW_RESOURCE_WIRE_LEN {}u",
        HwResource::WIRE_LEN
    );
    let _ = writeln!(
        out,
        "#define TAIRIX_HW_NODE_HEADER_LEN {HW_NODE_HEADER_LEN}u"
    );
    let _ = writeln!(out, "#define TAIRIX_HW_NODE_WIRE_LEN {}u", HwNode::WIRE_LEN);
    out.push('\n');

    hwtree_enum_macros(&mut out);
    hwtree_structs(&mut out);

    out.push_str("#endif /* TAIRIX_HWTREE_H */\n");
    out
}

/// Emit the closed device-class / match-kind / resource-kind enumerations
/// as `TAIRIX_HW_*` macros, reading every value from `lib/abi`.
fn hwtree_enum_macros(out: &mut String) {
    use std::fmt::Write as _;
    out.push_str("/* Device classes (uint16_t). */\n");
    for (name, class) in [
        ("ROOT", HwDeviceClass::Root),
        ("BUS", HwDeviceClass::Bus),
        ("CPU", HwDeviceClass::Cpu),
        ("MEMORY", HwDeviceClass::Memory),
        ("TIMER", HwDeviceClass::Timer),
        ("INTERRUPT_CONTROLLER", HwDeviceClass::InterruptController),
        ("DISPLAY", HwDeviceClass::Display),
        ("INPUT", HwDeviceClass::Input),
        ("NETWORK", HwDeviceClass::Network),
        ("STORAGE", HwDeviceClass::Storage),
        ("SERIAL", HwDeviceClass::Serial),
        ("RTC", HwDeviceClass::Rtc),
        ("OTHER", HwDeviceClass::Other),
    ] {
        let _ = writeln!(
            out,
            "#define TAIRIX_HW_CLASS_{name} ((uint16_t){}u)",
            class.as_u16()
        );
    }
    out.push('\n');

    out.push_str("/* Match-key kinds (uint16_t). */\n");
    for (name, kind) in [
        ("COMPATIBLE", HwMatchKind::Compatible),
        ("PCI", HwMatchKind::Pci),
        ("USB", HwMatchKind::Usb),
        ("VIRTIO", HwMatchKind::Virtio),
    ] {
        let _ = writeln!(
            out,
            "#define TAIRIX_HW_MATCH_{name} ((uint16_t){}u)",
            kind.as_u16()
        );
    }
    out.push('\n');

    out.push_str("/* Resource kinds (uint16_t). */\n");
    for (name, kind) in [
        ("MMIO", HwResourceKind::Mmio),
        ("IRQ", HwResourceKind::Irq),
        ("PORT", HwResourceKind::Port),
        ("DMA", HwResourceKind::Dma),
        ("BUS_WINDOW", HwResourceKind::BusWindow),
        ("ENDPOINT", HwResourceKind::Endpoint),
        ("SHARED", HwResourceKind::Shared),
        ("FRAMEBUFFER", HwResourceKind::Framebuffer),
    ] {
        let _ = writeln!(
            out,
            "#define TAIRIX_HW_RES_{name} ((uint16_t){}u)",
            kind.as_u16()
        );
    }
    out.push('\n');
}

/// Emit the `#[repr(C)]` hardware-tree record layouts as C typedefs.
fn hwtree_structs(out: &mut String) {
    out.push_str(
        "/* One match key on a node. Mirrors the #[repr(C)] layout; the packed\n\
         * little-endian wire size is TAIRIX_HW_MATCH_KEY_WIRE_LEN. */\n\
         typedef struct tairix_hw_match_key {\n\
         \x20   uint16_t kind;\n\
         \x20   uint8_t compatible_len;\n\
         \x20   uint16_t vendor;\n\
         \x20   uint16_t product;\n\
         \x20   uint32_t class_code;\n\
         \x20   uint8_t compatible[TAIRIX_HW_COMPATIBLE_MAX];\n\
         } tairix_hw_match_key_t;\n\n",
    );
    out.push_str(
        "/* One resource a node exposes, as a capability-grant request. */\n\
         typedef struct tairix_hw_resource {\n\
         \x20   uint16_t kind;\n\
         \x20   uint16_t capability;\n\
         \x20   uint32_t flags;\n\
         \x20   uint64_t base;\n\
         \x20   uint64_t length;\n\
         \x20   uint64_t translated_base;\n\
         } tairix_hw_resource_t;\n\n",
    );
    out.push_str(
        "/* One node in the hardware tree. Mirrors the #[repr(C)] layout; the\n\
         * packed little-endian wire size is TAIRIX_HW_NODE_WIRE_LEN. */\n\
         typedef struct tairix_hw_node {\n\
         \x20   uint32_t id;\n\
         \x20   uint32_t parent;\n\
         \x20   uint32_t address;\n\
         \x20   uint16_t device_class;\n\
         \x20   uint8_t match_key_count;\n\
         \x20   uint8_t resource_count;\n\
         \x20   uint8_t fault_health;\n\
         \x20   tairix_hw_match_key_t match_keys[TAIRIX_HW_NODE_MAX_MATCH_KEYS];\n\
         \x20   tairix_hw_resource_t resources[TAIRIX_HW_NODE_MAX_RESOURCES];\n\
         } tairix_hw_node_t;\n\n",
    );
}

/// `tairix_ipc.h` — the IPC message header and port-name wire types.
///
/// `tairix_ipc_message_header_t` mirrors the `#[repr(C)]` layout of
/// [`IpcMessageHeader`] and `tairix_port_name_t` that of [`PortName`]; each is
/// naturally aligned. Their packed little-endian *wire* size is the separate
/// `*_WIRE_LEN` macro. Every numeric value is read from `lib/abi`, never
/// re-typed; only the C spelling lives here.
fn generate_ipc() -> String {
    use std::fmt::Write as _;
    let mut out = banner("IPC message header and port-name wire types (AGENTS.md sec.4).");
    out.push_str("#ifndef TAIRIX_IPC_H\n#define TAIRIX_IPC_H\n\n");
    out.push_str("#include <stdint.h>\n\n");

    out.push_str("/* Magic word identifying an abi-v1 IPC message (\"IPC1\" little-endian). */\n");
    let _ = writeln!(
        out,
        "#define TAIRIX_IPC_MESSAGE_HEADER_MAGIC {IPC_MESSAGE_HEADER_MAGIC:#x}u"
    );
    out.push_str("/* Maximum payload length, in bytes, an IPC message header may advertise. */\n");
    let _ = writeln!(
        out,
        "#define TAIRIX_IPC_MESSAGE_MAX_PAYLOAD_LEN {}u",
        tairix_abi::ipc::IPC_MESSAGE_MAX_PAYLOAD_LEN
    );
    out.push_str("/* Packed little-endian wire size of an IPC message header, in bytes. */\n");
    let _ = writeln!(
        out,
        "#define TAIRIX_IPC_MESSAGE_HEADER_WIRE_LEN {}u",
        IpcMessageHeader::WIRE_LEN
    );
    out.push('\n');

    out.push_str(
        "/* call_recv flags (uint32_t). Every undefined bit is reserved and must be zero. */\n",
    );
    let _ = writeln!(
        out,
        "#define TAIRIX_CALL_RECV_FLAG_NON_BLOCKING {:#x}u",
        CallRecvFlags::NON_BLOCKING.bits()
    );
    out.push('\n');

    out.push_str("/* Maximum length, in bytes, of a port name (excludes the length byte). */\n");
    let _ = writeln!(out, "#define TAIRIX_PORT_NAME_MAX_LEN {PORT_NAME_MAX_LEN}u");
    out.push_str("/* Packed little-endian wire size of a port name, in bytes. */\n");
    let _ = writeln!(
        out,
        "#define TAIRIX_PORT_NAME_WIRE_LEN {}u",
        PortName::WIRE_LEN
    );
    out.push('\n');

    out.push_str(
        "/* IPC message header: prefixes every message; encoded little-endian on the wire. */\n\
         typedef struct tairix_ipc_message_header {\n\
         \x20   uint32_t magic;\n\
         \x20   uint16_t version;\n\
         \x20   uint16_t flags;\n\
         \x20   uint64_t endpoint;\n\
         \x20   uint64_t sender;\n\
         \x20   uint32_t payload_len;\n\
         \x20   uint32_t reserved;\n\
         } tairix_ipc_message_header_t;\n\n",
    );
    out.push_str(
        "/* Validated well-known IPC port name: NUL-padded name bytes + a length byte. */\n\
         typedef struct tairix_port_name {\n\
         \x20   uint8_t bytes[TAIRIX_PORT_NAME_MAX_LEN];\n\
         \x20   uint8_t len;\n\
         } tairix_port_name_t;\n\n",
    );

    out.push_str("#endif /* TAIRIX_IPC_H */\n");
    out
}

/// `tairix_stdinfo.h` — the Standard Information Stream ABI.
///
/// Declares the reserved `stdinfo` file descriptor, the framing version tags,
/// and the closed [`StdInfoKind`] / [`Severity`] discriminant sets. The kinds
/// and severities travel on the wire as strings; the `#[repr(u8)]`
/// discriminants are emitted so a C consumer can name each variant. Every
/// value is read from `lib/abi`, never re-typed; only the C spelling lives
/// here.
fn generate_stdinfo() -> String {
    use std::fmt::Write as _;
    let mut out = banner("Standard Information Stream ABI (AGENTS.md sec.20).");
    out.push_str("#ifndef TAIRIX_STDINFO_H\n#define TAIRIX_STDINFO_H\n\n");
    out.push_str("#include <stdint.h>\n\n");

    out.push_str("/* Reserved stdinfo file descriptor; no component may repurpose it. */\n");
    let _ = writeln!(out, "#define TAIRIX_STDINFO_FD {STDINFO_FD}u");
    out.push_str("/* stdinfo framing version tag for the frozen v1 framing. */\n");
    let _ = writeln!(
        out,
        "#define TAIRIX_STDINFO_VERSION_V1 {STDINFO_VERSION_V1}u"
    );
    out.push_str("/* stdinfo framing version this header set describes. */\n");
    let _ = writeln!(
        out,
        "#define TAIRIX_STDINFO_VERSION_CURRENT {STDINFO_VERSION_CURRENT}u"
    );
    out.push('\n');

    out.push_str(
        "/* Closed set of record kinds (uint8_t). Wire spelling is the string in parens. */\n",
    );
    let _ = writeln!(
        out,
        "#define TAIRIX_STDINFO_KIND_OMISSION ((uint8_t){}u) /* \"{}\" */",
        StdInfoKind::Omission as u8,
        StdInfoKind::Omission.as_str()
    );
    let _ = writeln!(
        out,
        "#define TAIRIX_STDINFO_KIND_SUMMARY ((uint8_t){}u) /* \"{}\" */",
        StdInfoKind::Summary as u8,
        StdInfoKind::Summary.as_str()
    );
    let _ = writeln!(
        out,
        "#define TAIRIX_STDINFO_KIND_SCHEMA ((uint8_t){}u) /* \"{}\" */",
        StdInfoKind::Schema as u8,
        StdInfoKind::Schema.as_str()
    );
    let _ = writeln!(
        out,
        "#define TAIRIX_STDINFO_KIND_SUGGESTION ((uint8_t){}u) /* \"{}\" */",
        StdInfoKind::Suggestion as u8,
        StdInfoKind::Suggestion.as_str()
    );
    let _ = writeln!(
        out,
        "#define TAIRIX_STDINFO_KIND_CONTEXT ((uint8_t){}u) /* \"{}\" */",
        StdInfoKind::Context as u8,
        StdInfoKind::Context.as_str()
    );
    out.push('\n');

    out.push_str("/* Advisory severity (uint8_t). Security events use lib/log, not fd 3. */\n");
    let _ = writeln!(
        out,
        "#define TAIRIX_STDINFO_SEVERITY_INFO ((uint8_t){}u) /* \"{}\" */",
        Severity::Info as u8,
        Severity::Info.as_str()
    );
    let _ = writeln!(
        out,
        "#define TAIRIX_STDINFO_SEVERITY_DEBUG ((uint8_t){}u) /* \"{}\" */",
        Severity::Debug as u8,
        Severity::Debug.as_str()
    );
    out.push('\n');

    out.push_str("#endif /* TAIRIX_STDINFO_H */\n");
    out
}

/// `tairix_manifest.h` — the signed `rxe` manifest header.
///
/// `tairix_manifest_header_t` mirrors the `#[repr(C)]` layout of
/// [`ManifestHeader`]: the fixed-size prefix of the signed manifest section of
/// an `rxe` binary. Its packed little-endian *wire* size is the separate
/// `TAIRIX_MANIFEST_HEADER_WIRE_LEN` macro (equal to the struct size here, as the
/// layout has no trailing padding). Every numeric value is read from
/// `lib/abi`, never re-typed; only the C spelling lives here.
fn generate_manifest() -> String {
    use std::fmt::Write as _;
    let mut out = banner("Signed rxe manifest header (AGENTS.md sec.9).");
    out.push_str("#ifndef TAIRIX_MANIFEST_H\n#define TAIRIX_MANIFEST_H\n\n");
    out.push_str("#include <stdint.h>\n\n");

    out.push_str("/* Magic word identifying an abi-v1 manifest (\"RXM1\" little-endian). */\n");
    let _ = writeln!(out, "#define TAIRIX_MANIFEST_MAGIC {MANIFEST_MAGIC:#x}u");
    out.push_str("/* Maximum number of capability identifiers a manifest may request. */\n");
    let _ = writeln!(
        out,
        "#define TAIRIX_MANIFEST_MAX_CAPABILITIES {MANIFEST_MAX_CAPABILITIES}u"
    );
    out.push_str("/* Length, in bytes, of the linked syscall-table hash (SHA-256). */\n");
    let _ = writeln!(
        out,
        "#define TAIRIX_SYSCALL_TABLE_HASH_LEN {SYSCALL_TABLE_HASH_LEN}u"
    );
    out.push_str("/* Packed little-endian wire size of a manifest header, in bytes. */\n");
    let _ = writeln!(
        out,
        "#define TAIRIX_MANIFEST_HEADER_WIRE_LEN {}u",
        ManifestHeader::WIRE_LEN
    );
    out.push('\n');

    out.push_str(
        "/* Signed rxe manifest prefix; encoded little-endian on the wire. */\n\
         typedef struct tairix_manifest_header {\n\
         \x20   uint32_t magic;\n\
         \x20   uint32_t abi_version;\n\
         \x20   uint32_t flags;\n\
         \x20   uint16_t capability_count;\n\
         \x20   uint16_t reserved0;\n\
         \x20   uint8_t syscall_table_hash[TAIRIX_SYSCALL_TABLE_HASH_LEN];\n\
         \x20   uint8_t signer_pubkey[32];\n\
         \x20   uint8_t signature[64];\n\
         } tairix_manifest_header_t;\n\n",
    );

    out.push_str("#endif /* TAIRIX_MANIFEST_H */\n");
    out
}

/// `tairix_input.h` — the desktop input event ABI.
///
/// Declares the pointer ([`PointerInput`]) and keyboard ([`KeyInput`]) record
/// magics and packed wire sizes, the `kind`, `button`, `key_class`, and
/// modifier field codes, and the [`PointerButtonCode`] / [`NamedKeyCode`]
/// `#[repr(u16)]` discriminant sets. Both records are hand-serialised
/// little-endian byte images (not a `#[repr(C)]` struct), so the header
/// exports their field codes and wire sizes rather than a C struct mirror; a
/// [`Errno`] decoder on the Rust side validates the bytes. Every value is read
/// from `lib/abi`, never re-typed; only the C spelling lives here.
fn generate_input() -> String {
    use std::fmt::Write as _;
    let mut out = banner("Desktop pointer and keyboard input ABI (AGENTS.md sec.9, sec.10).");
    out.push_str("#ifndef TAIRIX_INPUT_H\n#define TAIRIX_INPUT_H\n\n");
    out.push_str("#include <stdint.h>\n\n");

    // Record magics ("PIN1" / "KIN1") and their packed little-endian wire sizes.
    let _ = writeln!(
        out,
        "#define TAIRIX_POINTER_INPUT_MAGIC {POINTER_INPUT_MAGIC:#x}u"
    );
    let _ = writeln!(out, "#define TAIRIX_KEY_INPUT_MAGIC {KEY_INPUT_MAGIC:#x}u");
    let pwl = PointerInput::WIRE_LEN;
    let kwl = KeyInput::WIRE_LEN;
    let _ = writeln!(out, "#define TAIRIX_POINTER_INPUT_WIRE_LEN {pwl}u");
    let _ = writeln!(out, "#define TAIRIX_KEY_INPUT_WIRE_LEN {kwl}u");
    out.push('\n');

    // Each uint16_t field code, grouped by record field; `hex` selects the C
    // spelling (bitmask fields read better in hex). Only the names live here.
    let mut emit = |comment: &str, defs: &[(&str, u16)], hex: bool| {
        let _ = writeln!(out, "/* {comment} */");
        for &(name, value) in defs {
            if hex {
                let _ = writeln!(out, "#define {name} ((uint16_t){value:#x}u)");
            } else {
                let _ = writeln!(out, "#define {name} ((uint16_t){value}u)");
            }
        }
        out.push('\n');
    };
    emit(
        "Record `kind` codes: pointer moves/clicks/scroll then key down/up (uint16_t).",
        &[
            ("TAIRIX_INPUT_KIND_MOVED_BY", KIND_MOVED_BY),
            ("TAIRIX_INPUT_KIND_PRESSED", KIND_PRESSED),
            ("TAIRIX_INPUT_KIND_RELEASED", KIND_RELEASED),
            ("TAIRIX_INPUT_KIND_SCROLLED", KIND_SCROLLED),
            ("TAIRIX_INPUT_KIND_KEY_PRESSED", KIND_KEY_PRESSED),
            ("TAIRIX_INPUT_KIND_KEY_RELEASED", KIND_KEY_RELEASED),
        ],
        false,
    );
    emit(
        "`button` (motion=none, else a button) and keyboard `key_class` codes (uint16_t).",
        &[
            ("TAIRIX_INPUT_BUTTON_NONE", BUTTON_NONE),
            (
                "TAIRIX_POINTER_BUTTON_PRIMARY",
                PointerButtonCode::Primary.code(),
            ),
            (
                "TAIRIX_POINTER_BUTTON_SECONDARY",
                PointerButtonCode::Secondary.code(),
            ),
            (
                "TAIRIX_POINTER_BUTTON_MIDDLE",
                PointerButtonCode::Middle.code(),
            ),
            ("TAIRIX_KEY_CLASS_CHAR", KEY_CLASS_CHAR),
            ("TAIRIX_KEY_CLASS_NAMED", KEY_CLASS_NAMED),
        ],
        false,
    );
    emit(
        "Modifier bits held while a key event was produced (uint16_t).",
        &[
            ("TAIRIX_MOD_SHIFT", MOD_SHIFT),
            ("TAIRIX_MOD_CTRL", MOD_CTRL),
            ("TAIRIX_MOD_ALT", MOD_ALT),
            ("TAIRIX_MOD_META", MOD_META),
            ("TAIRIX_MOD_MASK", MOD_MASK),
        ],
        true,
    );
    emit(
        "Named non-character key codes carried in a record's `named` field (uint16_t).",
        &NAMED_KEY_CODES,
        false,
    );

    out.push_str("#endif /* TAIRIX_INPUT_H */\n");
    out
}

/// The C spelling of each [`NamedKeyCode`] variant paired with its frozen wire
/// code, read from `lib/abi` via [`NamedKeyCode::code`]. Only the C *name*
/// lives here (Rust offers no variant-name reflection); the numeric value is
/// the source of truth and is pinned by the in-module test.
const NAMED_KEY_CODES: [(&str, u16); 26] = [
    ("TAIRIX_KEY_ENTER", NamedKeyCode::Enter.code()),
    ("TAIRIX_KEY_ESCAPE", NamedKeyCode::Escape.code()),
    ("TAIRIX_KEY_BACKSPACE", NamedKeyCode::Backspace.code()),
    ("TAIRIX_KEY_TAB", NamedKeyCode::Tab.code()),
    ("TAIRIX_KEY_DELETE", NamedKeyCode::Delete.code()),
    ("TAIRIX_KEY_INSERT", NamedKeyCode::Insert.code()),
    ("TAIRIX_KEY_HOME", NamedKeyCode::Home.code()),
    ("TAIRIX_KEY_END", NamedKeyCode::End.code()),
    ("TAIRIX_KEY_PAGE_UP", NamedKeyCode::PageUp.code()),
    ("TAIRIX_KEY_PAGE_DOWN", NamedKeyCode::PageDown.code()),
    ("TAIRIX_KEY_LEFT", NamedKeyCode::Left.code()),
    ("TAIRIX_KEY_RIGHT", NamedKeyCode::Right.code()),
    ("TAIRIX_KEY_UP", NamedKeyCode::Up.code()),
    ("TAIRIX_KEY_DOWN", NamedKeyCode::Down.code()),
    ("TAIRIX_KEY_F1", NamedKeyCode::F1.code()),
    ("TAIRIX_KEY_F2", NamedKeyCode::F2.code()),
    ("TAIRIX_KEY_F3", NamedKeyCode::F3.code()),
    ("TAIRIX_KEY_F4", NamedKeyCode::F4.code()),
    ("TAIRIX_KEY_F5", NamedKeyCode::F5.code()),
    ("TAIRIX_KEY_F6", NamedKeyCode::F6.code()),
    ("TAIRIX_KEY_F7", NamedKeyCode::F7.code()),
    ("TAIRIX_KEY_F8", NamedKeyCode::F8.code()),
    ("TAIRIX_KEY_F9", NamedKeyCode::F9.code()),
    ("TAIRIX_KEY_F10", NamedKeyCode::F10.code()),
    ("TAIRIX_KEY_F11", NamedKeyCode::F11.code()),
    ("TAIRIX_KEY_F12", NamedKeyCode::F12.code()),
];

/// `tairix_appinfo.h` — the application-bundle manifest ABI.
///
/// `tairix_appinfo_header_t` mirrors the `#[repr(C)]` layout of [`AppInfoHeader`]
/// (the signed manifest prefix; naturally aligned with no trailing padding, so
/// the struct size equals the wire size). Alongside it the header declares the
/// `APPINFO_*` / `BUNDLE_*` / `MIME_*` size and count limits, the curated
/// shared-library directory ([`SYSTEM_LIBRARIES_DIR`]), the fixed set of
/// permitted bundle top-level entry names ([`BundleEntry::as_str`]), and the
/// [`LibraryScope`] discriminants. Every numeric value, name, and discriminant
/// is read from `lib/abi`, never re-typed; only the C spelling lives here.
fn generate_appinfo() -> String {
    use std::fmt::Write as _;
    let mut out = banner("Application-bundle manifest ABI (AGENTS.md sec.16.5, sec.16.4).");
    out.push_str("#ifndef TAIRIX_APPINFO_H\n#define TAIRIX_APPINFO_H\n\n");
    // The syscall-table hash length is defined once, beside the driver
    // manifest that first needed it, and reused here rather than restated.
    out.push_str("#include <stdint.h>\n#include \"tairix_manifest.h\"\n\n");

    out.push_str(
        "/* Magic word identifying an abi-v1 AppInfo manifest (\"RAI1\" little-endian). */\n",
    );
    let _ = writeln!(out, "#define TAIRIX_APPINFO_MAGIC {APPINFO_MAGIC:#x}u");
    out.push_str("/* Maximum number of capability identifiers a manifest may request. */\n");
    let _ = writeln!(
        out,
        "#define TAIRIX_APPINFO_MAX_CAPABILITIES {APPINFO_MAX_CAPABILITIES}u"
    );
    out.push_str("/* Maximum number of MIME / file-type associations a bundle may declare. */\n");
    let _ = writeln!(out, "#define TAIRIX_APPINFO_MAX_MIME {APPINFO_MAX_MIME}u");
    out.push_str("/* Maximum length, in bytes, of a bundle identifier. */\n");
    let _ = writeln!(out, "#define TAIRIX_BUNDLE_ID_MAX {BUNDLE_ID_MAX}u");
    out.push_str("/* Maximum length, in bytes, of a bundle's human-readable name. */\n");
    let _ = writeln!(out, "#define TAIRIX_BUNDLE_NAME_MAX {BUNDLE_NAME_MAX}u");
    out.push_str("/* Maximum length, in bytes, of a bundle version string. */\n");
    let _ = writeln!(
        out,
        "#define TAIRIX_BUNDLE_VERSION_MAX {BUNDLE_VERSION_MAX}u"
    );
    out.push_str("/* Maximum length, in bytes, of one declared MIME-type string. */\n");
    let _ = writeln!(out, "#define TAIRIX_MIME_TYPE_MAX {MIME_TYPE_MAX}u");
    out.push_str("/* Encoded length of one MIME-type body entry (length byte + buffer). */\n");
    let _ = writeln!(out, "#define TAIRIX_MIME_ENTRY_LEN {MIME_ENTRY_LEN}u");
    out.push_str("/* Maximum length, in bytes, of a bundle's library icon asset name. */\n");
    let _ = writeln!(out, "#define TAIRIX_LIBRARY_ICON_MAX {LIBRARY_ICON_MAX}u");
    out.push_str("/* Maximum length, in bytes, of a bundle's one-line purpose. */\n");
    let _ = writeln!(
        out,
        "#define TAIRIX_BUNDLE_PURPOSE_MAX {BUNDLE_PURPOSE_MAX}u"
    );
    out.push_str("/* Maximum length, in bytes, of a bundle's author attribution. */\n");
    let _ = writeln!(out, "#define TAIRIX_BUNDLE_AUTHOR_MAX {BUNDLE_AUTHOR_MAX}u");
    out.push_str("/* Packed little-endian wire size of an AppInfo header, in bytes. */\n");
    let _ = writeln!(
        out,
        "#define TAIRIX_APPINFO_HEADER_WIRE_LEN {}u",
        AppInfoHeader::WIRE_LEN
    );
    out.push('\n');

    out.push_str("/* Curated, OS-provided shared-library directory (AGENTS.md sec.16.4). */\n");
    let _ = writeln!(
        out,
        "#define TAIRIX_SYSTEM_LIBRARIES_DIR \"{SYSTEM_LIBRARIES_DIR}\""
    );
    out.push('\n');

    out.push_str(
        "/* Fixed set of names permitted at a bundle's top level (AGENTS.md sec.16.5). */\n",
    );
    for entry in BundleEntry::ALL {
        let _ = writeln!(
            out,
            "#define TAIRIX_BUNDLE_ENTRY_{} \"{}\"",
            entry.as_str().to_ascii_uppercase(),
            entry.as_str()
        );
    }
    out.push('\n');

    out.push_str(
        "/* Which permitted root a shared-library reference resolved against (uint8_t). */\n",
    );
    let _ = writeln!(
        out,
        "#define TAIRIX_LIBRARY_SCOPE_BUNDLE ((uint8_t){}u)",
        LibraryScope::Bundle as u8
    );
    let _ = writeln!(
        out,
        "#define TAIRIX_LIBRARY_SCOPE_SYSTEM ((uint8_t){}u)",
        LibraryScope::System as u8
    );
    out.push('\n');

    emit_library_listing_bytes(&mut out);

    out.push_str("/* Signed AppInfo manifest prefix; encoded little-endian on the wire. */\n");
    out.push_str("typedef struct tairix_appinfo_header {\n");
    for &(declaration, _) in APPINFO_HEADER_FIELDS {
        let _ = writeln!(out, "    {declaration};");
    }
    out.push_str("} tairix_appinfo_header_t;\n");

    out.push_str("#endif /* TAIRIX_APPINFO_H */\n");
    out
}

/// The C mirror of [`AppInfoHeader`], field by field in **wire order**: the
/// C declaration and the number of bytes it occupies.
///
/// `AppInfoHeader`'s declaration order is its wire order (`lib/abi` pins that
/// field by field), so emitting the mirror from one ordered table and
/// checking the widths sum to [`AppInfoHeader::WIRE_LEN`] is what stops the C
/// view drifting into a differently-shaped struct of the same name — which is
/// exactly what it had done: it was still declaring a three-byte `reserved0`
/// and carrying neither `purpose` nor `author`, so a third-party program
/// filling it in would have written a manifest the loader could not read.
/// Array extents are spelled with the same macros the header defines from
/// `lib/abi`, so an extent and its byte count cannot disagree either.
const APPINFO_HEADER_FIELDS: &[(&str, usize)] = &[
    ("uint32_t magic", 4),
    ("uint32_t abi_version", 4),
    ("uint32_t flags", 4),
    ("uint16_t capability_count", 2),
    ("uint16_t mime_count", 2),
    ("uint8_t id_len", 1),
    ("uint8_t name_len", 1),
    ("uint8_t version_len", 1),
    ("uint8_t library_icon_len", 1),
    ("uint8_t library", 1),
    ("uint8_t purpose_len", 1),
    ("uint8_t author_len", 1),
    ("uint8_t reserved0[1]", 1),
    ("uint8_t id[TAIRIX_BUNDLE_ID_MAX]", BUNDLE_ID_MAX),
    ("uint8_t name[TAIRIX_BUNDLE_NAME_MAX]", BUNDLE_NAME_MAX),
    (
        "uint8_t version[TAIRIX_BUNDLE_VERSION_MAX]",
        BUNDLE_VERSION_MAX,
    ),
    (
        "uint8_t library_icon[TAIRIX_LIBRARY_ICON_MAX]",
        LIBRARY_ICON_MAX,
    ),
    (
        "uint8_t purpose[TAIRIX_BUNDLE_PURPOSE_MAX]",
        BUNDLE_PURPOSE_MAX,
    ),
    (
        "uint8_t author[TAIRIX_BUNDLE_AUTHOR_MAX]",
        BUNDLE_AUTHOR_MAX,
    ),
    (
        "uint8_t syscall_table_hash[TAIRIX_SYSCALL_TABLE_HASH_LEN]",
        SYSCALL_TABLE_HASH_LEN,
    ),
    ("uint8_t content_hash[32]", 32),
    ("uint8_t signer_pubkey[32]", 32),
    ("uint8_t publisher_pubkey[32]", 32),
    ("uint8_t publisher_cert[64]", 64),
    ("uint8_t signature[64]", 64),
];

/// Emit the program-library listing wire bytes (the `AppInfoHeader::library`
/// field's vocabulary): `TAIRIX_APPINFO_LIBRARY_NONE` for a bundle the
/// desktop's launcher never lists, plus one constant per closed
/// [`LibraryCategory`] folder — every value read from `lib/abi`'s own wire
/// encoding, never re-typed.
fn emit_library_listing_bytes(out: &mut String) {
    use std::fmt::Write as _;

    out.push_str(
        "/* Program-library listing wire byte (`library` field): not listed, or the\n\
         \x20* folder the bundle files itself under (uint8_t). */\n",
    );
    let _ = writeln!(out, "#define TAIRIX_APPINFO_LIBRARY_NONE ((uint8_t)0u)");
    for category in LibraryCategory::ALL {
        let _ = writeln!(
            out,
            "#define TAIRIX_APPINFO_LIBRARY_{} ((uint8_t){}u)",
            category.as_str().to_ascii_uppercase(),
            LibraryCategory::to_wire(Some(category))
        );
    }
    out.push('\n');
}

/// `tairix_rxe.h` — the `rxe` load-image table and load-time hardening
/// policy.
///
/// `tairix_load_header_t` mirrors the `#[repr(C)]` layout of [`LoadHeader`] (the
/// fixed image prefix; naturally aligned, so the struct size equals the wire
/// size). A [`Segment`] record is hand-serialised, so the header exports its
/// packed wire size (`TAIRIX_SEGMENT_WIRE_LEN`) and the `TAIRIX_SEG_FLAG_*` field
/// codes rather than a C struct mirror. Alongside them the header declares the
/// `TAIRIX_LOAD_MAGIC` / `TAIRIX_RXE_PAGE_SIZE` / `TAIRIX_LOAD_MAX_SEGMENTS` /
/// `TAIRIX_LOAD_FLAG_PIE` constants and the [`RxePermission`] discriminants.
/// Every numeric value and discriminant is read from `lib/abi`, never
/// re-typed; only the C spelling lives here.
fn generate_rxe() -> String {
    use std::fmt::Write as _;
    let mut out =
        banner("rxe load-image table and load-time hardening (AGENTS.md sec.9, sec.19.2).");
    out.push_str("#ifndef TAIRIX_RXE_H\n#define TAIRIX_RXE_H\n\n");
    out.push_str("#include <stdint.h>\n\n");

    out.push_str("/* Magic word identifying an abi-v1 load header (\"RXEL\" little-endian). */\n");
    let _ = writeln!(out, "#define TAIRIX_LOAD_MAGIC {LOAD_MAGIC:#x}u");
    out.push_str("/* Page size the load image is expressed in, in bytes. */\n");
    let _ = writeln!(
        out,
        "#define TAIRIX_RXE_PAGE_SIZE ((uint64_t){RXE_PAGE_SIZE}ull)"
    );
    out.push_str("/* Maximum number of segment records a single load image may carry. */\n");
    let _ = writeln!(
        out,
        "#define TAIRIX_LOAD_MAX_SEGMENTS ((uintptr_t){LOAD_MAX_SEGMENTS}u)"
    );
    out.push_str("/* Maximum number of needed-library references a load image may declare. */\n");
    let _ = writeln!(
        out,
        "#define TAIRIX_LOAD_MAX_NEEDED ((uintptr_t){LOAD_MAX_NEEDED}u)"
    );
    out.push_str("/* Maximum length, in bytes, of a needed-library reference path. */\n");
    let _ = writeln!(out, "#define TAIRIX_LIBREF_MAX ((uintptr_t){LIBREF_MAX}u)");
    out.push('\n');

    out.push_str("/* Load-header flag bits (uint32_t). Every undefined bit must be zero. */\n");
    out.push_str("/* The image is position-independent (PIE); required by sec.19.2. */\n");
    let _ = writeln!(out, "#define TAIRIX_LOAD_FLAG_PIE {LOAD_FLAG_PIE:#x}u");
    out.push('\n');

    out.push_str("/* Segment flag bits (uint32_t) in a packed segment record. */\n");
    let _ = writeln!(out, "#define TAIRIX_SEG_FLAG_READ {SEG_FLAG_READ:#x}u");
    let _ = writeln!(out, "#define TAIRIX_SEG_FLAG_WRITE {SEG_FLAG_WRITE:#x}u");
    let _ = writeln!(out, "#define TAIRIX_SEG_FLAG_EXEC {SEG_FLAG_EXEC:#x}u");
    out.push('\n');

    out.push_str("/* Packed little-endian wire size of a load header, in bytes. */\n");
    let _ = writeln!(
        out,
        "#define TAIRIX_LOAD_HEADER_WIRE_LEN {}u",
        LoadHeader::WIRE_LEN
    );
    out.push_str("/* Packed little-endian wire size of one segment record, in bytes. */\n");
    let _ = writeln!(
        out,
        "#define TAIRIX_SEGMENT_WIRE_LEN {}u",
        Segment::WIRE_LEN
    );
    out.push_str("/* Packed little-endian wire size of one needed-library record, in bytes. */\n");
    let _ = writeln!(
        out,
        "#define TAIRIX_NEEDED_LIBRARY_WIRE_LEN {}u",
        NeededLibrary::WIRE_LEN
    );
    out.push('\n');

    out.push_str("/* W^X-clean permission a segment is mapped with (uint8_t). */\n");
    let _ = writeln!(
        out,
        "#define TAIRIX_RXE_PERMISSION_READ_ONLY ((uint8_t){}u)",
        RxePermission::ReadOnly as u8
    );
    let _ = writeln!(
        out,
        "#define TAIRIX_RXE_PERMISSION_READ_EXECUTE ((uint8_t){}u)",
        RxePermission::ReadExecute as u8
    );
    let _ = writeln!(
        out,
        "#define TAIRIX_RXE_PERMISSION_READ_WRITE ((uint8_t){}u)",
        RxePermission::ReadWrite as u8
    );
    out.push('\n');

    out.push_str(
        "/* Fixed rxe load-image prefix; encoded little-endian on the wire. */\n\
         typedef struct tairix_load_header {\n\
         \x20   uint32_t magic;\n\
         \x20   uint32_t abi_version;\n\
         \x20   uint32_t flags;\n\
         \x20   uint16_t segment_count;\n\
         \x20   uint16_t needed_count;\n\
         \x20   uint64_t entry;\n",
    );
    let _ = writeln!(out, "\x20   uint8_t cfi_tag[{SYSCALL_TABLE_HASH_LEN}];");
    out.push_str("} tairix_load_header_t;\n\n");

    out.push_str("#endif /* TAIRIX_RXE_H */\n");
    out
}

/// `tairix_process.h` — the process startup vector the kernel hands a freshly
/// spawned program (`plans/CCOMPAT.md` CC3).
///
/// `tairix_process_start_header_t` mirrors the `#[repr(C)]` layout of
/// [`ProcessStartHeader`] (the fixed block prefix; naturally aligned, so the
/// struct size equals the wire size) and `tairix_string_slot_t` that of
/// [`StringSlot`] (one `(offset, len)` reference into the block's string
/// region). Alongside them the header declares the `TAIRIX_PROCESS_START_MAGIC`
/// magic, the `TAIRIX_PROCESS_START_MAX_*` limits, and the packed `*_WIRE_LEN`
/// sizes. Every numeric value is read from `lib/abi`, never re-typed; only the
/// C spelling lives here.
fn generate_process() -> String {
    use std::fmt::Write as _;
    let mut out = banner(
        "Process startup vector handed to a freshly spawned program \
         (AGENTS.md sec.16.5; plans/CCOMPAT.md CC3).",
    );
    out.push_str("#ifndef TAIRIX_PROCESS_H\n#define TAIRIX_PROCESS_H\n\n");
    out.push_str("#include <stdint.h>\n\n");

    out.push_str(
        "/* Magic word identifying an abi-v1 startup-vector block (\"PSV1\" little-endian). */\n",
    );
    let _ = writeln!(
        out,
        "#define TAIRIX_PROCESS_START_MAGIC {PROCESS_START_MAGIC:#x}u"
    );
    out.push_str(
        "/* Maximum number of strings (arguments + environment entries) a vector may carry. */\n",
    );
    let _ = writeln!(
        out,
        "#define TAIRIX_PROCESS_START_MAX_STRINGS {PROCESS_START_MAX_STRINGS}u"
    );
    out.push_str("/* Maximum length, in bytes, of one argument or environment string. */\n");
    let _ = writeln!(
        out,
        "#define TAIRIX_PROCESS_START_MAX_STRING_LEN {PROCESS_START_MAX_STRING_LEN}u"
    );
    out.push_str("/* Maximum total size, in bytes, of a startup-vector block. */\n");
    let _ = writeln!(
        out,
        "#define TAIRIX_PROCESS_START_MAX_TOTAL_LEN ((uint64_t){PROCESS_START_MAX_TOTAL_LEN}ull)"
    );
    out.push('\n');

    out.push_str(
        "/* `console` argument to tairix_sys_spawn: attach the child to the caller's own\n\
         \x20* console (any other value names an installed console index, see\n\
         \x20* tairix_sys_console_count). */\n",
    );
    let _ = writeln!(
        out,
        "#define TAIRIX_CONSOLE_INHERIT ((uint64_t){CONSOLE_INHERIT:#x}ull)"
    );
    out.push_str(
        "/* `target_uid` argument to tairix_sys_spawn: start the child under the\n\
         \x20* caller's own credential (any other value switches to that user, which\n\
         \x20* requires TAIRIX_CAP_SPAWN_AS_USER). */\n",
    );
    let _ = writeln!(
        out,
        "#define TAIRIX_SPAWN_UID_INHERIT ((uint32_t){SPAWN_UID_INHERIT:#x}u)"
    );
    out.push_str(
        "/* `stack_len` argument to tairix_sys_thread_create: give the new thread the\n\
         \x20* kernel's default per-thread stack (the caller's effective stack-bytes\n\
         \x20* bound) instead of naming a size. */\n",
    );
    let _ = writeln!(
        out,
        "#define TAIRIX_THREAD_STACK_DEFAULT ((uintptr_t){THREAD_STACK_DEFAULT}u)"
    );
    out.push('\n');

    out.push_str("/* Packed little-endian wire size of a startup-vector header, in bytes. */\n");
    let _ = writeln!(
        out,
        "#define TAIRIX_PROCESS_START_HEADER_WIRE_LEN {}u",
        ProcessStartHeader::WIRE_LEN
    );
    out.push_str("/* Packed little-endian wire size of one string slot, in bytes. */\n");
    let _ = writeln!(
        out,
        "#define TAIRIX_STRING_SLOT_WIRE_LEN {}u",
        StringSlot::WIRE_LEN
    );
    out.push('\n');

    out.push_str(
        "/* One string's (offset, len) reference into the block; encoded little-endian. */\n\
         typedef struct tairix_string_slot {\n\
         \x20   uint32_t offset;\n\
         \x20   uint32_t len;\n\
         } tairix_string_slot_t;\n\n",
    );
    out.push_str(
        "/* Fixed startup-vector block prefix; followed by the slot table then string data. */\n\
         typedef struct tairix_process_start_header {\n\
         \x20   uint32_t magic;\n\
         \x20   uint32_t abi_version;\n\
         \x20   uint32_t arg_count;\n\
         \x20   uint32_t env_count;\n\
         \x20   uint64_t total_len;\n\
         \x20   uint64_t canary;\n\
         \x20   uint64_t cpu_features;\n\
         } tairix_process_start_header_t;\n\n",
    );

    out.push_str("#endif /* TAIRIX_PROCESS_H */\n");
    out
}

/// `tairix_sysinfo.h` — the System Information API surface.
///
/// Declares the `sysinfo-v1` framing (`TAIRIX_SYSINFO_VERSION_*` /
/// `TAIRIX_SYSINFO_REQUEST_MAGIC` / `TAIRIX_SYSINFO_MAX_PAYLOAD_LEN`), the
/// [`SysinfoQueryId`] well-known identifiers and their `TAIRIX_SYSINFO_QUERY_ID_MAX`
/// ceiling, the canonical registry-encoding constants
/// (`TAIRIX_SYSINFO_QUERY_NAME_MAX` / `_RECORD_LEN` / `_ENCODED_QUERY_TABLE_LEN`),
/// the [`ProcessState`] `#[repr(u8)]` discriminants, the inline-buffer size
/// limits (`TAIRIX_PROCESS_NAME_MAX`, `TAIRIX_MACHINE_ID_LEN`, `TAIRIX_HOSTNAME_MAX`,
/// `TAIRIX_MOUNT_*_MAX`), and a `#[repr(C)]` C struct mirror plus a packed
/// `*_WIRE_LEN` macro for each of the nine wire types
/// ([`SysinfoRequestHeader`], [`ProcessListRequest`], [`ProcessRecord`],
/// [`KernelMemoryStats`], [`Uptime`], [`SystemIdentity`], [`MountListRequest`],
/// [`MountRecord`], [`ResourceLimitRecord`]). [`Uptime`]'s members are the
/// `tairix_duration64_t` / `tairix_time64_t` types from `tairix_time.h`; a
/// [`ResourceLimitRecord`]'s `limit` is the `tairix_resource_limit_t` from
/// `tairix_rlimit.h`. Every numeric value and
/// discriminant is read from `lib/abi`, never re-typed; only the C spelling
/// lives here.
fn generate_sysinfo() -> String {
    let mut out = banner("System Information API surface (AGENTS.md sec.16.6).");
    out.push_str("#ifndef TAIRIX_SYSINFO_H\n#define TAIRIX_SYSINFO_H\n\n");
    out.push_str("#include <stdint.h>\n");
    out.push_str("#include \"tairix_time.h\"\n");
    out.push_str("#include \"tairix_rlimit.h\"\n");
    out.push_str("#include \"tairix_driver.h\"\n\n");

    sysinfo_emit_framing(&mut out);
    sysinfo_emit_record_sizes(&mut out);
    out.push_str(SYSINFO_RECORD_TYPEDEFS);
    out.push_str("#endif /* TAIRIX_SYSINFO_H */\n");
    out
}

/// Emit the sysinfo framing, registry-encoding, query-id, and process-state
/// constants (every value read from `lib/abi`).
fn sysinfo_emit_framing(out: &mut String) {
    use std::fmt::Write as _;
    out.push_str("/* sysinfo protocol version tag for the frozen v1 surface. */\n");
    let _ = writeln!(
        out,
        "#define TAIRIX_SYSINFO_VERSION_V1 {SYSINFO_VERSION_V1}u"
    );
    out.push_str("/* sysinfo protocol version this header set describes. */\n");
    let _ = writeln!(
        out,
        "#define TAIRIX_SYSINFO_VERSION_CURRENT {SYSINFO_VERSION_CURRENT}u"
    );
    out.push_str("/* Magic word identifying a sysinfo-v1 request (\"SYI1\" little-endian). */\n");
    let _ = writeln!(
        out,
        "#define TAIRIX_SYSINFO_REQUEST_MAGIC {SYSINFO_REQUEST_MAGIC:#x}u"
    );
    out.push_str(
        "/* Maximum request/response payload length, in bytes, a header may advertise. */\n",
    );
    let _ = writeln!(
        out,
        "#define TAIRIX_SYSINFO_MAX_PAYLOAD_LEN {SYSINFO_MAX_PAYLOAD_LEN}u"
    );
    out.push_str("/* Inclusive upper bound on the sysinfo-v1 query identifier space. */\n");
    let _ = writeln!(
        out,
        "#define TAIRIX_SYSINFO_QUERY_ID_MAX {}u",
        SysinfoQueryId::MAX
    );
    out.push('\n');

    out.push_str(
        "/* Canonical query-registry encoding constants (the hashable registry image). */\n",
    );
    let _ = writeln!(
        out,
        "#define TAIRIX_SYSINFO_QUERY_NAME_MAX {SYSINFO_QUERY_NAME_MAX}u"
    );
    let _ = writeln!(
        out,
        "#define TAIRIX_SYSINFO_QUERY_RECORD_LEN {SYSINFO_QUERY_RECORD_LEN}u"
    );
    let _ = writeln!(
        out,
        "#define TAIRIX_SYSINFO_ENCODED_QUERY_TABLE_LEN {ENCODED_QUERY_TABLE_LEN}u"
    );
    let _ = writeln!(
        out,
        "#define TAIRIX_SYSINFO_LOAD_FIXED_SHIFT {}u",
        tairix_abi::LOAD_FIXED_SHIFT
    );
    out.push('\n');

    sysinfo_emit_query_ids(out);

    out.push_str("/* Process lifecycle state carried in a process record (uint8_t). */\n");
    let process_states = [
        ("TAIRIX_PROCESS_STATE_RUNNABLE", ProcessState::Runnable),
        ("TAIRIX_PROCESS_STATE_RUNNING", ProcessState::Running),
        ("TAIRIX_PROCESS_STATE_BLOCKED", ProcessState::Blocked),
        ("TAIRIX_PROCESS_STATE_ZOMBIE", ProcessState::Zombie),
        ("TAIRIX_PROCESS_STATE_STOPPED", ProcessState::Stopped),
    ];
    for (name, state) in process_states {
        let _ = writeln!(out, "#define {name} ((uint8_t){}u)", state.as_u8());
    }
    out.push_str(
        "/* tairix_process_record.cpu sentinel: the process is not currently scheduled. */\n",
    );
    let _ = writeln!(
        out,
        "#define TAIRIX_PROCESS_CPU_NONE ((uint8_t){PROCESS_CPU_NONE}u)"
    );
    out.push('\n');
}

/// Emit the well-known `sysinfo-v1` query identifiers (every value read
/// from `lib/abi`'s [`SysinfoQueryId`] constants).
fn sysinfo_emit_query_ids(out: &mut String) {
    use std::fmt::Write as _;
    out.push_str("/* Well-known sysinfo-v1 query identifiers (uint16_t). Do not renumber. */\n");
    // One macro per registry row, named from the row's own stable name,
    // so a query added to `lib/abi` appears here with no edit and the
    // header cannot fall behind the registry it is generated from.
    for spec in SYSINFO_QUERIES {
        let _ = writeln!(
            out,
            "#define TAIRIX_SYSINFO_QUERY_{} ((uint16_t){}u)",
            spec.name.to_ascii_uppercase(),
            spec.id.as_u16()
        );
    }
    out.push('\n');
}

/// Emit the storage-medium discriminants a mount record carries.
///
/// Each value is read from the ABI's own encoder rather than re-typed here,
/// so the C view cannot drift from the Rust definition it publishes.
fn sysinfo_emit_mount_media(out: &mut String) {
    use std::fmt::Write as _;
    out.push_str(
        "/* Storage medium of the block device backing a mount (uint8_t).\n\
         \x20  UNKNOWN covers both a mount with no block backing and a class this\n\
         \x20  ABI does not define: the record never guesses a medium. */\n",
    );
    let mount_media = [
        ("TAIRIX_MOUNT_MEDIUM_UNKNOWN", None),
        (
            "TAIRIX_MOUNT_MEDIUM_ROTATIONAL",
            Some(BlkDeviceClass::Rotational),
        ),
        (
            "TAIRIX_MOUNT_MEDIUM_SOLID_STATE",
            Some(BlkDeviceClass::SolidState),
        ),
        (
            "TAIRIX_MOUNT_MEDIUM_REMOVABLE",
            Some(BlkDeviceClass::Removable),
        ),
        ("TAIRIX_MOUNT_MEDIUM_VIRTUAL", Some(BlkDeviceClass::Virtual)),
    ];
    for (name, medium) in mount_media {
        let _ = writeln!(
            out,
            "#define {name} ((uint8_t){}u)",
            MountRecord::medium_to_wire(medium)
        );
    }
}

/// Emit the inline-buffer capacities and the per-record packed wire sizes.
fn sysinfo_emit_record_sizes(out: &mut String) {
    use std::fmt::Write as _;
    out.push_str("/* Inline fixed-buffer capacities carried in the record types below. */\n");
    let _ = writeln!(out, "#define TAIRIX_PROCESS_NAME_MAX {PROCESS_NAME_MAX}u");
    let _ = writeln!(out, "#define TAIRIX_MACHINE_ID_LEN {MACHINE_ID_LEN}u");
    let _ = writeln!(out, "#define TAIRIX_HOSTNAME_MAX {HOSTNAME_MAX}u");
    let _ = writeln!(out, "#define TAIRIX_MOUNT_SOURCE_MAX {MOUNT_SOURCE_MAX}u");
    let _ = writeln!(out, "#define TAIRIX_MOUNT_TARGET_MAX {MOUNT_TARGET_MAX}u");
    let _ = writeln!(out, "#define TAIRIX_MOUNT_FSTYPE_MAX {MOUNT_FSTYPE_MAX}u");
    let _ = writeln!(
        out,
        "#define TAIRIX_MOUNT_VOLUME_ID_LEN {MOUNT_VOLUME_ID_LEN}u"
    );
    out.push_str("/* Mount availability carried in a mount record (uint8_t). */\n");
    let mount_availabilities = [
        ("TAIRIX_MOUNT_AVAILABLE", MountAvailability::Available),
        (
            "TAIRIX_MOUNT_UNAVAILABLE_DIRTY",
            MountAvailability::UnavailableDirty,
        ),
        (
            "TAIRIX_MOUNT_UNAVAILABLE_LOST",
            MountAvailability::UnavailableLost,
        ),
        (
            "TAIRIX_MOUNT_RECOVERY_CONFLICT",
            MountAvailability::RecoveryConflict,
        ),
        ("TAIRIX_MOUNT_DEGRADED", MountAvailability::Degraded),
        ("TAIRIX_MOUNT_RECOVERING", MountAvailability::Recovering),
    ];
    for (name, state) in mount_availabilities {
        let _ = writeln!(out, "#define {name} ((uint8_t){}u)", state.as_u8());
    }
    sysinfo_emit_mount_media(out);
    let _ = writeln!(
        out,
        "#define TAIRIX_USER_DIRECTORY_NAME_MAX {USER_DIRECTORY_NAME_MAX}u"
    );
    out.push('\n');

    out.push_str("/* Packed little-endian wire size of each sysinfo record type, in bytes. */\n");
    let wire_lens = [
        (
            "TAIRIX_SYSINFO_REQUEST_HEADER_WIRE_LEN",
            SysinfoRequestHeader::WIRE_LEN,
        ),
        (
            "TAIRIX_PROCESS_LIST_REQUEST_WIRE_LEN",
            ProcessListRequest::WIRE_LEN,
        ),
        ("TAIRIX_PROCESS_RECORD_WIRE_LEN", ProcessRecord::WIRE_LEN),
        (
            "TAIRIX_KERNEL_MEMORY_STATS_WIRE_LEN",
            KernelMemoryStats::WIRE_LEN,
        ),
        ("TAIRIX_UPTIME_WIRE_LEN", Uptime::WIRE_LEN),
        ("TAIRIX_LOAD_AVERAGE_WIRE_LEN", LoadAverage::WIRE_LEN),
        ("TAIRIX_SYSTEM_IDENTITY_WIRE_LEN", SystemIdentity::WIRE_LEN),
        (
            "TAIRIX_MOUNT_LIST_REQUEST_WIRE_LEN",
            MountListRequest::WIRE_LEN,
        ),
        ("TAIRIX_MOUNT_RECORD_WIRE_LEN", MountRecord::WIRE_LEN),
        (
            "TAIRIX_RESOURCE_LIMIT_RECORD_WIRE_LEN",
            ResourceLimitRecord::WIRE_LEN,
        ),
        (
            "TAIRIX_USER_DIRECTORY_REQUEST_WIRE_LEN",
            UserDirectoryRequest::WIRE_LEN,
        ),
        (
            "TAIRIX_USER_DIRECTORY_RECORD_WIRE_LEN",
            UserDirectoryRecord::WIRE_LEN,
        ),
    ];
    for (name, len) in wire_lens {
        let _ = writeln!(out, "#define {name} {len}u");
    }
    out.push('\n');
    out.push_str(
        "/* Byte length of a full RESOURCE_LIMITS response: one record per LimitKind. */\n",
    );
    let _ = writeln!(
        out,
        "#define TAIRIX_SYSINFO_RESOURCE_LIMITS_REPORT_LEN {RESOURCE_LIMITS_REPORT_LEN}u"
    );
    out.push('\n');
}

/// The C struct mirrors of the eleven `#[repr(C)]` System Information wire
/// types, as static text (the field names/order are part of the frozen ABI
/// view; the in-module pinning test checks the layout against `lib/abi`).
const SYSINFO_RECORD_TYPEDEFS: &str = concat!(
    "/* Envelope prefixing every sysinfo request; encoded little-endian on the wire. */\n\
         typedef struct tairix_sysinfo_request_header {\n\
         \x20   uint32_t magic;\n\
         \x20   uint16_t version;\n\
         \x20   uint16_t flags;\n\
         \x20   uint16_t query;\n\
         \x20   uint16_t reserved;\n\
         \x20   uint32_t payload_len;\n\
         \x20   uint64_t request_id;\n\
         } tairix_sysinfo_request_header_t;\n\n",
    "/* Process-list request payload (offset/limit paging). */\n\
         typedef struct tairix_process_list_request {\n\
         \x20   uint32_t offset;\n\
         \x20   uint16_t limit;\n\
         \x20   uint16_t flags;\n\
         } tairix_process_list_request_t;\n\n",
    "/* One process entry. The numeric pid/parent_pid are reused across process\n\
         * lifetimes; proc_id/parent_proc_id are the kernel-attested, never-reused\n\
         * process-instance identities (correlate on those, not the numeric ids).\n\
         * `cpu` is TAIRIX_PROCESS_CPU_NONE when the process is not currently\n\
         * scheduled; `priority` is the TAIRIX_SCHED_PRIORITY_* time-shared service\n\
         * level (tairix_syscall.h); cpu_time_ns is the cumulative on-CPU time and\n\
         * mem_bytes the mapped address-space size. io_bytes_read/io_bytes_written\n\
         * are the bytes this process's own file reads/writes actually transferred\n\
         * over its whole lifetime (the quantity Linux reports as rchar/wchar),\n\
         * never block-device traffic and never the byte count a caller asked for;\n\
         * both saturate at UINT64_MAX. The inline name is valid for\n\
         * name_len bytes. */\n\
         typedef struct tairix_process_record {\n\
         \x20   uint64_t pid;\n\
         \x20   uint64_t parent_pid;\n\
         \x20   uint8_t proc_id[16];\n\
         \x20   uint8_t parent_proc_id[16];\n\
         \x20   uint32_t uid;\n\
         \x20   uint32_t gid;\n\
         \x20   uint8_t state;\n\
         \x20   uint8_t cpu;\n\
         \x20   uint32_t priority;\n\
         \x20   uint64_t cpu_time_ns;\n\
         \x20   uint64_t mem_bytes;\n\
         \x20   uint64_t io_bytes_read;\n\
         \x20   uint64_t io_bytes_written;\n\
         \x20   uint8_t name_len;\n\
         \x20   uint8_t name[TAIRIX_PROCESS_NAME_MAX];\n\
         } tairix_process_record_t;\n\n",
    "/* Kernel memory statistics response. */\n\
         typedef struct tairix_kernel_memory_stats {\n\
         \x20   uint64_t total_bytes;\n\
         \x20   uint64_t free_bytes;\n\
         \x20   uint64_t kernel_heap_bytes;\n\
         \x20   uint64_t user_resident_bytes;\n\
         \x20   uint32_t page_size;\n\
         \x20   uint32_t reserved;\n\
         } tairix_kernel_memory_stats_t;\n\n",
    "/* Uptime response: monotonic span since boot + wall-clock boot instant. */\n\
         typedef struct tairix_uptime {\n\
         \x20   tairix_duration64_t since_boot;\n\
         \x20   tairix_time64_t boot_time;\n\
         } tairix_uptime_t;\n\n",
    "/* Load-average response; load1/5/15 are fixed-point with\n\
         \x20  TAIRIX_SYSINFO_LOAD_FIXED_SHIFT fractional bits. */\n\
         typedef struct tairix_load_average {\n\
         \x20   uint32_t load1;\n\
         \x20   uint32_t load5;\n\
         \x20   uint32_t load15;\n\
         \x20   uint32_t runnable;\n\
         \x20   uint32_t total_tasks;\n\
         \x20   uint32_t users;\n\
         } tairix_load_average_t;\n\n",
    "/* Machine identity response; the inline hostname is valid for hostname_len bytes. */\n\
         typedef struct tairix_system_identity {\n\
         \x20   uint8_t machine_id[TAIRIX_MACHINE_ID_LEN];\n\
         \x20   uint16_t version_major;\n\
         \x20   uint16_t version_minor;\n\
         \x20   uint16_t version_patch;\n\
         \x20   uint8_t hostname_len;\n\
         \x20   uint8_t hostname[TAIRIX_HOSTNAME_MAX];\n\
         } tairix_system_identity_t;\n\n",
    "/* Mount-list request payload (offset/limit paging). */\n\
         typedef struct tairix_mount_list_request {\n\
         \x20   uint32_t offset;\n\
         \x20   uint16_t limit;\n\
         \x20   uint16_t flags;\n\
         } tairix_mount_list_request_t;\n\n",
    "/* One mount-table entry. `flags` is a MountFlags bitmap (AGENTS.md sec.5.3);\n\
         * its flag bits are defined by the filesystem driver ABI. `availability` is\n\
         * a TAIRIX_MOUNT_* state (a surprise-removed volume never reads as healthy).\n\
         * `medium` is the storage medium of the block device backing the mount, a\n\
         * TAIRIX_MOUNT_MEDIUM_* value; TAIRIX_MOUNT_MEDIUM_UNKNOWN means no block\n\
         * device backs it or its class was not recognised -- never a guess.\n\
         * `usage` is the backing volume's space accounting (all-zero when none is\n\
         * known). `volume_id` is the volume's stable published identity (all-zero\n\
         * when the mount has none), the identity a volume_detach request names.\n\
         * The inline source/target/fstype buffers are valid for their respective\n\
         * *_len byte counts. */\n\
         typedef struct tairix_mount_record {\n\
         \x20   uint32_t flags;\n\
         \x20   uint8_t source_len;\n\
         \x20   uint8_t target_len;\n\
         \x20   uint8_t fstype_len;\n\
         \x20   uint8_t availability;\n\
         \x20   uint8_t medium;\n\
         \x20   uint8_t reserved0[7];\n\
         \x20   tairix_volume_stats_t usage;\n\
         \x20   uint8_t volume_id[TAIRIX_MOUNT_VOLUME_ID_LEN];\n\
         \x20   uint8_t source[TAIRIX_MOUNT_SOURCE_MAX];\n\
         \x20   uint8_t target[TAIRIX_MOUNT_TARGET_MAX];\n\
         \x20   uint8_t fstype[TAIRIX_MOUNT_FSTYPE_MAX];\n\
         } tairix_mount_record_t;\n\n",
    "/* One row of the RESOURCE_LIMITS response: a resource's effective soft/hard\n\
         * bound (a tairix_resource_limit_t) and the caller's current live usage of it.\n\
         * The full response is TAIRIX_LIMIT_KIND_COUNT records in LimitKind order. */\n\
         typedef struct tairix_resource_limit_record {\n\
         \x20   uint32_t kind;\n\
         \x20   uint32_t reserved;\n\
         \x20   tairix_resource_limit_t limit;\n\
         \x20   uint64_t usage;\n\
         } tairix_resource_limit_record_t;\n\n",
    "/* User-directory request payload (offset/limit paging). */\n\
         typedef struct tairix_user_directory_request {\n\
         \x20   uint32_t offset;\n\
         \x20   uint16_t limit;\n\
         \x20   uint16_t flags;\n\
         } tairix_user_directory_request_t;\n\n",
    "/* One account entry: the uid + username pairing, and nothing else (no\n\
         * credential material). The inline name is valid for name_len bytes. */\n\
         typedef struct tairix_user_directory_record {\n\
         \x20   uint32_t uid;\n\
         \x20   uint8_t name_len;\n\
         \x20   uint8_t name[TAIRIX_USER_DIRECTORY_NAME_MAX];\n\
         } tairix_user_directory_record_t;\n\n",
);

/// Emit the driver-manifest magic / count / key-length / wire-size constants
/// (every value read from `lib/abi`).
fn driver_emit_constants(out: &mut String) {
    use std::fmt::Write as _;
    out.push_str(
        "/* Magic word identifying an abi-v1 driver manifest (\"DRV1\" little-endian). */\n",
    );
    let _ = writeln!(
        out,
        "#define TAIRIX_DRIVER_MANIFEST_MAGIC {DRIVER_MANIFEST_MAGIC:#x}u"
    );
    out.push_str("/* Maximum number of capability identifiers a driver manifest may request. */\n");
    let _ = writeln!(
        out,
        "#define TAIRIX_DRIVER_MANIFEST_MAX_CAPABILITIES {DRIVER_MANIFEST_MAX_CAPABILITIES}u"
    );
    out.push_str("/* Maximum number of bind-table entries a driver manifest may declare. */\n");
    let _ = writeln!(
        out,
        "#define TAIRIX_DRIVER_MANIFEST_MAX_BIND_KEYS {DRIVER_MANIFEST_MAX_BIND_KEYS}u"
    );
    out.push_str("/* Length, in bytes, of the Ed25519 signer public key. */\n");
    let _ = writeln!(
        out,
        "#define TAIRIX_DRIVER_SIGNER_PUBKEY_LEN {DRIVER_SIGNER_PUBKEY_LEN}u"
    );
    out.push_str("/* Length, in bytes, of the Ed25519 manifest signature. */\n");
    let _ = writeln!(
        out,
        "#define TAIRIX_DRIVER_SIGNATURE_LEN {DRIVER_SIGNATURE_LEN}u"
    );
    out.push_str("/* Packed little-endian wire size of a driver manifest, in bytes. */\n");
    let _ = writeln!(
        out,
        "#define TAIRIX_DRIVER_MANIFEST_WIRE_LEN {}u",
        DriverManifest::WIRE_LEN
    );
    out.push_str("/* Packed little-endian wire size of one bind-table entry, in bytes. */\n");
    let _ = writeln!(
        out,
        "#define TAIRIX_DRIVER_BIND_KEY_WIRE_LEN {}u",
        DriverBindKey::WIRE_LEN
    );
    out.push('\n');

    out.push_str(
        "/* Magic word identifying an abi-v1 driver register reply (\"DRR1\" little-endian). */\n",
    );
    let _ = writeln!(
        out,
        "#define TAIRIX_DRIVER_REGISTER_REPLY_MAGIC {DRIVER_REGISTER_REPLY_MAGIC:#x}u"
    );
    out.push_str("/* `status` value of a successful register reply; any other value is a\n * TAIRIX_DRIVER_ERROR_* code. */\n");
    let _ = writeln!(
        out,
        "#define TAIRIX_DRIVER_REGISTER_STATUS_OK ((int32_t){DRIVER_REGISTER_STATUS_OK})"
    );
    out.push_str("/* Packed little-endian wire size of a driver register reply, in bytes. */\n");
    let _ = writeln!(
        out,
        "#define TAIRIX_DRIVER_REGISTER_REPLY_WIRE_LEN {}u",
        DriverRegisterReply::WIRE_LEN
    );
    out.push('\n');
}

/// Emit the [`DriverKind`] / [`BufferClass`] / [`DriverError`] discriminants
/// and the [`DriverHandle`] sentinel (every value read from `lib/abi`).
fn driver_emit_discriminants(out: &mut String) {
    use std::fmt::Write as _;
    out.push_str(
        "/* Driver execution domain (uint8_t); IN_KERNEL additionally needs CAP_DRV_KERNEL. */\n",
    );
    let _ = writeln!(
        out,
        "#define TAIRIX_DRIVER_KIND_USER_SPACE ((uint8_t){}u)",
        DriverKind::UserSpace.as_u8()
    );
    let _ = writeln!(
        out,
        "#define TAIRIX_DRIVER_KIND_IN_KERNEL ((uint8_t){}u)",
        DriverKind::InKernel.as_u8()
    );
    out.push('\n');

    out.push_str("/* Payload sensitivity hint (uint8_t); SENSITIVE requires zero-on-free. */\n");
    let _ = writeln!(
        out,
        "#define TAIRIX_BUFFER_CLASS_NON_SENSITIVE ((uint8_t){}u)",
        BufferClass::NonSensitive.as_u8()
    );
    let _ = writeln!(
        out,
        "#define TAIRIX_BUFFER_CLASS_SENSITIVE ((uint8_t){}u)",
        BufferClass::Sensitive.as_u8()
    );
    out.push('\n');

    out.push_str("/* Sentinel \"no driver handle\"; a live handle travels as a uint64_t. */\n");
    let _ = writeln!(
        out,
        "#define TAIRIX_DRIVER_HANDLE_NONE ((uint64_t){}ull)",
        DriverHandle::NONE.as_u64()
    );
    out.push('\n');

    out.push_str(
        "/* Stable driver-ABI error codes (int32_t), disjoint from TAIRIX_E_* errno. */\n",
    );
    for (name, err) in DRIVER_ERROR_NAMES {
        let _ = writeln!(
            out,
            "#define TAIRIX_DRIVER_ERROR_{name} ((int32_t){})",
            err.as_i32()
        );
    }
    out.push('\n');
}

/// Emit the driver-submodule POD constants: the `VIRTIO_PCI_*` ids, the
/// [`MountFlags`] bit set, and the [`NodeId`] sentinel (every value read
/// from `lib/abi`). The Ethernet address length is `tairix_hwtree.h`'s, which
/// this header includes — one definition.
///
/// The [`MountFlags`] bits live here — not in `tairix_sysinfo.h` where the
/// `MountRecord.flags` field is a bare `uint32_t` — because the flag
/// semantics are owned by the filesystem driver ABI.
///
/// [`MountFlags`]: tairix_abi::driver::filesystem::MountFlags
/// [`NodeId`]: tairix_abi::driver::filesystem::NodeId
fn driver_emit_submodule_constants(out: &mut String) {
    use std::fmt::Write as _;
    use tairix_abi::driver::filesystem::{MountFlags, NodeId};
    use tairix_abi::{
        VIRTIO_PCI_CFG_COMMON, VIRTIO_PCI_CFG_DEVICE, VIRTIO_PCI_CFG_ISR, VIRTIO_PCI_CFG_NOTIFY,
        VIRTIO_PCI_CFG_PCI, VIRTIO_PCI_VENDOR_ID,
    };

    out.push_str(
        "/* PCI vendor ID assigned to virtio devices (uint16_t; virtio 1.1 sec.4.1.2). */\n",
    );
    let _ = writeln!(
        out,
        "#define TAIRIX_VIRTIO_PCI_VENDOR_ID ((uint16_t){VIRTIO_PCI_VENDOR_ID:#x}u)"
    );
    out.push_str(
        "/* virtio PCI capability `cfg_type` values (uint8_t; virtio 1.1 sec.4.1.4). */\n",
    );
    for (name, value) in [
        ("COMMON", VIRTIO_PCI_CFG_COMMON),
        ("NOTIFY", VIRTIO_PCI_CFG_NOTIFY),
        ("ISR", VIRTIO_PCI_CFG_ISR),
        ("DEVICE", VIRTIO_PCI_CFG_DEVICE),
        ("PCI", VIRTIO_PCI_CFG_PCI),
    ] {
        let _ = writeln!(
            out,
            "#define TAIRIX_VIRTIO_PCI_CFG_{name} ((uint8_t){value}u)"
        );
    }
    out.push('\n');

    out.push_str(
        "/* Mount-flag bitmap (uint32_t); any bit outside KNOWN_MASK is reserved and rejected. */\n",
    );
    for (name, flag) in [
        ("READ_ONLY", MountFlags::READ_ONLY),
        ("NOSUID", MountFlags::NOSUID),
        ("NODEV", MountFlags::NODEV),
        ("NOEXEC", MountFlags::NOEXEC),
        ("KNOWN_MASK", MountFlags::KNOWN_MASK),
    ] {
        let _ = writeln!(
            out,
            "#define TAIRIX_MOUNT_FLAG_{name} ((uint32_t){:#x}u)",
            flag.bits()
        );
    }
    out.push('\n');

    out.push_str("/* Sentinel \"no node\"; a live NodeId travels as a uint64_t. */\n");
    let _ = writeln!(
        out,
        "#define TAIRIX_NODE_ID_NONE ((uint64_t){}ull)",
        NodeId::NONE.raw()
    );
    out.push('\n');
}

/// Emit the driver-submodule enum discriminants: [`DisplayFormat`],
/// [`NodeKind`], and the driver-class [`InputEventKind`] (every value read
/// from `lib/abi`).
///
/// The driver input-event kinds are spelled `TAIRIX_INPUT_EVENT_KIND_*` to
/// stay disjoint from the windowing `TAIRIX_INPUT_KIND_*` codes in
/// `tairix_input.h`; they are different ABIs that happen to share the word
/// "input".
///
/// [`DisplayFormat`]: tairix_abi::driver::display::DisplayFormat
/// [`NodeKind`]: tairix_abi::driver::filesystem::NodeKind
/// [`InputEventKind`]: tairix_abi::driver::input::InputEventKind
fn driver_emit_submodule_discriminants(out: &mut String) {
    use std::fmt::Write as _;
    use tairix_abi::driver::display::DisplayFormat;
    use tairix_abi::driver::filesystem::NodeKind;
    use tairix_abi::driver::input::InputEventKind;

    out.push_str(
        "/* Display pixel encoding (uint8_t); named by the byte order of the first pixel. */\n",
    );
    for (name, fmt) in [
        ("RGBA8888", DisplayFormat::Rgba8888),
        ("BGRA8888", DisplayFormat::Bgra8888),
    ] {
        let _ = writeln!(
            out,
            "#define TAIRIX_DISPLAY_FORMAT_{name} ((uint8_t){}u)",
            fmt.as_u8()
        );
    }
    out.push('\n');

    out.push_str("/* Filesystem node kind (uint8_t). */\n");
    let _ = writeln!(
        out,
        "#define TAIRIX_NODE_KIND_DIRECTORY ((uint8_t){}u)",
        NodeKind::Directory as u8
    );
    let _ = writeln!(
        out,
        "#define TAIRIX_NODE_KIND_REGULAR_FILE ((uint8_t){}u)",
        NodeKind::RegularFile as u8
    );
    let _ = writeln!(
        out,
        "#define TAIRIX_NODE_KIND_SYMLINK ((uint8_t){}u)",
        NodeKind::Symlink as u8
    );
    out.push('\n');

    out.push_str(
        "/* Driver input-event kind (uint8_t); distinct from the windowing TAIRIX_INPUT_KIND_*. */\n",
    );
    for (name, kind) in [
        ("KEY", InputEventKind::Key),
        ("POINTER", InputEventKind::Pointer),
        ("SCROLL", InputEventKind::Scroll),
    ] {
        let _ = writeln!(
            out,
            "#define TAIRIX_INPUT_EVENT_KIND_{name} ((uint8_t){}u)",
            kind.as_u8()
        );
    }
    out.push('\n');
}

/// `tairix_driver.h` — the driver-class ABI.
///
/// `tairix_driver_manifest_t` mirrors the `#[repr(C)]` layout of
/// [`DriverManifest`] (the signed driver-manifest prefix; naturally aligned,
/// so the struct size equals the wire size), `tairix_driver_bind_key_t` mirrors
/// [`DriverBindKey`] (one bind-table entry: a `tairix_hw_match_key_t` from
/// `tairix_hwtree.h` plus the bind priority), and
/// `tairix_driver_register_reply_t`
/// mirrors [`DriverRegisterReply`] (the register-handshake outcome a spawned
/// driver process reports to its host over IPC) with its
/// `TAIRIX_DRIVER_REGISTER_REPLY_MAGIC` / `_STATUS_OK` / `_WIRE_LEN` constants.
/// Alongside them the header declares
/// the `TAIRIX_DRIVER_MANIFEST_MAGIC` / `_MAX_CAPABILITIES` / `_MAX_BIND_KEYS` /
/// `_WIRE_LEN` / `TAIRIX_DRIVER_BIND_KEY_WIRE_LEN` and
/// signer-key/signature length constants, the [`DriverKind`] / [`BufferClass`]
/// `#[repr(u8)]` and [`DriverError`] `#[repr(i32)]` discriminant sets, and the
/// [`DriverHandle`] `TAIRIX_DRIVER_HANDLE_NONE` sentinel (a live driver handle
/// travels as a `uint64_t`). The syscall-table-hash length is shared with the
/// application manifest, so the struct reuses `TAIRIX_SYSCALL_TABLE_HASH_LEN` from
/// `tairix_manifest.h` rather than re-declaring it.
///
/// The header also carries the driver-class **submodule** POD surface: the
/// `VIRTIO_PCI_*` / [`MountFlags`] / [`NodeId`] constants
/// (see [`driver_emit_submodule_constants`]), the [`DisplayFormat`] /
/// [`NodeKind`] / [`InputEventKind`] discriminants (see
/// [`driver_emit_submodule_discriminants`]), and the struct mirrors in
/// [`DRIVER_SUBMODULE_TYPEDEFS`]. `NodeTimes` is built from `tairix_time64_t`, so
/// the header `#include`s `tairix_time.h`. Every numeric value and discriminant
/// is read from `lib/abi`, never re-typed; only the C spelling lives here.
///
/// [`MountFlags`]: tairix_abi::driver::filesystem::MountFlags
/// [`NodeId`]: tairix_abi::driver::filesystem::NodeId
/// [`DisplayFormat`]: tairix_abi::driver::display::DisplayFormat
/// [`NodeKind`]: tairix_abi::driver::filesystem::NodeKind
/// [`InputEventKind`]: tairix_abi::driver::input::InputEventKind
fn generate_driver() -> String {
    let mut out =
        banner("Driver-class ABI core: manifest, kinds, errors (AGENTS.md sec.8, sec.9).");
    out.push_str("#ifndef TAIRIX_DRIVER_H\n#define TAIRIX_DRIVER_H\n\n");
    out.push_str("#include <stdint.h>\n");
    out.push_str("#include \"tairix_hwtree.h\"\n");
    out.push_str("#include \"tairix_manifest.h\"\n");
    out.push_str("#include \"tairix_time.h\"\n\n");

    driver_emit_constants(&mut out);
    driver_emit_discriminants(&mut out);
    driver_emit_submodule_constants(&mut out);
    driver_emit_submodule_discriminants(&mut out);

    out.push_str(
        "/* Signed driver-manifest prefix; encoded little-endian on the wire. */\n\
         typedef struct tairix_driver_manifest {\n\
         \x20   uint32_t magic;\n\
         \x20   uint32_t abi_version;\n\
         \x20   uint8_t kind;\n\
         \x20   uint8_t bind_key_count;\n\
         \x20   uint16_t capability_count;\n\
         \x20   uint8_t syscall_table_hash[TAIRIX_SYSCALL_TABLE_HASH_LEN];\n\
         \x20   uint8_t signer_pubkey[TAIRIX_DRIVER_SIGNER_PUBKEY_LEN];\n\
         \x20   uint8_t signature[TAIRIX_DRIVER_SIGNATURE_LEN];\n\
         } tairix_driver_manifest_t;\n\n",
    );

    out.push_str(
        "/* One bind-table entry: a hardware-tree match key plus the manifest's\n\
         \x20* bind priority (AGENTS.md sec.18.3). bind_key_count entries follow the\n\
         \x20* capability body; all are covered by the manifest signature. */\n\
         typedef struct tairix_driver_bind_key {\n\
         \x20   uint16_t priority;\n\
         \x20   uint16_t reserved0;\n\
         \x20   tairix_hw_match_key_t key;\n\
         } tairix_driver_bind_key_t;\n\n",
    );

    out.push_str(
        "/* Outcome of a spawned driver process's register() entry, sent to the\n\
         \x20* driver host over IPC; encoded little-endian on the wire. `status` is\n\
         \x20* TAIRIX_DRIVER_REGISTER_STATUS_OK or a TAIRIX_DRIVER_ERROR_* code; `handle` is\n\
         \x20* non-zero exactly when `status` is OK (informational only — the host\n\
         \x20* mints its own unforgeable handle). */\n\
         typedef struct tairix_driver_register_reply {\n\
         \x20   uint32_t magic;\n\
         \x20   uint32_t abi_version;\n\
         \x20   int32_t status;\n\
         \x20   uint32_t reserved0;\n\
         \x20   uint64_t handle;\n\
         } tairix_driver_register_reply_t;\n\n",
    );

    out.push_str(DRIVER_SUBMODULE_TYPEDEFS);

    out.push_str("#endif /* TAIRIX_DRIVER_H */\n");
    out
}

/// The C struct mirrors of the driver-submodule `#[repr(C)]` POD types, as
/// static text. The field names/order are the frozen `abi-v1` view; the
/// in-module pinning test checks each mirror's size/align against `lib/abi`.
///
/// Every struct is the naturally-aligned in-memory layout. None of these
/// types has a packed wire encoder (unlike [`DriverManifest`]), so the C
/// mirror *is* the layout and there is no separate `*_WIRE_LEN` macro. The
/// `BusDevice::class` field is spelled `device_class` here because `class`
/// is reserved in C++ and the umbrella header must compile under a C++
/// `extern "C"` include; only the byte layout is part of the ABI, not the
/// field name.
///
/// The error enums (`WindowError`, `MmioMapError`), the opaque arch-built
/// `MsiMessage`, the in-process policy records (`NodeSecurity`,
/// `SecurityAcl`, `SecuritySubject`) and the runtime objects (`RegisterWindow`,
/// `DmaSlab`, `PoolId`) carry no `#[repr(C)]`/explicit-primitive layout and do
/// not cross the C boundary, so — like the driver traits — they are
/// deliberately omitted.
const DRIVER_SUBMODULE_TYPEDEFS: &str = concat!(
    "/* Block-device geometry (the drivers/storage class). */\n\
         typedef struct tairix_block_geometry {\n\
         \x20   uint32_t block_size;\n\
         \x20   uint64_t block_count;\n\
         } tairix_block_geometry_t;\n\n",
    "/* Discard (TRIM/unmap) capability a block device reports. */\n\
         typedef struct tairix_discard_capability {\n\
         \x20   uint8_t supported;\n\
         \x20   uint64_t granularity_blocks;\n\
         \x20   uint64_t max_blocks_per_request;\n\
         } tairix_discard_capability_t;\n\n",
    "/* Point-in-time device-health snapshot (SMART / NVMe telemetry). */\n\
         typedef struct tairix_health_snapshot {\n\
         \x20   uint64_t power_on_hours;\n\
         \x20   uint64_t unsafe_shutdowns;\n\
         \x20   uint64_t media_errors;\n\
         \x20   uint64_t reallocated_sectors;\n\
         \x20   uint64_t pending_sectors;\n\
         \x20   uint64_t uncorrectable_sectors;\n\
         \x20   uint64_t crc_errors;\n\
         \x20   uint16_t percentage_used;\n\
         \x20   uint16_t available_spare;\n\
         \x20   uint16_t temperature_kelvin;\n\
         \x20   uint8_t critical_warning;\n\
         } tairix_health_snapshot_t;\n\n",
    "/* Identifying tuple for a discovered device (the drivers/bus class).\n\
         * `device_class` mirrors the Rust `class` field (renamed for C++). */\n\
         typedef struct tairix_bus_device {\n\
         \x20   uint32_t vendor;\n\
         \x20   uint32_t device;\n\
         \x20   uint16_t device_class;\n\
         \x20   uint16_t reserved0;\n\
         \x20   uint64_t address;\n\
         } tairix_bus_device_t;\n\n",
    "/* Active display mode (the drivers/display class); `format` is a\n\
         * TAIRIX_DISPLAY_FORMAT_*. */\n\
         typedef struct tairix_display_mode {\n\
         \x20   uint32_t width_px;\n\
         \x20   uint32_t height_px;\n\
         \x20   uint32_t stride_bytes;\n\
         \x20   uint8_t format;\n\
         } tairix_display_mode_t;\n\n",
    "/* What a hardware compositor back-end can do this frame. */\n\
         typedef struct tairix_accel_caps {\n\
         \x20   uint32_t max_layers;\n\
         \x20   uint32_t max_width_px;\n\
         \x20   uint32_t max_height_px;\n\
         \x20   uint8_t per_layer_opacity;\n\
         } tairix_accel_caps_t;\n\n",
    "/* The four AGENTS.md sec.21 timestamps stored for a filesystem node. A\n\
         * stamp the backing format does not keep is the epoch (never a\n\
         * fabricated wall time). */\n\
         typedef struct tairix_node_times {\n\
         \x20   tairix_time64_t created;\n\
         \x20   tairix_time64_t modified;\n\
         \x20   tairix_time64_t accessed;\n\
         \x20   tairix_time64_t changed;\n\
         } tairix_node_times_t;\n\n",
    "/* Structural metadata about a filesystem node; `kind` is a TAIRIX_NODE_KIND_*.\n\
         * `times` carries the node's four timestamps, read in the same structural\n\
         * read as kind/size. */\n\
         typedef struct tairix_node_info {\n\
         \x20   uint8_t kind;\n\
         \x20   uint64_t size;\n\
         \x20   uint64_t allocated;\n\
         \x20   tairix_node_times_t times;\n\
         } tairix_node_info_t;\n\n",
    "/* One directory entry; `node` is a NodeId (uint64_t). The entry carries the\n\
         * child's full tairix_node_info_t (including its timestamps) and the opaque\n\
         * cursor that resumes the listing after it (pass it back to read_dir; 0\n\
         * starts a listing). */\n\
         typedef struct tairix_dir_entry {\n\
         \x20   uint64_t node;\n\
         \x20   tairix_node_info_t info;\n\
         \x20   uintptr_t name_len;\n\
         \x20   uint64_t next_cursor;\n\
         } tairix_dir_entry_t;\n\n",
    "/* A mounted volume's space accounting, in whole blocks of block_size bytes.\n\
         * avail_blocks <= free_blocks <= total_blocks always holds; files/files_free\n\
         * are 0 when the format tracks no fixed inode table. */\n\
         typedef struct tairix_volume_stats {\n\
         \x20   uint32_t block_size;\n\
         \x20   uint32_t reserved0;\n\
         \x20   uint64_t total_blocks;\n\
         \x20   uint64_t free_blocks;\n\
         \x20   uint64_t avail_blocks;\n\
         \x20   uint64_t files;\n\
         \x20   uint64_t files_free;\n\
         } tairix_volume_stats_t;\n\n",
    "/* A single input event; `kind` is a TAIRIX_INPUT_EVENT_KIND_*. */\n\
         typedef struct tairix_input_event {\n\
         \x20   uint8_t kind;\n\
         \x20   uint8_t reserved0;\n\
         \x20   uint16_t code;\n\
         \x20   int32_t value;\n\
         } tairix_input_event_t;\n\n",
    "/* A 48-bit IEEE 802 link-layer address (the drivers/network class). */\n\
         typedef struct tairix_mac_address {\n\
         \x20   uint8_t octets[TAIRIX_MAC_ADDRESS_LEN];\n\
         } tairix_mac_address_t;\n\n",
);

/// `tairix_syscall.h` — the syscall numbers and C entry-point prototypes.
fn generate_syscall() -> String {
    use std::fmt::Write as _;
    let mut out = banner("Syscall numbers and C entry-point prototypes (AGENTS.md sec.9).");
    out.push_str("#ifndef TAIRIX_SYSCALL_H\n#define TAIRIX_SYSCALL_H\n\n");
    out.push_str("#include <stdint.h>\n\n");
    out.push_str("#ifdef __cplusplus\nextern \"C\" {\n#endif\n\n");

    out.push_str("/* Syscall numbers (AGENTS.md sec.9). */\n");
    let _ = writeln!(out, "#define TAIRIX_SYSCALL_MAX_ARGS {SYSCALL_MAX_ARGS}u");
    for spec in SYSCALLS {
        let _ = writeln!(
            out,
            "#define TAIRIX_SYS_{} {}u",
            spec.name.to_ascii_uppercase(),
            spec.number.as_u16()
        );
    }
    out.push('\n');

    emit_wait_contract(&mut out);
    emit_spawn_attach_contract(&mut out);
    emit_fs_contract(&mut out);
    emit_signal_contract(&mut out);
    emit_power_contract(&mut out);
    emit_waitset_contract(&mut out);

    out.push_str("/* Syscall entry points, implemented by the user-space stub library. */\n");
    for spec in SYSCALLS {
        let _ = writeln!(out, "{}", prototype(spec));
    }
    out.push('\n');

    out.push_str("#ifdef __cplusplus\n} /* extern \"C\" */\n#endif\n\n");
    out.push_str("#endif /* TAIRIX_SYSCALL_H */\n");
    out
}

/// Emit the signal-call contract items into `tairix_syscall.h`: the
/// `signal()` control-signal discriminants and the `signal_intake()`
/// operations, every value read from `lib/abi` and never re-typed.
fn emit_signal_contract(out: &mut String) {
    use std::fmt::Write as _;
    out.push_str(
        "/* signal() control signals (the `signal` argument, uint32_t). 0 is reserved and\n\
         * never valid; a value outside this set is rejected with TAIRIX_E_OUT_OF_RANGE. */\n",
    );
    let _ = writeln!(
        out,
        "#define TAIRIX_SIGNAL_CONTINUE {}u",
        Signal::Continue.as_u32()
    );
    let _ = writeln!(
        out,
        "#define TAIRIX_SIGNAL_TERMINATE {}u",
        Signal::Terminate.as_u32()
    );
    let _ = writeln!(out, "#define TAIRIX_SIGNAL_KILL {}u", Signal::Kill.as_u32());
    let _ = writeln!(
        out,
        "#define TAIRIX_SIGNAL_INTERRUPT {}u",
        Signal::Interrupt.as_u32()
    );
    let _ = writeln!(out, "#define TAIRIX_SIGNAL_STOP {}u", Signal::Stop.as_u32());
    out.push('\n');

    out.push_str(
        "/* signal_intake() operations (the `op` argument, uint32_t). A value outside\n\
         * this set is rejected with TAIRIX_E_OUT_OF_RANGE. With the intake enabled, a\n\
         * pending observed signal is waited on through a wait-set member of kind\n\
         * TAIRIX_WAIT_SOURCE_SIGNAL (id 0) and drained with the take operation, which\n\
         * returns the drained TAIRIX_SIGNAL_* discriminant. TAIRIX_SIGNAL_KILL is never\n\
         * observable; a second termination request while one is pending undrained\n\
         * escalates to the default terminate path. */\n",
    );
    let _ = writeln!(
        out,
        "#define TAIRIX_SIGNAL_INTAKE_OP_ENABLE {}u",
        SignalIntakeOp::Enable.as_u32()
    );
    let _ = writeln!(
        out,
        "#define TAIRIX_SIGNAL_INTAKE_OP_DISABLE {}u",
        SignalIntakeOp::Disable.as_u32()
    );
    let _ = writeln!(
        out,
        "#define TAIRIX_SIGNAL_INTAKE_OP_TAKE {}u",
        SignalIntakeOp::Take.as_u32()
    );
    out.push('\n');

    out.push_str(
        "/* sched_set_priority() service levels (the `priority` argument, uint32_t),\n\
         * also carried in tairix_process_record.priority. 0 is reserved and never\n\
         * valid; a value outside this set is rejected with TAIRIX_E_OUT_OF_RANGE.\n\
         * The target rule mirrors signal(): an own child, else a process of the\n\
         * caller's own principal, else TAIRIX_CAP_PROC_CONTROL. Raising the level\n\
         * (toward HIGH) always requires TAIRIX_CAP_PROC_CONTROL. */\n",
    );
    let _ = writeln!(
        out,
        "#define TAIRIX_SCHED_PRIORITY_HIGH {}u",
        SchedPriority::High.as_u32()
    );
    let _ = writeln!(
        out,
        "#define TAIRIX_SCHED_PRIORITY_NORMAL {}u",
        SchedPriority::Normal.as_u32()
    );
    let _ = writeln!(
        out,
        "#define TAIRIX_SCHED_PRIORITY_LOW {}u",
        SchedPriority::Low.as_u32()
    );
    out.push('\n');
}

/// Emit the power-call contract items into `tairix_syscall.h`: the
/// `system_power()` transitions, read from `lib/abi` and never re-typed.
fn emit_power_contract(out: &mut String) {
    use std::fmt::Write as _;
    out.push_str(
        "/* system_power() transitions (the `action` argument, uint32_t). 0 is reserved\n\
         * and never valid; a value outside this set is rejected with\n\
         * TAIRIX_E_OUT_OF_RANGE. The call requires TAIRIX_CAP_SYSTEM_POWER, flushes\n\
         * every mounted volume first (a volume that will not flush abandons the\n\
         * transition and returns its error), and returns only when the transition\n\
         * was refused: TAIRIX_E_NOT_SUPPORTED on a port with no such primitive. */\n",
    );
    let _ = writeln!(
        out,
        "#define TAIRIX_POWER_ACTION_POWER_OFF {}u",
        PowerAction::PowerOff.as_u32()
    );
    let _ = writeln!(
        out,
        "#define TAIRIX_POWER_ACTION_RESTART {}u",
        PowerAction::Restart.as_u32()
    );
    out.push('\n');
}

/// Emit the wait-set contract items into `tairix_syscall.h`: the
/// `waitset_ctl()` operations and member source kinds, every value read
/// from `lib/abi` and never re-typed.
fn emit_waitset_contract(out: &mut String) {
    use std::fmt::Write as _;
    out.push_str(
        "/* waitset_ctl() operations (the `op` argument, uint32_t) and member source\n\
         * kinds (the `kind` argument, uint32_t). A value outside either set is\n\
         * rejected with TAIRIX_E_OUT_OF_RANGE; every member is owner-checked against the\n\
         * calling task when it is added. */\n",
    );
    let _ = writeln!(
        out,
        "#define TAIRIX_WAITSET_OP_ADD {}u",
        WaitSetOp::Add.as_u32()
    );
    let _ = writeln!(
        out,
        "#define TAIRIX_WAITSET_OP_DEL {}u",
        WaitSetOp::Del.as_u32()
    );
    // Every wait source in the closed ABI set, discovered by walking the
    // wire values `WaitSourceKind::from_u32` accepts rather than a list
    // kept in step by hand: a hand-written list silently omitted two
    // kinds once already, and a header that does not name a source a
    // program can legally use is a broken contract.
    for value in 0.. {
        let Ok(kind) = WaitSourceKind::from_u32(value) else {
            break;
        };
        let _ = writeln!(
            out,
            "#define TAIRIX_WAIT_SOURCE_{} {}u",
            wait_source_macro_suffix(kind),
            kind.as_u32()
        );
    }
    out.push('\n');
}

/// The C macro suffix naming `kind`, as `TAIRIX_WAIT_SOURCE_<suffix>`.
///
/// Exhaustive on purpose: adding a wait source to the ABI must fail to
/// compile here until it is named, so the generated header can never
/// again fall behind the enum it is generated from.
const fn wait_source_macro_suffix(kind: WaitSourceKind) -> &'static str {
    match kind {
        WaitSourceKind::Endpoint => "ENDPOINT",
        WaitSourceKind::Irq => "IRQ",
        WaitSourceKind::Child => "CHILD",
        WaitSourceKind::SeatInput => "SEAT_INPUT",
        WaitSourceKind::Port => "PORT",
        WaitSourceKind::Stream => "STREAM",
        WaitSourceKind::Signal => "SIGNAL",
        WaitSourceKind::File => "FILE",
        WaitSourceKind::CallReply => "CALL_REPLY",
        WaitSourceKind::MemoryPressure => "MEMORY_PRESSURE",
        WaitSourceKind::PortRoom => "PORT_ROOM",
    }
}

/// Emit the filesystem-call contract items into `tairix_syscall.h`: the
/// `fs_open()` and `fs_unlink()` flag bits and the `fs_set_mode()`
/// permission mask, every value read from `lib/abi` and never re-typed.
fn emit_fs_contract(out: &mut String) {
    use std::fmt::Write as _;
    out.push_str(
        "/* fs_open() flag bits (uint32_t). Every undefined bit is reserved and rejected\n\
         * with TAIRIX_E_OUT_OF_RANGE, as is a combination the contract forbids (TRUNCATE/\n\
         * APPEND without WRITE, EXCLUSIVE without CREATE, DIRECTORY with WRITE). An open\n\
         * with neither READ nor WRITE is a resolve-only handle. */\n",
    );
    let _ = writeln!(
        out,
        "#define TAIRIX_OPEN_FLAG_READ {:#x}u",
        OpenFlags::READ.bits()
    );
    let _ = writeln!(
        out,
        "#define TAIRIX_OPEN_FLAG_WRITE {:#x}u",
        OpenFlags::WRITE.bits()
    );
    let _ = writeln!(
        out,
        "#define TAIRIX_OPEN_FLAG_CREATE {:#x}u",
        OpenFlags::CREATE.bits()
    );
    let _ = writeln!(
        out,
        "#define TAIRIX_OPEN_FLAG_TRUNCATE {:#x}u",
        OpenFlags::TRUNCATE.bits()
    );
    let _ = writeln!(
        out,
        "#define TAIRIX_OPEN_FLAG_APPEND {:#x}u",
        OpenFlags::APPEND.bits()
    );
    let _ = writeln!(
        out,
        "#define TAIRIX_OPEN_FLAG_DIRECTORY {:#x}u",
        OpenFlags::DIRECTORY.bits()
    );
    let _ = writeln!(
        out,
        "#define TAIRIX_OPEN_FLAG_EXCLUSIVE {:#x}u",
        OpenFlags::EXCLUSIVE.bits()
    );
    let _ = writeln!(
        out,
        "#define TAIRIX_OPEN_FLAG_NO_FOLLOW {:#x}u",
        OpenFlags::NO_FOLLOW.bits()
    );
    out.push('\n');

    out.push_str(
        "/* fs_unlink() flag bits (uint32_t). Every undefined bit is reserved and rejected\n\
         * with TAIRIX_E_OUT_OF_RANGE. 0 removes the named file or (empty) directory; with\n\
         * the DIRECTORY bit the removal succeeds only when the name is an (empty)\n\
         * directory (the atomic rmdir posture) and a non-directory is refused with\n\
         * TAIRIX_E_NOT_A_DIRECTORY. */\n",
    );
    let _ = writeln!(
        out,
        "#define TAIRIX_UNLINK_FLAG_DIRECTORY {:#x}u",
        UnlinkFlags::DIRECTORY.bits()
    );
    out.push('\n');

    out.push_str(
        "/* fs_link() flag bits (uint32_t). Every undefined bit is reserved and rejected\n\
         * with TAIRIX_E_OUT_OF_RANGE. 0 is POSIX link(): neither operand's final\n\
         * component is followed, so the node that gains a name is the one spelled. With\n\
         * the FOLLOW bit the existing name's final symbolic link is resolved and the new\n\
         * name is given to what it names (the linkat(AT_SYMLINK_FOLLOW) posture). The new\n\
         * name is never followed under either. */\n",
    );
    let _ = writeln!(
        out,
        "#define TAIRIX_LINK_FLAG_FOLLOW {:#x}u",
        LinkFlags::FOLLOW.bits()
    );
    out.push('\n');

    out.push_str(
        "/* fs_realpath() mode (uint32_t). The three readings are alternatives, so this is\n\
         * one value rather than bits, and any other value is rejected with\n\
         * TAIRIX_E_OUT_OF_RANGE. EXISTING requires every component to exist, FINAL lets\n\
         * the last one be absent, and MISSING lets any of them be. All three resolve\n\
         * identically otherwise. */\n",
    );
    for (name, mode) in [
        ("EXISTING", RealpathMode::Existing),
        ("FINAL", RealpathMode::Final),
        ("MISSING", RealpathMode::Missing),
    ] {
        let _ = writeln!(
            out,
            "#define TAIRIX_REALPATH_MODE_{name} {}u",
            mode.as_u32()
        );
    }
    out.push('\n');

    out.push_str(
        "/* fs_set_mode() permission-bit mask (the `mode` argument, uint32_t): the\n\
         * owner/group/other rwx triads plus the setuid/setgid/sticky bits. A mode\n\
         * carrying any higher bit (a file-type bit, say) is rejected with\n\
         * TAIRIX_E_OUT_OF_RANGE, never silently masked. */\n",
    );
    let _ = writeln!(out, "#define TAIRIX_FS_MODE_MASK {FS_MODE_MASK:#x}u");
    out.push('\n');

    out.push_str(
        "/* fs_attr_*() bounds: an extended-attribute key (a `namespace.rest`\n\
         * lib/fsmeta-grammar key) carries 1..=TAIRIX_FS_ATTR_KEY_MAX bytes, and a value\n\
         * at most TAIRIX_FS_ATTR_VALUE_MAX opaque bytes; a call outside either bound is\n\
         * rejected with TAIRIX_E_LENGTH_OUT_OF_RANGE before any copy. An absent\n\
         * attribute reads as TAIRIX_E_NO_DATA (a value may be empty, so absence is\n\
         * never an empty read), and a mount whose on-disk format stores no\n\
         * attributes answers every fs_attr_*() call with TAIRIX_E_NOT_SUPPORTED. */\n",
    );
    let _ = writeln!(out, "#define TAIRIX_FS_ATTR_KEY_MAX {FS_ATTR_KEY_MAX}u");
    let _ = writeln!(out, "#define TAIRIX_FS_ATTR_VALUE_MAX {FS_ATTR_VALUE_MAX}u");
    out.push('\n');
}

/// Emit the `wait()` contract items into `tairix_syscall.h`: the flag bits
/// and the typed `tairix_wait_status_t` record the syscall writes through its
/// status pointer, every value read from `lib/abi` and never re-typed.
fn emit_wait_contract(out: &mut String) {
    use std::fmt::Write as _;
    out.push_str(
        "/* wait() flag bits (uint32_t). Every undefined bit is reserved and must be zero;\n\
         * with the NONBLOCK bit set, wait() polls and returns TAIRIX_E_WOULD_BLOCK when a\n\
         * matching child has nothing to report; with the STOPPED bit set, wait() also\n\
         * reports a child freshly stopped by TAIRIX_SIGNAL_STOP, without reaping it. */\n",
    );
    let _ = writeln!(
        out,
        "#define TAIRIX_WAIT_FLAG_NONBLOCK {:#x}u",
        WaitFlags::NONBLOCK.bits()
    );
    let _ = writeln!(
        out,
        "#define TAIRIX_WAIT_FLAG_STOPPED {:#x}u",
        WaitFlags::STOPPED.bits()
    );
    out.push('\n');

    out.push_str(
        "/* The typed record wait() writes through its status pointer: kind names the\n\
         * event (exited => value is the exit code; stopped => value is the stopping\n\
         * TAIRIX_SIGNAL_* discriminant); 0 and every other kind are reserved. */\n",
    );
    let _ = writeln!(
        out,
        "#define TAIRIX_WAIT_STATUS_KIND_EXITED {}u",
        tairix_abi::WAIT_STATUS_KIND_EXITED
    );
    let _ = writeln!(
        out,
        "#define TAIRIX_WAIT_STATUS_KIND_STOPPED {}u",
        tairix_abi::WAIT_STATUS_KIND_STOPPED
    );
    out.push_str(
        "typedef struct tairix_wait_status {\n\
         \x20   uint32_t kind;\n\
         \x20   int32_t value;\n\
         } tairix_wait_status_t;\n",
    );
    out.push('\n');

    out.push_str(
        "/* Reserved load-failure exit statuses (a tairix_wait_status_t.value when kind is\n\
         * EXITED). A spawn() returns once the child is ADMITTED, not once it is LOADED, so\n\
         * a load failure the child discovers on its own task surfaces as one of these\n\
         * exit statuses rather than as a spawn() error. They sit in a high reserved band\n\
         * well above the small codes a program passes to exit(), so a parent can tell a\n\
         * loader refusal apart from an ordinary exit: NOT_FOUND (missing or unreadable\n\
         * bundle), UNVERIFIED (bad signature / content or interface hash), MALFORMED\n\
         * (un-parseable or unfit image), OOM (out of memory building the image). */\n",
    );
    let _ = writeln!(
        out,
        "#define TAIRIX_LOAD_NOT_FOUND ((int32_t){})",
        tairix_abi::LOAD_NOT_FOUND
    );
    let _ = writeln!(
        out,
        "#define TAIRIX_LOAD_UNVERIFIED ((int32_t){})",
        tairix_abi::LOAD_UNVERIFIED
    );
    let _ = writeln!(
        out,
        "#define TAIRIX_LOAD_MALFORMED ((int32_t){})",
        tairix_abi::LOAD_MALFORMED
    );
    let _ = writeln!(
        out,
        "#define TAIRIX_LOAD_OOM ((int32_t){})",
        tairix_abi::LOAD_OOM
    );
    out.push('\n');
}

/// Emit the `spawn()` attach-block contract items into `tairix_syscall.h`:
/// the version/length constants, the per-descriptor wire kinds, and the
/// typed `tairix_spawn_attach_t` block the syscall's `attach` pointer names
/// (`plans/SPAWN.md` SP10), every value read from `lib/abi` and never
/// re-typed. Every Tier-1 target is little-endian, so the packed C struct
/// written in native order is exactly the encoded block the kernel parses.
fn emit_spawn_attach_contract(out: &mut String) {
    use std::fmt::Write as _;
    out.push_str(
        "/* spawn() attach block: the child's credential, base console, and one wire per\n\
         * standard descriptor (fd 0..3). Pass NULL/0 for full inherit. Every wire kind\n\
         * other than the values below (including 0) is reserved and refused; a HANDLE\n\
         * wire names a descriptor of the CALLER'S OWN open table (a file, resource, or\n\
         * pipe end), owner-checked kernel-side before any child state exists. */\n",
    );
    let _ = writeln!(
        out,
        "#define TAIRIX_SPAWN_ATTACH_VERSION {}u",
        tairix_abi::SPAWN_ATTACH_VERSION
    );
    let _ = writeln!(
        out,
        "#define TAIRIX_SPAWN_ATTACH_LEN {}u",
        tairix_abi::SPAWN_ATTACH_LEN
    );
    out.push_str(
        "/* Attach-block flags. SANDBOX starts the child as a minimum-capability\n\
         * parser sandbox: empty capability set, closed syscall allow-list, and\n\
         * every wire must be CLOSED or HANDLE (nothing ambient flows in). Any\n\
         * reserved flag bit is refused. */\n",
    );
    let _ = writeln!(
        out,
        "#define TAIRIX_SPAWN_FLAG_SANDBOX {}u",
        tairix_abi::SPAWN_FLAG_SANDBOX
    );
    let _ = writeln!(
        out,
        "#define TAIRIX_FD_WIRE_INHERIT {}u",
        tairix_abi::FD_WIRE_KIND_INHERIT
    );
    let _ = writeln!(
        out,
        "#define TAIRIX_FD_WIRE_INHERIT_SLOT {}u",
        tairix_abi::FD_WIRE_KIND_INHERIT_SLOT
    );
    let _ = writeln!(
        out,
        "#define TAIRIX_FD_WIRE_CLOSED {}u",
        tairix_abi::FD_WIRE_KIND_CLOSED
    );
    let _ = writeln!(
        out,
        "#define TAIRIX_FD_WIRE_HANDLE {}u",
        tairix_abi::FD_WIRE_KIND_HANDLE
    );
    out.push_str(
        "typedef struct tairix_fd_wire {\n\
         \x20   uint32_t kind;\n\
         \x20   uint32_t value;\n\
         } tairix_fd_wire_t;\n\
         typedef struct tairix_spawn_attach {\n\
         \x20   uint32_t version;\n\
         \x20   uint32_t target_uid;\n\
         \x20   uint64_t console;\n\
         \x20   uint64_t flags;\n\
         \x20   tairix_fd_wire_t wires[4];\n\
         } tairix_spawn_attach_t;\n",
    );
    out.push('\n');
}

/// `tairix_abi.h` — the umbrella header that includes every module header.
fn generate_umbrella() -> String {
    use std::fmt::Write as _;
    let mut out = banner(
        "Umbrella header: the whole abi-v1 C surface in one include.\n\
         * Each syscall is exported by the user-space stub library under the\n\
         * symbol `tairix_sys_<name>` (e.g. `tairix_sys_ipc_send`); link against\n\
         * that library to call the kernel from a non-Rust program.",
    );
    out.push_str("#ifndef TAIRIX_ABI_H\n#define TAIRIX_ABI_H\n\n");
    out.push_str("/* ABI version this header set describes (AGENTS.md sec.9). */\n");
    let _ = writeln!(out, "#define TAIRIX_ABI_VERSION {ABI_VERSION_V1}u\n");
    out.push_str("#include \"tairix_error.h\"\n");
    out.push_str("#include \"tairix_capability.h\"\n");
    out.push_str("#include \"tairix_time.h\"\n");
    out.push_str("#include \"tairix_random.h\"\n");
    out.push_str("#include \"tairix_log.h\"\n");
    out.push_str("#include \"tairix_rlimit.h\"\n");
    out.push_str("#include \"tairix_memory.h\"\n");
    out.push_str("#include \"tairix_hwtree.h\"\n");
    out.push_str("#include \"tairix_ipc.h\"\n");
    out.push_str("#include \"tairix_stdinfo.h\"\n");
    out.push_str("#include \"tairix_manifest.h\"\n");
    out.push_str("#include \"tairix_input.h\"\n");
    out.push_str("#include \"tairix_appinfo.h\"\n");
    out.push_str("#include \"tairix_rxe.h\"\n");
    out.push_str("#include \"tairix_process.h\"\n");
    out.push_str("#include \"tairix_sysinfo.h\"\n");
    out.push_str("#include \"tairix_driver.h\"\n");
    out.push_str("#include \"tairix_syscall.h\"\n\n");
    out.push_str("#endif /* TAIRIX_ABI_H */\n");
    out
}

/// Generate the full C ABI header set from the `lib/abi` source of truth.
///
/// The output is deterministic: the same workspace always produces the same
/// bytes for every file, which is what lets [`check_sync`] use a
/// byte-for-byte comparison as a drift guard.
#[must_use]
pub fn generate_all() -> Vec<GeneratedHeader> {
    vec![
        GeneratedHeader {
            file_name: "tairix_error.h",
            body: generate_error(),
        },
        GeneratedHeader {
            file_name: "tairix_capability.h",
            body: generate_capability(),
        },
        GeneratedHeader {
            file_name: "tairix_time.h",
            body: generate_time(),
        },
        GeneratedHeader {
            file_name: "tairix_random.h",
            body: generate_random(),
        },
        GeneratedHeader {
            file_name: "tairix_log.h",
            body: generate_log(),
        },
        GeneratedHeader {
            file_name: "tairix_rlimit.h",
            body: generate_rlimit(),
        },
        GeneratedHeader {
            file_name: "tairix_memory.h",
            body: generate_memory(),
        },
        GeneratedHeader {
            file_name: "tairix_hwtree.h",
            body: generate_hwtree(),
        },
        GeneratedHeader {
            file_name: "tairix_ipc.h",
            body: generate_ipc(),
        },
        GeneratedHeader {
            file_name: "tairix_stdinfo.h",
            body: generate_stdinfo(),
        },
        GeneratedHeader {
            file_name: "tairix_manifest.h",
            body: generate_manifest(),
        },
        GeneratedHeader {
            file_name: "tairix_input.h",
            body: generate_input(),
        },
        GeneratedHeader {
            file_name: "tairix_appinfo.h",
            body: generate_appinfo(),
        },
        GeneratedHeader {
            file_name: "tairix_rxe.h",
            body: generate_rxe(),
        },
        GeneratedHeader {
            file_name: "tairix_process.h",
            body: generate_process(),
        },
        GeneratedHeader {
            file_name: "tairix_sysinfo.h",
            body: generate_sysinfo(),
        },
        GeneratedHeader {
            file_name: "tairix_driver.h",
            body: generate_driver(),
        },
        GeneratedHeader {
            file_name: "tairix_syscall.h",
            body: generate_syscall(),
        },
        GeneratedHeader {
            file_name: "tairix_abi.h",
            body: generate_umbrella(),
        },
    ]
}

/// Verify that every committed header matches freshly generated output.
///
/// `include_dir` points at the committed header directory (callers default it
/// to [`DEFAULT_INCLUDE_DIR`]). A missing or stale header is a hard error
/// directing the developer to `cargo xtask c-header --write`.
pub fn check_sync(workspace_root: &Path, include_dir: &Path) -> Result<(), String> {
    for header in generate_all() {
        let path = include_dir.join(header.file_name);
        let rel = relative(workspace_root, &path);
        let on_disk = match std::fs::read_to_string(&path) {
            Ok(text) => text,
            Err(err) if err.kind() == std::io::ErrorKind::NotFound => {
                return Err(format!(
                    "c-header: `{rel}` is missing; run `cargo xtask c-header --write` \
                     to generate it from lib/abi (AGENTS.md sec.9)."
                ));
            }
            Err(err) => return Err(format!("c-header: cannot read {rel}: {err}")),
        };
        if on_disk != header.body {
            return Err(format!(
                "c-header: `{rel}` is out of date with the lib/abi source of truth; \
                 run `cargo xtask c-header --write` and commit the result \
                 (AGENTS.md sec.2.2, sec.9)."
            ));
        }
    }
    Ok(())
}

/// Regenerate every committed header in `include_dir`, creating it if needed.
pub fn write(workspace_root: &Path, include_dir: &Path) -> Result<(), String> {
    std::fs::create_dir_all(include_dir).map_err(|e| {
        format!(
            "c-header: cannot create {}: {e}",
            relative(workspace_root, include_dir)
        )
    })?;
    for header in generate_all() {
        let path = include_dir.join(header.file_name);
        std::fs::write(&path, header.body).map_err(|e| {
            format!(
                "c-header: cannot write {}: {e}",
                relative(workspace_root, &path)
            )
        })?;
    }
    Ok(())
}

fn relative(base: &Path, path: &Path) -> String {
    path.strip_prefix(base)
        .unwrap_or(path)
        .display()
        .to_string()
}

#[cfg(test)]
mod tests {
    use super::*;
    use tairix_abi::SYSCALL_NAME_MAX;

    fn workspace_root() -> std::path::PathBuf {
        // CARGO_MANIFEST_DIR points at tools/xtask; the workspace root is
        // its great-grandparent (matches abi_check.rs).
        let mut p = std::path::PathBuf::from(env!("CARGO_MANIFEST_DIR"));
        p.pop(); // tools
        p.pop(); // workspace
        p
    }

    fn body(file_name: &str) -> String {
        generate_all()
            .into_iter()
            .find(|h| h.file_name == file_name)
            .map_or_else(|| panic!("missing header {file_name}"), |h| h.body)
    }

    #[test]
    fn generation_is_deterministic() {
        for (a, b) in generate_all().iter().zip(generate_all().iter()) {
            assert_eq!(a.file_name, b.file_name);
            assert_eq!(a.body, b.body);
        }
    }

    #[test]
    fn umbrella_includes_every_module_header() {
        let h = body("tairix_abi.h");
        assert!(h.contains("#ifndef TAIRIX_ABI_H"), "guard present");
        assert!(h.contains("#define TAIRIX_ABI_VERSION 1u"), "version macro");
        for module in [
            "tairix_error.h",
            "tairix_capability.h",
            "tairix_time.h",
            "tairix_random.h",
            "tairix_log.h",
            "tairix_rlimit.h",
            "tairix_memory.h",
            "tairix_hwtree.h",
            "tairix_ipc.h",
            "tairix_stdinfo.h",
            "tairix_manifest.h",
            "tairix_input.h",
            "tairix_appinfo.h",
            "tairix_rxe.h",
            "tairix_process.h",
            "tairix_sysinfo.h",
            "tairix_driver.h",
            "tairix_syscall.h",
        ] {
            assert!(
                h.contains(&format!("#include \"{module}\"")),
                "umbrella must include {module}: {h}"
            );
        }
    }

    #[test]
    fn error_header_has_codes() {
        let h = body("tairix_error.h");
        assert!(h.contains("#ifndef TAIRIX_ERROR_H"), "guard present");
        assert!(h.contains("#define TAIRIX_E_PERMISSION_DENIED 6"), "errno");
    }

    #[test]
    fn capability_header_has_ids() {
        let h = body("tairix_capability.h");
        assert!(h.contains("#ifndef TAIRIX_CAPABILITY_H"), "guard present");
        assert!(h.contains("#include <stdint.h>"), "stdint included");
        assert!(
            h.contains("#define TAIRIX_CAP_USER_ADMIN ((uint16_t)5u)"),
            "capability id carries its canonical uint16_t type: {h}"
        );
    }

    /// The `fs_attr_*()` bounds are read from `lib/abi`, never hardcoded.
    #[test]
    fn syscall_header_carries_the_fs_attr_bounds() {
        let h = generate_syscall();
        assert!(
            h.contains(&format!(
                "#define TAIRIX_FS_ATTR_KEY_MAX {FS_ATTR_KEY_MAX}u"
            )),
            "fs_attr key bound: {h}"
        );
        assert!(
            h.contains(&format!(
                "#define TAIRIX_FS_ATTR_VALUE_MAX {FS_ATTR_VALUE_MAX}u"
            )),
            "fs_attr value bound: {h}"
        );
    }

    /// Every filesystem flag word the header publishes is read from
    /// `lib/abi` rather than re-typed, and each call's prototype carries its
    /// `uint32_t` flags slot — so a widened flag set cannot reach C as a
    /// stale constant or a dropped argument.
    #[test]
    fn syscall_header_pins_every_filesystem_flag_word() {
        let h = body("tairix_syscall.h");
        assert!(
            h.contains(&format!(
                "#define TAIRIX_OPEN_FLAG_EXCLUSIVE {:#x}u",
                OpenFlags::EXCLUSIVE.bits()
            )),
            "fs_open flag bits: {h}"
        );
        assert!(
            h.contains(&format!(
                "#define TAIRIX_UNLINK_FLAG_DIRECTORY {:#x}u",
                UnlinkFlags::DIRECTORY.bits()
            )),
            "fs_unlink directory flag bit: {h}"
        );
        assert!(
            h.contains("int32_t tairix_sys_fs_unlink(void * a0, uintptr_t a1, uint32_t a2);"),
            "fs_unlink prototype carries the flags argument: {h}"
        );
        assert!(
            h.contains(&format!(
                "#define TAIRIX_LINK_FLAG_FOLLOW {:#x}u",
                LinkFlags::FOLLOW.bits()
            )),
            "fs_link follow flag bit: {h}"
        );
        assert!(
            h.contains(
                "int32_t tairix_sys_fs_link(void * a0, uintptr_t a1, void * a2, uintptr_t a3, uint32_t a4);"
            ),
            "fs_link prototype carries the flags argument: {h}"
        );
        for (name, mode) in [
            ("EXISTING", RealpathMode::Existing),
            ("FINAL", RealpathMode::Final),
            ("MISSING", RealpathMode::Missing),
        ] {
            assert!(
                h.contains(&format!(
                    "#define TAIRIX_REALPATH_MODE_{name} {}u",
                    mode.as_u32()
                )),
                "fs_realpath {name} mode: {h}"
            );
        }
        assert!(
            h.contains(
                "uint64_t tairix_sys_fs_realpath(void * a0, uintptr_t a1, void * a2, uintptr_t a3, uint32_t a4);"
            ),
            "fs_realpath prototype carries the mode argument: {h}"
        );
    }

    #[test]
    fn syscall_header_has_numbers_and_prototypes() {
        let h = body("tairix_syscall.h");
        assert!(h.contains("#ifndef TAIRIX_SYSCALL_H"), "guard present");
        assert!(h.contains("#include <stdint.h>"), "stdint included");
        assert!(h.contains("extern \"C\""), "C++ guard present");
        assert!(h.contains("#define TAIRIX_SYS_EXIT 1u"), "syscall number");
        assert!(
            h.contains("void tairix_sys_yield(void);"),
            "nullary prototype: {h}"
        );
        assert!(
            h.contains("int32_t tairix_sys_ipc_send(uint64_t a0, void * a1, uintptr_t a2);"),
            "typed prototype: {h}"
        );
        // The wait() poll flag bit is read from lib/abi, never re-typed, and
        // the prototype carries its uint32_t flags argument.
        assert!(
            h.contains(&format!(
                "#define TAIRIX_WAIT_FLAG_NONBLOCK {:#x}u",
                WaitFlags::NONBLOCK.bits()
            )),
            "wait nonblock flag bit: {h}"
        );
        assert!(
            h.contains("uint64_t tairix_sys_wait(int32_t a0, void * a1, uint32_t a2);"),
            "wait prototype carries the flags argument: {h}"
        );
        // The fs_set_mode() permission mask is read from lib/abi, never
        // re-typed, and the prototype carries its uint32_t mode argument.
        assert!(
            h.contains(&format!("#define TAIRIX_FS_MODE_MASK {FS_MODE_MASK:#x}u")),
            "fs_set_mode permission mask: {h}"
        );
        assert!(
            h.contains("int32_t tairix_sys_fs_set_mode(void * a0, uintptr_t a1, uint32_t a2);"),
            "fs_set_mode prototype carries the mode argument: {h}"
        );
        // The signal() control-signal discriminants are read from lib/abi,
        // never re-typed, and the prototype carries its (pid, signal) args.
        assert!(
            h.contains(&format!(
                "#define TAIRIX_SIGNAL_TERMINATE {}u",
                Signal::Terminate.as_u32()
            )),
            "signal terminate discriminant: {h}"
        );
        assert!(
            h.contains("int32_t tairix_sys_signal(int32_t a0, uint32_t a1);"),
            "signal prototype carries the pid and signal arguments: {h}"
        );
        // The stop-report flag, the two line-discipline signals, and the
        // typed wait-status record are read from lib/abi, never re-typed.
        assert!(
            h.contains(&format!(
                "#define TAIRIX_WAIT_FLAG_STOPPED {:#x}u",
                WaitFlags::STOPPED.bits()
            )),
            "wait stopped flag bit: {h}"
        );
        assert!(
            h.contains(&format!(
                "#define TAIRIX_SIGNAL_STOP {}u",
                Signal::Stop.as_u32()
            )),
            "signal stop discriminant: {h}"
        );
        assert!(
            h.contains(&format!(
                "#define TAIRIX_SIGNAL_INTERRUPT {}u",
                Signal::Interrupt.as_u32()
            )),
            "signal interrupt discriminant: {h}"
        );
        assert!(
            h.contains("} tairix_wait_status_t;"),
            "wait status record struct: {h}"
        );
        assert!(
            h.contains(&format!(
                "#define TAIRIX_WAIT_STATUS_KIND_STOPPED {}u",
                tairix_abi::WAIT_STATUS_KIND_STOPPED
            )),
            "wait status stopped kind: {h}"
        );
        assert!(
            h.contains("int32_t tairix_sys_console_foreground(uint32_t a0, int32_t a1);"),
            "console_foreground prototype carries the fd and pid arguments: {h}"
        );
    }

    /// The `sched_set_priority()` contract: the prototype carries its
    /// `(pid, level)` arguments and the service-level discriminants are
    /// read from `lib/abi`, never re-typed.
    #[test]
    fn syscall_header_carries_the_sched_priority_contract() {
        let h = body("tairix_syscall.h");
        assert!(
            h.contains("int32_t tairix_sys_sched_set_priority(int32_t a0, uint32_t a1);"),
            "sched_set_priority prototype carries the pid and level arguments: {h}"
        );
        for (name, level) in [
            ("TAIRIX_SCHED_PRIORITY_HIGH", SchedPriority::High),
            ("TAIRIX_SCHED_PRIORITY_NORMAL", SchedPriority::Normal),
            ("TAIRIX_SCHED_PRIORITY_LOW", SchedPriority::Low),
        ] {
            let line = format!("#define {name} {}u", level.as_u32());
            assert!(h.contains(&line), "level constant pinned: {line}");
        }
        // The record mirror carries the same level, so a C consumer can
        // interpret tairix_process_record.priority with one vocabulary.
        let s = body("tairix_sysinfo.h");
        assert!(
            s.contains("uint32_t priority;"),
            "process record mirror carries the service level: {s}"
        );
    }

    /// The `system_power()` contract: the prototype carries its `action`
    /// argument and the transition discriminants are read from `lib/abi`,
    /// never re-typed.
    #[test]
    fn syscall_header_carries_the_system_power_contract() {
        let h = body("tairix_syscall.h");
        assert!(
            h.contains("int32_t tairix_sys_system_power(uint32_t a0);"),
            "system_power prototype carries the action argument: {h}"
        );
        for (name, action) in [
            ("TAIRIX_POWER_ACTION_POWER_OFF", PowerAction::PowerOff),
            ("TAIRIX_POWER_ACTION_RESTART", PowerAction::Restart),
        ] {
            let line = format!("#define {name} {}u", action.as_u32());
            assert!(h.contains(&line), "action constant pinned: {line}");
        }
    }

    /// The reserved load-failure exit statuses are read from `lib/abi` and
    /// emitted into the wait contract, never re-typed, so the C view can
    /// never drift from the source of truth.
    #[test]
    fn syscall_header_carries_the_reserved_load_failure_statuses() {
        let h = body("tairix_syscall.h");
        for (name, value) in [
            ("TAIRIX_LOAD_NOT_FOUND", tairix_abi::LOAD_NOT_FOUND),
            ("TAIRIX_LOAD_UNVERIFIED", tairix_abi::LOAD_UNVERIFIED),
            ("TAIRIX_LOAD_MALFORMED", tairix_abi::LOAD_MALFORMED),
            ("TAIRIX_LOAD_OOM", tairix_abi::LOAD_OOM),
        ] {
            assert!(
                h.contains(&format!("#define {name} ((int32_t){value})")),
                "load status {name}: {h}"
            );
        }
    }

    #[test]
    fn rlimit_header_pins_kinds_and_struct() {
        let h = body("tairix_rlimit.h");
        assert!(h.contains("#ifndef TAIRIX_RLIMIT_H"), "guard present");
        assert!(h.contains("#include <stdint.h>"), "stdint included");
        // The no-limit sentinel and a representative kind are read from
        // lib/abi, never re-typed.
        assert!(
            h.contains(&format!(
                "#define TAIRIX_RLIMIT_INFINITY ((uint64_t){RLIMIT_INFINITY}u)"
            )),
            "infinity sentinel: {h}"
        );
        assert!(
            h.contains("#define TAIRIX_LIMIT_KIND_PROCESSES ((uint32_t)2u)"),
            "processes kind macro: {h}"
        );
        assert!(
            h.contains(&format!(
                "#define TAIRIX_LIMIT_KIND_COUNT ((uint32_t){}u)",
                LimitKind::COUNT
            )),
            "kind count: {h}"
        );
        assert!(
            h.contains("typedef struct tairix_resource_limit {"),
            "resource-limit struct: {h}"
        );
        assert_eq!(
            core::mem::size_of::<ResourceLimit>(),
            16,
            "ResourceLimit repr(C) size"
        );
    }

    #[test]
    fn time_header_pins_layout_and_values() {
        let h = body("tairix_time.h");
        assert!(h.contains("#ifndef TAIRIX_TIME_H"), "guard present");
        assert!(h.contains("typedef struct tairix_time64 {"), "time struct");
        assert!(
            h.contains("typedef struct tairix_duration64 {"),
            "duration struct"
        );
        // Values are read from lib/abi, never re-typed: assert they match.
        assert!(h.contains(&format!("#define TAIRIX_NANOS_PER_SEC {NANOS_PER_SEC}u")));
        assert!(h.contains(&format!(
            "#define TAIRIX_TIME64_WIRE_LEN {}u",
            Time64::WIRE_LEN
        )));
        assert!(h.contains(&format!(
            "#define TAIRIX_RELEASE_EPOCH_SECS INT64_C({RELEASE_EPOCH_SECS})"
        )));
        assert!(h.contains(&format!(
            "#define TAIRIX_PLAUSIBLE_FUTURE_SECS INT64_C({PLAUSIBLE_FUTURE_SECS})"
        )));
        // The C struct mirrors the #[repr(C)] Rust layout (8 + 4 + 4 pad).
        assert_eq!(core::mem::size_of::<Time64>(), 16, "Time64 repr(C) size");
        assert_eq!(core::mem::align_of::<Time64>(), 8, "Time64 repr(C) align");
        assert_eq!(core::mem::size_of::<Duration64>(), 16, "Duration64 size");
    }

    #[test]
    fn random_header_pins_flags_and_limits() {
        let h = body("tairix_random.h");
        assert!(h.contains("#ifndef TAIRIX_RANDOM_H"), "guard present");
        assert!(h.contains("#include <stdint.h>"), "stdint included");
        // Values are read from lib/abi, never re-typed: assert they match.
        assert!(
            h.contains(&format!(
                "#define TAIRIX_RANDOM_FLAG_NON_BLOCKING {:#x}u",
                RandomFlags::NON_BLOCKING.bits()
            )),
            "non-blocking flag bit: {h}"
        );
        assert!(
            h.contains(&format!(
                "#define TAIRIX_RANDOM_RESERVE_DEFAULT_BYTES ((uintptr_t){RANDOM_RESERVE_DEFAULT_BYTES}u)"
            )),
            "reserve default: {h}"
        );
        assert!(
            h.contains(&format!(
                "#define TAIRIX_RANDOM_REQUEST_MAX_BYTES ((uintptr_t){RANDOM_REQUEST_MAX_BYTES}u)"
            )),
            "request max: {h}"
        );
    }

    #[test]
    fn memory_header_pins_map_flags() {
        let h = body("tairix_memory.h");
        assert!(h.contains("#ifndef TAIRIX_MEMORY_H"), "guard present");
        assert!(h.contains("#include <stdint.h>"), "stdint included");
        // The granule is read from lib/abi, never re-typed.
        assert!(
            h.contains(&format!(
                "#define TAIRIX_PAGE_SIZE ((uintptr_t){PAGE_SIZE}u)"
            )),
            "page granule: {h}"
        );
        // The flag bit value is read from lib/abi, never re-typed.
        assert!(
            h.contains(&format!(
                "#define TAIRIX_MAP_FLAG_FIXED {:#x}u",
                MapFlags::FIXED.bits()
            )),
            "fixed flag bit: {h}"
        );
    }

    #[test]
    fn hwtree_header_pins_enums_and_layout() {
        let h = body("tairix_hwtree.h");
        assert!(h.contains("#ifndef TAIRIX_HWTREE_H"), "guard present");
        assert!(h.contains("#include <stdint.h>"), "stdint included");
        // Values are read from lib/abi, never re-typed: assert they match.
        assert!(h.contains(&format!(
            "#define TAIRIX_HWTREE_VERSION {HWTREE_VERSION_V1}u"
        )));
        assert!(h.contains(&format!("#define TAIRIX_HW_NODE_ROOT {HW_NODE_ROOT}u")));
        assert!(
            h.contains(&format!(
                "#define TAIRIX_HW_NODE_WIRE_LEN {}u",
                HwNode::WIRE_LEN
            )),
            "node wire len: {h}"
        );
        assert!(
            h.contains(&format!(
                "#define TAIRIX_HW_CLASS_NETWORK ((uint16_t){}u)",
                HwDeviceClass::Network.as_u16()
            )),
            "device class macro: {h}"
        );
        assert!(
            h.contains(&format!(
                "#define TAIRIX_HW_MATCH_PCI ((uint16_t){}u)",
                HwMatchKind::Pci.as_u16()
            )),
            "match kind macro: {h}"
        );
        assert!(
            h.contains(&format!(
                "#define TAIRIX_HW_RES_IRQ ((uint16_t){}u)",
                HwResourceKind::Irq.as_u16()
            )),
            "resource kind macro: {h}"
        );
        assert!(h.contains("typedef struct tairix_hw_node {"), "node struct");
        // The flat record structs mirror their #[repr(C)] layout exactly,
        // so their wire size equals their in-memory size.
        assert_eq!(core::mem::size_of::<HwMatchKey>(), HwMatchKey::WIRE_LEN);
        assert_eq!(core::mem::size_of::<HwResource>(), HwResource::WIRE_LEN);
    }

    #[test]
    fn ipc_header_pins_layout_and_values() {
        use tairix_abi::ipc::IPC_MESSAGE_MAX_PAYLOAD_LEN;
        let h = body("tairix_ipc.h");
        assert!(h.contains("#ifndef TAIRIX_IPC_H"), "guard present");
        assert!(h.contains("#include <stdint.h>"), "stdint included");
        assert!(
            h.contains("typedef struct tairix_ipc_message_header {"),
            "message-header struct"
        );
        assert!(
            h.contains("typedef struct tairix_port_name {"),
            "port-name struct"
        );
        // Values are read from lib/abi, never re-typed: assert they match.
        assert!(
            h.contains(&format!(
                "#define TAIRIX_IPC_MESSAGE_HEADER_MAGIC {IPC_MESSAGE_HEADER_MAGIC:#x}u"
            )),
            "magic word: {h}"
        );
        assert!(
            h.contains(&format!(
                "#define TAIRIX_IPC_MESSAGE_MAX_PAYLOAD_LEN {IPC_MESSAGE_MAX_PAYLOAD_LEN}u"
            )),
            "max payload: {h}"
        );
        assert!(
            h.contains(&format!(
                "#define TAIRIX_IPC_MESSAGE_HEADER_WIRE_LEN {}u",
                IpcMessageHeader::WIRE_LEN
            )),
            "header wire len: {h}"
        );
        assert!(
            h.contains(&format!(
                "#define TAIRIX_PORT_NAME_MAX_LEN {PORT_NAME_MAX_LEN}u"
            )),
            "port-name max len: {h}"
        );
        assert!(
            h.contains(&format!(
                "#define TAIRIX_PORT_NAME_WIRE_LEN {}u",
                PortName::WIRE_LEN
            )),
            "port-name wire len: {h}"
        );
        assert!(
            h.contains(&format!(
                "#define TAIRIX_CALL_RECV_FLAG_NON_BLOCKING {:#x}u",
                CallRecvFlags::NON_BLOCKING.bits()
            )),
            "call_recv non-blocking flag bit: {h}"
        );
        // The C structs mirror the #[repr(C)] Rust layout.
        assert_eq!(
            core::mem::size_of::<IpcMessageHeader>(),
            32,
            "IpcMessageHeader repr(C) size"
        );
        assert_eq!(
            core::mem::align_of::<IpcMessageHeader>(),
            8,
            "IpcMessageHeader repr(C) align"
        );
        assert_eq!(
            core::mem::size_of::<PortName>(),
            32,
            "PortName repr(C) size"
        );
        assert_eq!(
            core::mem::align_of::<PortName>(),
            1,
            "PortName repr(C) align"
        );
    }

    #[test]
    fn stdinfo_header_pins_fd_versions_and_discriminants() {
        let h = body("tairix_stdinfo.h");
        assert!(h.contains("#ifndef TAIRIX_STDINFO_H"), "guard present");
        assert!(h.contains("#include <stdint.h>"), "stdint included");
        // Values are read from lib/abi, never re-typed: assert they match.
        assert!(
            h.contains(&format!("#define TAIRIX_STDINFO_FD {STDINFO_FD}u")),
            "reserved fd: {h}"
        );
        assert!(
            h.contains(&format!(
                "#define TAIRIX_STDINFO_VERSION_V1 {STDINFO_VERSION_V1}u"
            )),
            "version v1: {h}"
        );
        assert!(
            h.contains(&format!(
                "#define TAIRIX_STDINFO_VERSION_CURRENT {STDINFO_VERSION_CURRENT}u"
            )),
            "current version: {h}"
        );
        assert!(
            h.contains(&format!(
                "#define TAIRIX_STDINFO_KIND_OMISSION ((uint8_t){}u)",
                StdInfoKind::Omission as u8
            )),
            "omission discriminant: {h}"
        );
        assert!(
            h.contains(&format!(
                "#define TAIRIX_STDINFO_KIND_CONTEXT ((uint8_t){}u)",
                StdInfoKind::Context as u8
            )),
            "context discriminant: {h}"
        );
        assert!(
            h.contains(&format!(
                "#define TAIRIX_STDINFO_SEVERITY_INFO ((uint8_t){}u)",
                Severity::Info as u8
            )),
            "info severity: {h}"
        );
        assert!(
            h.contains(&format!(
                "#define TAIRIX_STDINFO_SEVERITY_DEBUG ((uint8_t){}u)",
                Severity::Debug as u8
            )),
            "debug severity: {h}"
        );
    }

    #[test]
    fn manifest_header_pins_layout_and_values() {
        let h = body("tairix_manifest.h");
        assert!(h.contains("#ifndef TAIRIX_MANIFEST_H"), "guard present");
        assert!(h.contains("#include <stdint.h>"), "stdint included");
        assert!(
            h.contains("typedef struct tairix_manifest_header {"),
            "manifest-header struct"
        );
        // Values are read from lib/abi, never re-typed: assert they match.
        assert!(
            h.contains(&format!(
                "#define TAIRIX_MANIFEST_MAGIC {MANIFEST_MAGIC:#x}u"
            )),
            "magic word: {h}"
        );
        assert!(
            h.contains(&format!(
                "#define TAIRIX_MANIFEST_MAX_CAPABILITIES {MANIFEST_MAX_CAPABILITIES}u"
            )),
            "max capabilities: {h}"
        );
        assert!(
            h.contains(&format!(
                "#define TAIRIX_SYSCALL_TABLE_HASH_LEN {SYSCALL_TABLE_HASH_LEN}u"
            )),
            "hash length: {h}"
        );
        assert!(
            h.contains(&format!(
                "#define TAIRIX_MANIFEST_HEADER_WIRE_LEN {}u",
                ManifestHeader::WIRE_LEN
            )),
            "header wire len: {h}"
        );
        // The C struct mirrors the #[repr(C)] Rust layout (no trailing pad).
        assert_eq!(
            core::mem::size_of::<ManifestHeader>(),
            ManifestHeader::WIRE_LEN,
            "ManifestHeader repr(C) size equals wire len"
        );
        assert_eq!(
            core::mem::align_of::<ManifestHeader>(),
            4,
            "ManifestHeader repr(C) align"
        );
    }

    #[test]
    fn input_header_pins_constants_and_discriminants() {
        use tairix_abi::{
            KeyInput, NamedKeyCode, PointerButtonCode, PointerInput, BUTTON_NONE, KEY_CLASS_CHAR,
            KEY_CLASS_NAMED, KEY_INPUT_MAGIC, KIND_KEY_PRESSED, KIND_KEY_RELEASED, KIND_MOVED_BY,
            KIND_PRESSED, KIND_RELEASED, KIND_SCROLLED, MOD_ALT, MOD_CTRL, MOD_MASK, MOD_META,
            MOD_SHIFT, POINTER_INPUT_MAGIC,
        };
        let h = body("tairix_input.h");
        assert!(h.contains("#ifndef TAIRIX_INPUT_H"), "guard present");
        assert!(h.contains("#include <stdint.h>"), "stdint included");

        // Values are read from lib/abi, never re-typed: assert they match.
        let mut expected = vec![
            format!("#define TAIRIX_POINTER_INPUT_MAGIC {POINTER_INPUT_MAGIC:#x}u"),
            format!("#define TAIRIX_KEY_INPUT_MAGIC {KEY_INPUT_MAGIC:#x}u"),
            format!(
                "#define TAIRIX_POINTER_INPUT_WIRE_LEN {}u",
                PointerInput::WIRE_LEN
            ),
            format!("#define TAIRIX_KEY_INPUT_WIRE_LEN {}u", KeyInput::WIRE_LEN),
        ];
        for (name, value) in [
            ("TAIRIX_INPUT_KIND_MOVED_BY", KIND_MOVED_BY),
            ("TAIRIX_INPUT_KIND_PRESSED", KIND_PRESSED),
            ("TAIRIX_INPUT_KIND_RELEASED", KIND_RELEASED),
            ("TAIRIX_INPUT_KIND_SCROLLED", KIND_SCROLLED),
            ("TAIRIX_INPUT_KIND_KEY_PRESSED", KIND_KEY_PRESSED),
            ("TAIRIX_INPUT_KIND_KEY_RELEASED", KIND_KEY_RELEASED),
            ("TAIRIX_INPUT_BUTTON_NONE", BUTTON_NONE),
            (
                "TAIRIX_POINTER_BUTTON_PRIMARY",
                PointerButtonCode::Primary.code(),
            ),
            (
                "TAIRIX_POINTER_BUTTON_SECONDARY",
                PointerButtonCode::Secondary.code(),
            ),
            (
                "TAIRIX_POINTER_BUTTON_MIDDLE",
                PointerButtonCode::Middle.code(),
            ),
            ("TAIRIX_KEY_CLASS_CHAR", KEY_CLASS_CHAR),
            ("TAIRIX_KEY_CLASS_NAMED", KEY_CLASS_NAMED),
        ] {
            expected.push(format!("#define {name} ((uint16_t){value}u)"));
        }
        for (name, bits) in [
            ("TAIRIX_MOD_SHIFT", MOD_SHIFT),
            ("TAIRIX_MOD_CTRL", MOD_CTRL),
            ("TAIRIX_MOD_ALT", MOD_ALT),
            ("TAIRIX_MOD_META", MOD_META),
            ("TAIRIX_MOD_MASK", MOD_MASK),
        ] {
            expected.push(format!("#define {name} ((uint16_t){bits:#x}u)"));
        }
        for (name, code) in NAMED_KEY_CODES {
            expected.push(format!("#define {name} ((uint16_t){code}u)"));
        }
        for line in &expected {
            assert!(h.contains(line), "missing `{line}` in:\n{h}");
        }
        // The named-key discriminants are frozen at their lib/abi values.
        assert_eq!(NamedKeyCode::Enter.code(), 1, "Enter wire code frozen");
        assert_eq!(NamedKeyCode::F12.code(), 26, "F12 wire code frozen");
    }

    /// The C mirror covers the whole manifest header and nothing more. A
    /// field added to `AppInfoHeader` without a row in
    /// [`APPINFO_HEADER_FIELDS`] leaves the widths short and fails here,
    /// instead of shipping a C struct of the same name and a different shape.
    #[test]
    fn the_c_appinfo_mirror_covers_every_wire_byte() {
        use tairix_abi::AppInfoHeader;
        let total: usize = APPINFO_HEADER_FIELDS.iter().map(|&(_, width)| width).sum();
        assert_eq!(
            total,
            AppInfoHeader::WIRE_LEN,
            "C mirror field widths must sum to the manifest wire length"
        );
        let h = body("tairix_appinfo.h");
        for &(declaration, _) in APPINFO_HEADER_FIELDS {
            assert!(
                h.contains(&format!("    {declaration};")),
                "missing `{declaration}` in:\n{h}"
            );
        }
    }

    #[test]
    fn appinfo_header_pins_layout_constants_and_names() {
        use tairix_abi::{
            AppInfoHeader, BundleEntry, LibraryCategory, LibraryScope, APPINFO_MAGIC,
            APPINFO_MAX_CAPABILITIES, APPINFO_MAX_MIME, BUNDLE_ID_MAX, BUNDLE_NAME_MAX,
            BUNDLE_VERSION_MAX, LIBRARY_ICON_MAX, MIME_ENTRY_LEN, MIME_TYPE_MAX,
            SYSTEM_LIBRARIES_DIR,
        };
        let h = body("tairix_appinfo.h");
        assert!(h.contains("#ifndef TAIRIX_APPINFO_H"), "guard present");
        assert!(h.contains("#include <stdint.h>"), "stdint included");
        assert!(
            h.contains("typedef struct tairix_appinfo_header {"),
            "appinfo-header struct"
        );
        // Values are read from lib/abi, never re-typed: assert they match.
        let expected = [
            format!("#define TAIRIX_APPINFO_MAGIC {APPINFO_MAGIC:#x}u"),
            format!("#define TAIRIX_APPINFO_MAX_CAPABILITIES {APPINFO_MAX_CAPABILITIES}u"),
            format!("#define TAIRIX_APPINFO_MAX_MIME {APPINFO_MAX_MIME}u"),
            format!("#define TAIRIX_BUNDLE_ID_MAX {BUNDLE_ID_MAX}u"),
            format!("#define TAIRIX_BUNDLE_NAME_MAX {BUNDLE_NAME_MAX}u"),
            format!("#define TAIRIX_BUNDLE_VERSION_MAX {BUNDLE_VERSION_MAX}u"),
            format!("#define TAIRIX_MIME_TYPE_MAX {MIME_TYPE_MAX}u"),
            format!("#define TAIRIX_MIME_ENTRY_LEN {MIME_ENTRY_LEN}u"),
            format!("#define TAIRIX_LIBRARY_ICON_MAX {LIBRARY_ICON_MAX}u"),
            format!(
                "#define TAIRIX_APPINFO_HEADER_WIRE_LEN {}u",
                AppInfoHeader::WIRE_LEN
            ),
            format!("#define TAIRIX_SYSTEM_LIBRARIES_DIR \"{SYSTEM_LIBRARIES_DIR}\""),
            format!(
                "#define TAIRIX_LIBRARY_SCOPE_BUNDLE ((uint8_t){}u)",
                LibraryScope::Bundle as u8
            ),
            format!(
                "#define TAIRIX_LIBRARY_SCOPE_SYSTEM ((uint8_t){}u)",
                LibraryScope::System as u8
            ),
        ];
        for line in &expected {
            assert!(h.contains(line), "missing `{line}` in:\n{h}");
        }
        // Every permitted bundle entry name is exported, read from lib/abi.
        for entry in BundleEntry::ALL {
            let line = format!(
                "#define TAIRIX_BUNDLE_ENTRY_{} \"{}\"",
                entry.as_str().to_ascii_uppercase(),
                entry.as_str()
            );
            assert!(h.contains(&line), "missing `{line}` in:\n{h}");
        }
        // Every program-library folder wire byte is exported, read from
        // lib/abi, alongside the "not listed" zero and the struct fields
        // that carry the listing.
        assert!(
            h.contains("#define TAIRIX_APPINFO_LIBRARY_NONE ((uint8_t)0u)"),
            "missing the not-listed wire byte in:\n{h}"
        );
        for category in LibraryCategory::ALL {
            let line = format!(
                "#define TAIRIX_APPINFO_LIBRARY_{} ((uint8_t){}u)",
                category.as_str().to_ascii_uppercase(),
                LibraryCategory::to_wire(Some(category))
            );
            assert!(h.contains(&line), "missing `{line}` in:\n{h}");
        }
        assert!(
            h.contains("uint8_t library_icon[TAIRIX_LIBRARY_ICON_MAX];"),
            "missing the library-icon field in:\n{h}"
        );
        // The listing wire encoding is frozen at its lib/abi values.
        assert_eq!(
            LibraryCategory::to_wire(Some(LibraryCategory::Accessories)),
            1,
            "Accessories wire byte frozen"
        );
        assert_eq!(
            LibraryCategory::to_wire(Some(LibraryCategory::Other)),
            10,
            "Other wire byte frozen"
        );
        // The C struct mirrors the #[repr(C)] Rust layout (no trailing pad).
        assert_eq!(
            core::mem::size_of::<AppInfoHeader>(),
            AppInfoHeader::WIRE_LEN,
            "AppInfoHeader repr(C) size equals wire len"
        );
        assert_eq!(
            core::mem::align_of::<AppInfoHeader>(),
            4,
            "AppInfoHeader repr(C) align"
        );
        // The library-scope discriminants are frozen at their lib/abi values.
        assert_eq!(LibraryScope::Bundle as u8, 0, "Bundle scope discriminant");
        assert_eq!(LibraryScope::System as u8, 1, "System scope discriminant");
    }

    #[test]
    fn rxe_header_pins_layout_constants_and_discriminants() {
        use tairix_abi::{
            LoadHeader, RxePermission, Segment, LOAD_FLAG_PIE, LOAD_MAGIC, LOAD_MAX_SEGMENTS,
            RXE_PAGE_SIZE, SEG_FLAG_EXEC, SEG_FLAG_READ, SEG_FLAG_WRITE, SYSCALL_TABLE_HASH_LEN,
        };
        let h = body("tairix_rxe.h");
        assert!(h.contains("#ifndef TAIRIX_RXE_H"), "guard present");
        assert!(h.contains("#include <stdint.h>"), "stdint included");
        assert!(
            h.contains("typedef struct tairix_load_header {"),
            "load-header struct"
        );
        // Values are read from lib/abi, never re-typed: assert they match.
        let expected = [
            format!("#define TAIRIX_LOAD_MAGIC {LOAD_MAGIC:#x}u"),
            format!("#define TAIRIX_RXE_PAGE_SIZE ((uint64_t){RXE_PAGE_SIZE}ull)"),
            format!("#define TAIRIX_LOAD_MAX_SEGMENTS ((uintptr_t){LOAD_MAX_SEGMENTS}u)"),
            format!("#define TAIRIX_LOAD_FLAG_PIE {LOAD_FLAG_PIE:#x}u"),
            format!("#define TAIRIX_SEG_FLAG_READ {SEG_FLAG_READ:#x}u"),
            format!("#define TAIRIX_SEG_FLAG_WRITE {SEG_FLAG_WRITE:#x}u"),
            format!("#define TAIRIX_SEG_FLAG_EXEC {SEG_FLAG_EXEC:#x}u"),
            format!(
                "#define TAIRIX_LOAD_HEADER_WIRE_LEN {}u",
                LoadHeader::WIRE_LEN
            ),
            format!("#define TAIRIX_SEGMENT_WIRE_LEN {}u", Segment::WIRE_LEN),
            format!(
                "#define TAIRIX_RXE_PERMISSION_READ_ONLY ((uint8_t){}u)",
                RxePermission::ReadOnly as u8
            ),
            format!(
                "#define TAIRIX_RXE_PERMISSION_READ_EXECUTE ((uint8_t){}u)",
                RxePermission::ReadExecute as u8
            ),
            format!(
                "#define TAIRIX_RXE_PERMISSION_READ_WRITE ((uint8_t){}u)",
                RxePermission::ReadWrite as u8
            ),
            format!("uint8_t cfi_tag[{SYSCALL_TABLE_HASH_LEN}];"),
        ];
        for line in &expected {
            assert!(h.contains(line), "missing `{line}` in:\n{h}");
        }
        // The C struct mirrors the #[repr(C)] Rust layout (no trailing pad).
        assert_eq!(
            core::mem::size_of::<LoadHeader>(),
            LoadHeader::WIRE_LEN,
            "LoadHeader repr(C) size equals wire len"
        );
        assert_eq!(
            core::mem::align_of::<LoadHeader>(),
            8,
            "LoadHeader repr(C) align"
        );
        // The permission discriminants are frozen at their lib/abi values.
        assert_eq!(RxePermission::ReadOnly as u8, 0, "ReadOnly discriminant");
        assert_eq!(
            RxePermission::ReadExecute as u8,
            1,
            "ReadExecute discriminant"
        );
        assert_eq!(RxePermission::ReadWrite as u8, 2, "ReadWrite discriminant");
    }

    #[test]
    fn process_header_pins_layout_constants_and_sizes() {
        use tairix_abi::{
            ProcessStartHeader, StringSlot, PROCESS_START_MAGIC, PROCESS_START_MAX_STRINGS,
            PROCESS_START_MAX_STRING_LEN, PROCESS_START_MAX_TOTAL_LEN,
        };
        let h = body("tairix_process.h");
        assert!(h.contains("#ifndef TAIRIX_PROCESS_H"), "guard present");
        assert!(h.contains("#include <stdint.h>"), "stdint included");
        assert!(
            h.contains("typedef struct tairix_process_start_header {"),
            "start-header struct"
        );
        assert!(
            h.contains("typedef struct tairix_string_slot {"),
            "string-slot struct"
        );
        // Values are read from lib/abi, never re-typed: assert they match.
        let expected = [
            format!("#define TAIRIX_PROCESS_START_MAGIC {PROCESS_START_MAGIC:#x}u"),
            format!("#define TAIRIX_PROCESS_START_MAX_STRINGS {PROCESS_START_MAX_STRINGS}u"),
            format!("#define TAIRIX_PROCESS_START_MAX_STRING_LEN {PROCESS_START_MAX_STRING_LEN}u"),
            format!(
                "#define TAIRIX_PROCESS_START_MAX_TOTAL_LEN ((uint64_t){PROCESS_START_MAX_TOTAL_LEN}ull)"
            ),
            format!(
                "#define TAIRIX_PROCESS_START_HEADER_WIRE_LEN {}u",
                ProcessStartHeader::WIRE_LEN
            ),
            format!("#define TAIRIX_STRING_SLOT_WIRE_LEN {}u", StringSlot::WIRE_LEN),
        ];
        for line in &expected {
            assert!(h.contains(line), "missing `{line}` in:\n{h}");
        }
        // The C struct mirrors the #[repr(C)] Rust layout (no trailing pad).
        assert_eq!(
            core::mem::size_of::<ProcessStartHeader>(),
            ProcessStartHeader::WIRE_LEN,
            "ProcessStartHeader repr(C) size equals wire len"
        );
        assert_eq!(
            core::mem::size_of::<StringSlot>(),
            StringSlot::WIRE_LEN,
            "StringSlot repr(C) size equals wire len"
        );
    }

    #[test]
    fn sysinfo_header_pins_layout_constants_and_discriminants() {
        use tairix_abi::{
            KernelMemoryStats, MountListRequest, MountRecord, ProcessListRequest, ProcessRecord,
            ProcessState, ResourceLimitRecord, SysinfoQueryId, SysinfoRequestHeader,
            SystemIdentity, Uptime, ENCODED_QUERY_TABLE_LEN, HOSTNAME_MAX, MACHINE_ID_LEN,
            MOUNT_FSTYPE_MAX, MOUNT_SOURCE_MAX, MOUNT_TARGET_MAX, PROCESS_NAME_MAX,
            RESOURCE_LIMITS_REPORT_LEN, SYSINFO_MAX_PAYLOAD_LEN, SYSINFO_QUERY_NAME_MAX,
            SYSINFO_QUERY_RECORD_LEN, SYSINFO_REQUEST_MAGIC, SYSINFO_VERSION_CURRENT,
            SYSINFO_VERSION_V1,
        };
        let h = body("tairix_sysinfo.h");
        assert!(h.contains("#ifndef TAIRIX_SYSINFO_H"), "guard present");
        assert!(h.contains("#include <stdint.h>"), "stdint included");
        assert!(
            h.contains("#include \"tairix_time.h\""),
            "time header included for tairix_uptime"
        );
        assert!(
            h.contains("#include \"tairix_rlimit.h\""),
            "rlimit header included for tairix_resource_limit_t"
        );
        // Values are read from lib/abi, never re-typed: assert they match.
        let expected = [
            format!("#define TAIRIX_SYSINFO_VERSION_V1 {SYSINFO_VERSION_V1}u"),
            format!("#define TAIRIX_SYSINFO_VERSION_CURRENT {SYSINFO_VERSION_CURRENT}u"),
            format!("#define TAIRIX_SYSINFO_REQUEST_MAGIC {SYSINFO_REQUEST_MAGIC:#x}u"),
            format!("#define TAIRIX_SYSINFO_MAX_PAYLOAD_LEN {SYSINFO_MAX_PAYLOAD_LEN}u"),
            format!(
                "#define TAIRIX_SYSINFO_QUERY_ID_MAX {}u",
                SysinfoQueryId::MAX
            ),
            format!("#define TAIRIX_SYSINFO_QUERY_NAME_MAX {SYSINFO_QUERY_NAME_MAX}u"),
            format!("#define TAIRIX_SYSINFO_QUERY_RECORD_LEN {SYSINFO_QUERY_RECORD_LEN}u"),
            format!("#define TAIRIX_SYSINFO_ENCODED_QUERY_TABLE_LEN {ENCODED_QUERY_TABLE_LEN}u"),
            format!(
                "#define TAIRIX_SYSINFO_QUERY_SELF_PROCESS_LIST ((uint16_t){}u)",
                SysinfoQueryId::SELF_PROCESS_LIST.as_u16()
            ),
            format!(
                "#define TAIRIX_SYSINFO_QUERY_MOUNT_LIST ((uint16_t){}u)",
                SysinfoQueryId::MOUNT_LIST.as_u16()
            ),
            format!(
                "#define TAIRIX_PROCESS_STATE_RUNNABLE ((uint8_t){}u)",
                ProcessState::Runnable as u8
            ),
            format!(
                "#define TAIRIX_PROCESS_STATE_STOPPED ((uint8_t){}u)",
                ProcessState::Stopped as u8
            ),
            format!("#define TAIRIX_PROCESS_NAME_MAX {PROCESS_NAME_MAX}u"),
            format!("#define TAIRIX_MACHINE_ID_LEN {MACHINE_ID_LEN}u"),
            format!("#define TAIRIX_HOSTNAME_MAX {HOSTNAME_MAX}u"),
            format!("#define TAIRIX_MOUNT_SOURCE_MAX {MOUNT_SOURCE_MAX}u"),
            format!("#define TAIRIX_MOUNT_TARGET_MAX {MOUNT_TARGET_MAX}u"),
            format!("#define TAIRIX_MOUNT_FSTYPE_MAX {MOUNT_FSTYPE_MAX}u"),
            format!(
                "#define TAIRIX_SYSINFO_REQUEST_HEADER_WIRE_LEN {}u",
                SysinfoRequestHeader::WIRE_LEN
            ),
            format!(
                "#define TAIRIX_PROCESS_LIST_REQUEST_WIRE_LEN {}u",
                ProcessListRequest::WIRE_LEN
            ),
            format!(
                "#define TAIRIX_PROCESS_RECORD_WIRE_LEN {}u",
                ProcessRecord::WIRE_LEN
            ),
            format!(
                "#define TAIRIX_KERNEL_MEMORY_STATS_WIRE_LEN {}u",
                KernelMemoryStats::WIRE_LEN
            ),
            format!("#define TAIRIX_UPTIME_WIRE_LEN {}u", Uptime::WIRE_LEN),
            format!(
                "#define TAIRIX_SYSTEM_IDENTITY_WIRE_LEN {}u",
                SystemIdentity::WIRE_LEN
            ),
            format!(
                "#define TAIRIX_MOUNT_LIST_REQUEST_WIRE_LEN {}u",
                MountListRequest::WIRE_LEN
            ),
            format!(
                "#define TAIRIX_MOUNT_RECORD_WIRE_LEN {}u",
                MountRecord::WIRE_LEN
            ),
            format!(
                "#define TAIRIX_SYSINFO_QUERY_RESOURCE_LIMITS ((uint16_t){}u)",
                SysinfoQueryId::RESOURCE_LIMITS.as_u16()
            ),
            format!(
                "#define TAIRIX_RESOURCE_LIMIT_RECORD_WIRE_LEN {}u",
                ResourceLimitRecord::WIRE_LEN
            ),
            format!(
                "#define TAIRIX_SYSINFO_RESOURCE_LIMITS_REPORT_LEN {RESOURCE_LIMITS_REPORT_LEN}u"
            ),
        ];
        for line in &expected {
            assert!(h.contains(line), "missing `{line}` in:\n{h}");
        }
    }

    #[test]
    fn sysinfo_header_declares_every_record_typedef() {
        let h = body("tairix_sysinfo.h");
        for typedef in [
            "typedef struct tairix_sysinfo_request_header {",
            "typedef struct tairix_process_list_request {",
            "typedef struct tairix_process_record {",
            "typedef struct tairix_kernel_memory_stats {",
            "typedef struct tairix_uptime {",
            "typedef struct tairix_system_identity {",
            "typedef struct tairix_mount_list_request {",
            "typedef struct tairix_mount_record {",
            "typedef struct tairix_resource_limit_record {",
            "typedef struct tairix_user_directory_request {",
            "typedef struct tairix_user_directory_record {",
        ] {
            assert!(h.contains(typedef), "missing `{typedef}` in:\n{h}");
        }
    }

    /// A mount record's storage-medium byte is only readable from C if the
    /// header publishes its encoding, so every value is pinned against the
    /// `lib/abi` encoder the generator reads — including the unknown a
    /// backing-less mount and an unrecognised class both take — and the byte
    /// is pinned to its place between the availability state and the usage
    /// block.
    #[test]
    fn sysinfo_header_publishes_the_mount_medium_encoding() {
        let h = body("tairix_sysinfo.h");
        for (macro_name, medium) in [
            ("TAIRIX_MOUNT_MEDIUM_UNKNOWN", None),
            (
                "TAIRIX_MOUNT_MEDIUM_ROTATIONAL",
                Some(BlkDeviceClass::Rotational),
            ),
            (
                "TAIRIX_MOUNT_MEDIUM_SOLID_STATE",
                Some(BlkDeviceClass::SolidState),
            ),
            (
                "TAIRIX_MOUNT_MEDIUM_REMOVABLE",
                Some(BlkDeviceClass::Removable),
            ),
            ("TAIRIX_MOUNT_MEDIUM_VIRTUAL", Some(BlkDeviceClass::Virtual)),
        ] {
            let line = format!(
                "#define {macro_name} ((uint8_t){}u)",
                MountRecord::medium_to_wire(medium)
            );
            assert!(h.contains(&line), "missing `{line}` in:\n{h}");
        }
        assert!(
            h.contains(concat!(
                "    uint8_t availability;\n",
                "    uint8_t medium;\n",
                "    uint8_t reserved0[7];\n",
                "    tairix_volume_stats_t usage;\n",
            )),
            "mount record medium placement: {h}"
        );
    }

    /// The naturally-aligned `#[repr(C)]` in-memory pins for the sysinfo
    /// wire types (the separate `*_WIRE_LEN` macros give the packed wire
    /// size), shared by `sysinfo_header_struct_layout_matches_lib_abi`.
    fn sysinfo_struct_pins() -> [(&'static str, usize, usize, usize, usize); 11] {
        use tairix_abi::{
            KernelMemoryStats, MountListRequest, MountRecord, ProcessListRequest, ProcessRecord,
            ResourceLimitRecord, SysinfoRequestHeader, SystemIdentity, Uptime, UserDirectoryRecord,
            UserDirectoryRequest,
        };
        [
            (
                "SysinfoRequestHeader",
                core::mem::size_of::<SysinfoRequestHeader>(),
                24,
                core::mem::align_of::<SysinfoRequestHeader>(),
                8,
            ),
            (
                "ProcessListRequest",
                core::mem::size_of::<ProcessListRequest>(),
                8,
                core::mem::align_of::<ProcessListRequest>(),
                4,
            ),
            (
                "ProcessRecord",
                core::mem::size_of::<ProcessRecord>(),
                136,
                core::mem::align_of::<ProcessRecord>(),
                8,
            ),
            (
                "KernelMemoryStats",
                core::mem::size_of::<KernelMemoryStats>(),
                40,
                core::mem::align_of::<KernelMemoryStats>(),
                8,
            ),
            (
                "Uptime",
                core::mem::size_of::<Uptime>(),
                32,
                core::mem::align_of::<Uptime>(),
                8,
            ),
            (
                "SystemIdentity",
                core::mem::size_of::<SystemIdentity>(),
                88,
                core::mem::align_of::<SystemIdentity>(),
                2,
            ),
            (
                "MountListRequest",
                core::mem::size_of::<MountListRequest>(),
                8,
                core::mem::align_of::<MountListRequest>(),
                4,
            ),
            (
                "MountRecord",
                core::mem::size_of::<MountRecord>(),
                224,
                core::mem::align_of::<MountRecord>(),
                8,
            ),
            (
                "ResourceLimitRecord",
                core::mem::size_of::<ResourceLimitRecord>(),
                32,
                core::mem::align_of::<ResourceLimitRecord>(),
                8,
            ),
            (
                "UserDirectoryRequest",
                core::mem::size_of::<UserDirectoryRequest>(),
                8,
                core::mem::align_of::<UserDirectoryRequest>(),
                4,
            ),
            (
                "UserDirectoryRecord",
                core::mem::size_of::<UserDirectoryRecord>(),
                40,
                core::mem::align_of::<UserDirectoryRecord>(),
                4,
            ),
        ]
    }

    #[test]
    fn sysinfo_header_struct_layout_matches_lib_abi() {
        use tairix_abi::{ProcessState, SysinfoQueryId};
        for (name, size, want_size, align, want_align) in sysinfo_struct_pins() {
            assert_eq!(size, want_size, "{name} repr(C) size");
            assert_eq!(align, want_align, "{name} repr(C) align");
        }
        // The well-known query ids and process-state discriminants are frozen.
        assert_eq!(
            SysinfoQueryId::SELF_PROCESS_LIST.as_u16(),
            0,
            "self list id"
        );
        assert_eq!(SysinfoQueryId::MOUNT_LIST.as_u16(), 6, "mount list id");
        assert_eq!(
            SysinfoQueryId::RESOURCE_LIMITS.as_u16(),
            7,
            "resource limits id"
        );
        assert_eq!(ProcessState::Runnable as u8, 0, "Runnable discriminant");
        assert_eq!(ProcessState::Stopped as u8, 4, "Stopped discriminant");
    }

    #[test]
    fn driver_header_pins_layout_constants_and_discriminants() {
        let h = body("tairix_driver.h");
        assert!(h.contains("#ifndef TAIRIX_DRIVER_H"), "guard present");
        assert!(h.contains("#include <stdint.h>"), "stdint included");
        // Reuses the syscall-table-hash length from the manifest header (no
        // re-declaration;).
        assert!(
            h.contains("#include \"tairix_manifest.h\""),
            "manifest header included for TAIRIX_SYSCALL_TABLE_HASH_LEN: {h}"
        );
        assert!(
            h.contains("typedef struct tairix_driver_manifest {"),
            "manifest struct mirror: {h}"
        );
        // The bind-table entry embeds the hwtree match key, so the header
        // pulls in tairix_hwtree.h (: no re-declaration).
        assert!(
            h.contains("#include \"tairix_hwtree.h\""),
            "hwtree header included for tairix_hw_match_key_t: {h}"
        );
        assert!(
            h.contains("typedef struct tairix_driver_bind_key {"),
            "bind-key struct mirror: {h}"
        );
        // Values are read from lib/abi, never re-typed: assert they match.
        let expected = [
            format!("#define TAIRIX_DRIVER_MANIFEST_MAGIC {DRIVER_MANIFEST_MAGIC:#x}u"),
            format!(
                "#define TAIRIX_DRIVER_MANIFEST_MAX_CAPABILITIES {DRIVER_MANIFEST_MAX_CAPABILITIES}u"
            ),
            format!("#define TAIRIX_DRIVER_MANIFEST_MAX_BIND_KEYS {DRIVER_MANIFEST_MAX_BIND_KEYS}u"),
            format!("#define TAIRIX_DRIVER_SIGNER_PUBKEY_LEN {DRIVER_SIGNER_PUBKEY_LEN}u"),
            format!("#define TAIRIX_DRIVER_SIGNATURE_LEN {DRIVER_SIGNATURE_LEN}u"),
            format!(
                "#define TAIRIX_DRIVER_MANIFEST_WIRE_LEN {}u",
                DriverManifest::WIRE_LEN
            ),
            format!(
                "#define TAIRIX_DRIVER_BIND_KEY_WIRE_LEN {}u",
                DriverBindKey::WIRE_LEN
            ),
            format!(
                "#define TAIRIX_DRIVER_KIND_USER_SPACE ((uint8_t){}u)",
                DriverKind::UserSpace.as_u8()
            ),
            format!(
                "#define TAIRIX_DRIVER_KIND_IN_KERNEL ((uint8_t){}u)",
                DriverKind::InKernel.as_u8()
            ),
            format!(
                "#define TAIRIX_BUFFER_CLASS_NON_SENSITIVE ((uint8_t){}u)",
                BufferClass::NonSensitive.as_u8()
            ),
            format!(
                "#define TAIRIX_BUFFER_CLASS_SENSITIVE ((uint8_t){}u)",
                BufferClass::Sensitive.as_u8()
            ),
            format!(
                "#define TAIRIX_DRIVER_HANDLE_NONE ((uint64_t){}ull)",
                DriverHandle::NONE.as_u64()
            ),
            format!(
                "#define TAIRIX_DRIVER_ERROR_PERMISSION_DENIED ((int32_t){})",
                DriverError::PermissionDenied.as_i32()
            ),
            format!(
                "#define TAIRIX_DRIVER_ERROR_NO_SPACE ((int32_t){})",
                DriverError::NoSpace.as_i32()
            ),
            format!("#define TAIRIX_DRIVER_REGISTER_REPLY_MAGIC {DRIVER_REGISTER_REPLY_MAGIC:#x}u"),
            format!("#define TAIRIX_DRIVER_REGISTER_STATUS_OK ((int32_t){DRIVER_REGISTER_STATUS_OK})"),
            format!(
                "#define TAIRIX_DRIVER_REGISTER_REPLY_WIRE_LEN {}u",
                DriverRegisterReply::WIRE_LEN
            ),
        ];
        for line in &expected {
            assert!(h.contains(line), "missing `{line}` in:\n{h}");
        }
        // The C struct mirrors the #[repr(C)] Rust layout with no trailing
        // padding, so the in-memory size equals the packed wire size.
        assert_eq!(
            core::mem::size_of::<DriverManifest>(),
            DriverManifest::WIRE_LEN,
            "DriverManifest repr(C) size == wire size"
        );
        assert_eq!(
            core::mem::size_of::<DriverRegisterReply>(),
            DriverRegisterReply::WIRE_LEN,
            "DriverRegisterReply repr(C) size == wire size"
        );
        assert_eq!(
            core::mem::size_of::<DriverBindKey>(),
            DriverBindKey::WIRE_LEN,
            "DriverBindKey repr(C) size == wire size"
        );
    }

    #[test]
    fn driver_header_pins_submodule_constants_and_discriminants() {
        use tairix_abi::driver::display::DisplayFormat;
        use tairix_abi::driver::filesystem::{MountFlags, NodeId, NodeKind};
        use tairix_abi::driver::input::InputEventKind;
        use tairix_abi::{VIRTIO_PCI_CFG_COMMON, VIRTIO_PCI_CFG_PCI, VIRTIO_PCI_VENDOR_ID};

        let h = body("tairix_driver.h");
        // NodeTimes mirrors tairix_time64_t, so the header must pull in time.
        assert!(
            h.contains("#include \"tairix_time.h\""),
            "time header included for tairix_time64_t: {h}"
        );
        // Every value/discriminant is read from lib/abi, never re-typed.
        let expected = [
            format!("#define TAIRIX_VIRTIO_PCI_VENDOR_ID ((uint16_t){VIRTIO_PCI_VENDOR_ID:#x}u)"),
            format!("#define TAIRIX_VIRTIO_PCI_CFG_COMMON ((uint8_t){VIRTIO_PCI_CFG_COMMON}u)"),
            format!("#define TAIRIX_VIRTIO_PCI_CFG_PCI ((uint8_t){VIRTIO_PCI_CFG_PCI}u)"),
            format!(
                "#define TAIRIX_MOUNT_FLAG_READ_ONLY ((uint32_t){:#x}u)",
                MountFlags::READ_ONLY.bits()
            ),
            format!(
                "#define TAIRIX_MOUNT_FLAG_NOEXEC ((uint32_t){:#x}u)",
                MountFlags::NOEXEC.bits()
            ),
            format!(
                "#define TAIRIX_MOUNT_FLAG_KNOWN_MASK ((uint32_t){:#x}u)",
                MountFlags::KNOWN_MASK.bits()
            ),
            format!(
                "#define TAIRIX_NODE_ID_NONE ((uint64_t){}ull)",
                NodeId::NONE.raw()
            ),
            format!(
                "#define TAIRIX_DISPLAY_FORMAT_RGBA8888 ((uint8_t){}u)",
                DisplayFormat::Rgba8888.as_u8()
            ),
            format!(
                "#define TAIRIX_DISPLAY_FORMAT_BGRA8888 ((uint8_t){}u)",
                DisplayFormat::Bgra8888.as_u8()
            ),
            format!(
                "#define TAIRIX_NODE_KIND_DIRECTORY ((uint8_t){}u)",
                NodeKind::Directory as u8
            ),
            format!(
                "#define TAIRIX_NODE_KIND_REGULAR_FILE ((uint8_t){}u)",
                NodeKind::RegularFile as u8
            ),
            format!(
                "#define TAIRIX_INPUT_EVENT_KIND_KEY ((uint8_t){}u)",
                InputEventKind::Key.as_u8()
            ),
            format!(
                "#define TAIRIX_INPUT_EVENT_KIND_SCROLL ((uint8_t){}u)",
                InputEventKind::Scroll.as_u8()
            ),
        ];
        for line in &expected {
            assert!(h.contains(line), "missing `{line}` in:\n{h}");
        }
        // The driver input-event kinds must stay disjoint from the windowing
        // input kinds in tairix_input.h (different ABIs).
        assert!(
            !body("tairix_input.h").contains("TAIRIX_INPUT_EVENT_KIND_"),
            "driver input-event kinds must not leak into tairix_input.h"
        );
    }

    /// Completeness guard: every `#[repr(C)]` ABI type (and the
    /// `#[repr(transparent)]` `MacAddress`, which the generator emits as a
    /// struct mirror) has a C `typedef` in the header set, and its in-memory
    /// size/align match the frozen `abi-v1` layout. This is the
    /// type-surface analogue of `errno_table_matches_the_frozen_enum`: a new
    /// `#[repr(C)]` type that escapes the C surface, or a layout change,
    /// fails here. Sizes/aligns are pinned for the host (64-bit) target; the
    /// `uintptr_t`-bearing types (`DirEntry`) are register-width by design.
    #[test]
    fn every_repr_c_abi_type_is_represented_in_the_header_set() {
        use core::mem::{align_of, size_of};
        use tairix_abi::driver::block::{BlockGeometry, DiscardCapability, HealthSnapshot};
        use tairix_abi::driver::bus::BusDevice;
        use tairix_abi::driver::display::{AccelCaps, DisplayMode};
        use tairix_abi::driver::filesystem::{DirEntry, NodeInfo, NodeTimes, VolumeStats};
        use tairix_abi::driver::input::InputEvent;
        use tairix_abi::driver::net::MacAddress;
        use tairix_abi::{
            AppInfoHeader, DriverBindKey, DriverManifest, Duration64, IpcMessageHeader,
            KernelMemoryStats, LoadHeader, ManifestHeader, MountListRequest, MountRecord, PortName,
            ProcessListRequest, ProcessRecord, ProcessStartHeader, ResourceLimit,
            ResourceLimitRecord, StringSlot, SysinfoRequestHeader, SystemIdentity, Time64, Uptime,
        };

        // (header file, typedef-closing line, type, frozen size, frozen align).
        // One entry per public abi-v1 `#[repr(C)]`/`#[repr(transparent)]` POD type;
        // adding a type without an entry here leaves it unrepresented and fails CI.
        #[rustfmt::skip]
        let registry: &[(&str, &str, usize, usize, usize, usize)] = &[
            ("tairix_time.h", "} tairix_time64_t;", size_of::<Time64>(), 16, align_of::<Time64>(), 8),
            ("tairix_time.h", "} tairix_duration64_t;", size_of::<Duration64>(), 16, align_of::<Duration64>(), 8),
            ("tairix_ipc.h", "} tairix_ipc_message_header_t;", size_of::<IpcMessageHeader>(), 32, align_of::<IpcMessageHeader>(), 8),
            ("tairix_ipc.h", "} tairix_port_name_t;", size_of::<PortName>(), 32, align_of::<PortName>(), 1),
            ("tairix_manifest.h", "} tairix_manifest_header_t;", size_of::<ManifestHeader>(), 144, align_of::<ManifestHeader>(), 4),
            ("tairix_appinfo.h", "} tairix_appinfo_header_t;", size_of::<AppInfoHeader>(), 664, align_of::<AppInfoHeader>(), 4),
            ("tairix_rxe.h", "} tairix_load_header_t;", size_of::<LoadHeader>(), 56, align_of::<LoadHeader>(), 8),
            ("tairix_process.h", "} tairix_process_start_header_t;", size_of::<ProcessStartHeader>(), 40, align_of::<ProcessStartHeader>(), 8),
            ("tairix_process.h", "} tairix_string_slot_t;", size_of::<StringSlot>(), 8, align_of::<StringSlot>(), 4),
            ("tairix_sysinfo.h", "} tairix_sysinfo_request_header_t;", size_of::<SysinfoRequestHeader>(), 24, align_of::<SysinfoRequestHeader>(), 8),
            ("tairix_sysinfo.h", "} tairix_process_list_request_t;", size_of::<ProcessListRequest>(), 8, align_of::<ProcessListRequest>(), 4),
            ("tairix_sysinfo.h", "} tairix_process_record_t;", size_of::<ProcessRecord>(), 136, align_of::<ProcessRecord>(), 8),
            ("tairix_sysinfo.h", "} tairix_kernel_memory_stats_t;", size_of::<KernelMemoryStats>(), 40, align_of::<KernelMemoryStats>(), 8),
            ("tairix_sysinfo.h", "} tairix_uptime_t;", size_of::<Uptime>(), 32, align_of::<Uptime>(), 8),
            ("tairix_sysinfo.h", "} tairix_load_average_t;", size_of::<LoadAverage>(), 24, align_of::<LoadAverage>(), 4),
            ("tairix_sysinfo.h", "} tairix_system_identity_t;", size_of::<SystemIdentity>(), 88, align_of::<SystemIdentity>(), 2),
            ("tairix_sysinfo.h", "} tairix_mount_list_request_t;", size_of::<MountListRequest>(), 8, align_of::<MountListRequest>(), 4),
            ("tairix_sysinfo.h", "} tairix_mount_record_t;", size_of::<MountRecord>(), 224, align_of::<MountRecord>(), 8),
            ("tairix_driver.h", "} tairix_volume_stats_t;", size_of::<VolumeStats>(), 48, align_of::<VolumeStats>(), 8),
            ("tairix_driver.h", "} tairix_driver_manifest_t;", size_of::<DriverManifest>(), 140, align_of::<DriverManifest>(), 4),
            ("tairix_driver.h", "} tairix_driver_bind_key_t;", size_of::<DriverBindKey>(), 80, align_of::<DriverBindKey>(), 4),
            ("tairix_driver.h", "} tairix_driver_register_reply_t;", size_of::<DriverRegisterReply>(), 24, align_of::<DriverRegisterReply>(), 8),
            ("tairix_driver.h", "} tairix_block_geometry_t;", size_of::<BlockGeometry>(), 16, align_of::<BlockGeometry>(), 8),
            ("tairix_driver.h", "} tairix_discard_capability_t;", size_of::<DiscardCapability>(), 24, align_of::<DiscardCapability>(), 8),
            ("tairix_driver.h", "} tairix_health_snapshot_t;", size_of::<HealthSnapshot>(), 64, align_of::<HealthSnapshot>(), 8),
            ("tairix_driver.h", "} tairix_bus_device_t;", size_of::<BusDevice>(), 24, align_of::<BusDevice>(), 8),
            ("tairix_driver.h", "} tairix_display_mode_t;", size_of::<DisplayMode>(), 16, align_of::<DisplayMode>(), 4),
            ("tairix_driver.h", "} tairix_accel_caps_t;", size_of::<AccelCaps>(), 16, align_of::<AccelCaps>(), 4),
            ("tairix_driver.h", "} tairix_node_info_t;", size_of::<NodeInfo>(), 88, align_of::<NodeInfo>(), 8),
            ("tairix_driver.h", "} tairix_dir_entry_t;", size_of::<DirEntry>(), 112, align_of::<DirEntry>(), 8),
            ("tairix_driver.h", "} tairix_node_times_t;", size_of::<NodeTimes>(), 64, align_of::<NodeTimes>(), 8),
            ("tairix_driver.h", "} tairix_input_event_t;", size_of::<InputEvent>(), 8, align_of::<InputEvent>(), 4),
            ("tairix_driver.h", "} tairix_mac_address_t;", size_of::<MacAddress>(), 6, align_of::<MacAddress>(), 1),
            ("tairix_rlimit.h", "} tairix_resource_limit_t;", size_of::<ResourceLimit>(), 16, align_of::<ResourceLimit>(), 8),
            ("tairix_sysinfo.h", "} tairix_resource_limit_record_t;", size_of::<ResourceLimitRecord>(), 32, align_of::<ResourceLimitRecord>(), 8),
            ("tairix_sysinfo.h", "} tairix_user_directory_request_t;", size_of::<UserDirectoryRequest>(), 8, align_of::<UserDirectoryRequest>(), 4),
            ("tairix_sysinfo.h", "} tairix_user_directory_record_t;", size_of::<UserDirectoryRecord>(), 40, align_of::<UserDirectoryRecord>(), 4),
        ];
        for &(header, typedef, size, want_size, align, want_align) in registry {
            let h = body(header);
            assert!(
                h.contains(typedef),
                "type `{typedef}` is not represented in {header}"
            );
            assert_eq!(size, want_size, "repr(C) size of `{typedef}`");
            assert_eq!(align, want_align, "repr(C) align of `{typedef}`");
        }
    }

    #[test]
    fn every_syscall_has_a_number_and_a_prototype() {
        let h = body("tairix_syscall.h");
        for spec in SYSCALLS {
            let upper = spec.name.to_ascii_uppercase();
            assert!(
                h.contains(&format!("#define TAIRIX_SYS_{upper} ")),
                "missing number macro for {}",
                spec.name
            );
            assert!(
                h.contains(&format!("tairix_sys_{}(", spec.name)),
                "missing prototype for {}",
                spec.name
            );
        }
    }

    /// The C errno table must mirror the [`Errno`] enum exactly: a dense
    /// `1..=N` numbering with no gaps, ending at the highest discriminant the
    /// enum actually defines. Appending a variant to `Errno` without listing
    /// it here therefore fails this test rather than silently dropping the
    /// code from the C view a third-party developer compiles against.
    #[test]
    fn errno_table_matches_the_frozen_enum() {
        for (idx, (_name, errno)) in ERRNO_NAMES.iter().enumerate() {
            let expected = i32::try_from(idx + 1).expect("small index");
            assert_eq!(errno.as_i32(), expected, "errno values must be dense 1..=N");
            assert_eq!(
                Errno::from_i32(expected),
                Some(*errno),
                "every emitted code must decode back to its own variant"
            );
        }
        let last = ERRNO_NAMES
            .last()
            .map(|(_, e)| e.as_i32())
            .expect("errno table is never empty");
        assert_eq!(
            last,
            i32::try_from(ERRNO_NAMES.len()).expect("small table"),
            "errno table must end at its own length"
        );
        assert!(
            Errno::from_i32(last + 1).is_none(),
            "Errno defines discriminant {} but the C table stops at {last}",
            last + 1
        );
    }

    /// The driver-ABI error table is dense and complete, so a variant added
    /// to `lib/abi` cannot silently miss the C view.
    #[test]
    fn driver_error_table_matches_the_enum() {
        for (idx, (_name, err)) in DRIVER_ERROR_NAMES.iter().enumerate() {
            let expected = i32::try_from(idx + 1).expect("small index");
            assert_eq!(
                err.as_i32(),
                expected,
                "driver-error values must be dense 1..=N"
            );
            assert_eq!(
                DriverError::from_i32(expected),
                Ok(*err),
                "every emitted code must decode back to its own variant"
            );
        }
        let last = i32::try_from(DRIVER_ERROR_NAMES.len()).expect("small table");
        assert!(
            DriverError::from_i32(last + 1).is_err(),
            "DriverError defines discriminant {} but the C table stops at {last}",
            last + 1
        );
    }

    /// The `spawn()` attach-block contract is read from `lib/abi`, never
    /// re-typed: the fixed length, the wire kinds, and the typed block
    /// (`plans/SPAWN.md` SP10).
    #[test]
    fn syscall_header_carries_the_spawn_attach_contract() {
        let h = generate_syscall();
        assert!(
            h.contains(&format!(
                "#define TAIRIX_SPAWN_ATTACH_VERSION {}u",
                tairix_abi::SPAWN_ATTACH_VERSION
            )),
            "spawn attach version: {h}"
        );
        assert!(
            h.contains(&format!(
                "#define TAIRIX_SPAWN_ATTACH_LEN {}u",
                tairix_abi::SPAWN_ATTACH_LEN
            )),
            "spawn attach length: {h}"
        );
        assert!(
            h.contains(&format!(
                "#define TAIRIX_SPAWN_FLAG_SANDBOX {}u",
                tairix_abi::SPAWN_FLAG_SANDBOX
            )),
            "spawn sandbox flag: {h}"
        );
        assert!(
            h.contains("    uint64_t flags;"),
            "spawn attach flags field: {h}"
        );
        for (name, value) in [
            ("INHERIT", tairix_abi::FD_WIRE_KIND_INHERIT),
            ("INHERIT_SLOT", tairix_abi::FD_WIRE_KIND_INHERIT_SLOT),
            ("CLOSED", tairix_abi::FD_WIRE_KIND_CLOSED),
            ("HANDLE", tairix_abi::FD_WIRE_KIND_HANDLE),
        ] {
            assert!(
                h.contains(&format!("#define TAIRIX_FD_WIRE_{name} {value}u")),
                "fd wire kind {name}: {h}"
            );
        }
        assert!(h.contains("} tairix_fd_wire_t;"), "fd wire struct: {h}");
        assert!(
            h.contains("} tairix_spawn_attach_t;"),
            "spawn attach struct: {h}"
        );
    }

    #[test]
    fn name_max_is_respected_by_every_prototype_symbol() {
        // The generator never truncates a name; this guards the assumption
        // that the source-of-truth names already fit SYSCALL_NAME_MAX.
        for spec in SYSCALLS {
            assert!(
                spec.name.len() <= SYSCALL_NAME_MAX,
                "syscall name too long: {}",
                spec.name
            );
        }
    }

    #[test]
    fn committed_headers_are_in_sync() {
        let root = workspace_root();
        let dir = root.join(DEFAULT_INCLUDE_DIR);
        check_sync(&root, &dir).expect("committed headers must match lib/abi");
    }

    #[test]
    fn missing_header_is_an_error() {
        let root = workspace_root();
        let absent = root.join("include").join("__nope__");
        let err = check_sync(&root, &absent).unwrap_err();
        assert!(err.contains("is missing"), "{err}");
    }

    #[test]
    fn stale_header_is_detected() {
        let root = workspace_root();
        // Write a deliberately wrong header set under the workspace scratch
        // area (target/tmp) so a failed test never leaks into /tmp.
        let tmp = root.join("target").join("tmp").join("xtask_c_header_stale");
        std::fs::create_dir_all(&tmp).expect("tmpdir");
        for header in generate_all() {
            std::fs::write(tmp.join(header.file_name), "/* not generated */\n").expect("write");
        }
        let err = check_sync(&root, &tmp).unwrap_err();
        assert!(err.contains("out of date"), "{err}");
    }
}
