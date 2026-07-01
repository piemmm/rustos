//! `cargo xtask c-header` implementation.
//!
//! RustOS is written entirely in Rust, but its kernel/user interface
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
//! `include/rustos/` (`rustos_error.h`, `rustos_capability.h`,
//! `rustos_time.h`, `rustos_syscall.h`, …) plus the umbrella `rustos_abi.h`
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
//! Each syscall is exposed to C as a function named `ros_sys_<name>`
//! (for example `ros_sys_ipc_send`). The names use the short `ros_` /
//! `ROS_` C-ABI prefix and are namespaced and frozen
//! alongside the rest of `abi-v1`. The future user-space stub crate that
//! issues the actual trap implements each one with an explicit
//! `#[export_name = "ros_sys_<name>"]` so the Rust compiler does not
//! mangle it; this header is the contract those exports satisfy.

use std::path::Path;

use rustos_abi::field::{
    TAG_BOOL, TAG_BYTES, TAG_CAP, TAG_DECIMAL, TAG_DURATION, TAG_ERROR, TAG_IP, TAG_LIST, TAG_MAC,
    TAG_NULL, TAG_SIGNED, TAG_STR, TAG_TIME, TAG_UNSIGNED, TAG_UUID,
};
use rustos_abi::{
    AbiType, AppInfoHeader, BufferClass, BundleEntry, CapabilityId, DriverBindKey, DriverError,
    DriverHandle, DriverKind, DriverManifest, DriverRegisterReply, Duration64, Errno,
    HwDeviceClass, HwMatchKey, HwMatchKind, HwNode, HwResource, HwResourceKind, IpcMessageHeader,
    KernelMemoryStats, KeyInput, LibraryScope, LimitKind, LoadHeader, ManifestHeader, MapFlags,
    MountListRequest, MountRecord, NamedKeyCode, NeededLibrary, PointerButtonCode, PointerInput,
    PortName, ProcessListRequest, ProcessRecord, ProcessStartHeader, ProcessState, RandomFlags,
    ResourceLimit, ResourceLimitRecord, RxePermission, Segment, Severity, StdInfoKind, StringSlot,
    SysinfoQueryId, SysinfoRequestHeader, SystemIdentity, Time64, Uptime, ABI_VERSION_V1,
    APPINFO_MAGIC, APPINFO_MAX_CAPABILITIES, APPINFO_MAX_MIME, BUNDLE_ID_MAX, BUNDLE_NAME_MAX,
    BUNDLE_VERSION_MAX, BUTTON_NONE, CAPABILITY_ID_MAX, COARSE_CLOCK_GRANULARITY_NS,
    CONSOLE_INHERIT, DRIVER_MANIFEST_MAGIC, DRIVER_MANIFEST_MAX_BIND_KEYS,
    DRIVER_MANIFEST_MAX_CAPABILITIES, DRIVER_REGISTER_REPLY_MAGIC, DRIVER_REGISTER_STATUS_OK,
    DRIVER_SIGNATURE_LEN, DRIVER_SIGNER_PUBKEY_LEN, ENCODED_QUERY_TABLE_LEN, HOSTNAME_MAX,
    HWTREE_VERSION_V1, HW_COMPATIBLE_MAX, HW_NODE_MAX_MATCH_KEYS, HW_NODE_MAX_RESOURCES,
    HW_NODE_ROOT, IPC_MESSAGE_HEADER_MAGIC, KEY_CLASS_CHAR, KEY_CLASS_NAMED, KEY_INPUT_MAGIC,
    KIND_KEY_PRESSED, KIND_KEY_RELEASED, KIND_MOVED, KIND_PRESSED, KIND_RELEASED, LIBREF_MAX,
    LOAD_FLAG_PIE, LOAD_MAGIC, LOAD_MAX_NEEDED, LOAD_MAX_SEGMENTS, LOG_FIELDS_MAX,
    LOG_FIELD_KEY_MAX, LOG_FIELD_VALUE_MAX, LOG_LEVEL_MAX, LOG_MESSAGE_MAX, LOG_RECORD_HEADER_LEN,
    LOG_RECORD_MAX, MACHINE_ID_LEN, MANIFEST_MAGIC, MANIFEST_MAX_CAPABILITIES, MIME_ENTRY_LEN,
    MIME_TYPE_MAX, MOD_ALT, MOD_CTRL, MOD_MASK, MOD_META, MOD_SHIFT, MOUNT_FSTYPE_MAX,
    MOUNT_SOURCE_MAX, MOUNT_TARGET_MAX, NANOS_PER_SEC, POINTER_INPUT_MAGIC, PORT_NAME_MAX_LEN,
    PROCESS_NAME_MAX, PROCESS_START_MAGIC, PROCESS_START_MAX_STRINGS, PROCESS_START_MAX_STRING_LEN,
    PROCESS_START_MAX_TOTAL_LEN, RANDOM_REQUEST_MAX_BYTES, RANDOM_RESERVE_DEFAULT_BYTES,
    RESOURCE_LIMITS_REPORT_LEN, RLIMIT_INFINITY, RXE_PAGE_SIZE, SEG_FLAG_EXEC, SEG_FLAG_READ,
    SEG_FLAG_WRITE, STDINFO_FD, STDINFO_VERSION_CURRENT, STDINFO_VERSION_V1, SYSCALLS,
    SYSCALL_MAX_ARGS, SYSCALL_TABLE_HASH_LEN, SYSINFO_MAX_PAYLOAD_LEN, SYSINFO_QUERY_NAME_MAX,
    SYSINFO_QUERY_RECORD_LEN, SYSINFO_REQUEST_MAGIC, SYSINFO_VERSION_CURRENT, SYSINFO_VERSION_V1,
    SYSTEM_LIBRARIES_DIR,
};

/// Default on-disk location of the generated C ABI header set, relative to
/// the workspace root. The umbrella header is `rustos_abi.h` inside it.
pub const DEFAULT_INCLUDE_DIR: &str = "include/rustos";

/// The `abi-v1` error codes, paired with the `ROS_E_*` suffix each is
/// emitted under.
///
/// The numeric value of every entry is read straight from the
/// [`Errno`] enum, so this table can never disagree with the frozen
/// discriminants: only the C spelling lives here, because
/// Rust offers no way to enumerate an enum's variants at run time. The
/// in-module `errno_table_matches_the_frozen_enum` test pins the count and
/// the dense `1..=N` numbering so a newly appended `Errno` variant cannot be
/// silently omitted from the header.
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
];

/// One generated C header: its file name (relative to the include directory)
/// and its full text.
pub struct GeneratedHeader {
    /// File name relative to [`DEFAULT_INCLUDE_DIR`], e.g. `rustos_time.h`.
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

/// Render one syscall's C prototype, e.g. `int32_t ros_sys_ipc_send(...)`.
fn prototype(spec: &rustos_abi::SyscallSpec) -> String {
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
    format!("{ret} ros_sys_{}({params});", spec.name)
}

/// Shared `GENERATED FILE` banner for one module header.
///
/// `purpose` is a one-line description of what the header declares.
fn banner(purpose: &str) -> String {
    format!(
        "/*\n\
         * RustOS abi-v1 C development header.\n\
         *\n\
         * GENERATED FILE - DO NOT EDIT BY HAND.\n\
         *\n\
         * {purpose}\n\
         *\n\
         * This is part of the C-language view of the RustOS kernel/user ABI.\n\
         * It is generated from the single source of truth in `lib/abi` by\n\
         * `cargo xtask c-header --write` and verified on every CI run by\n\
         * `cargo xtask c-header`. Edit `lib/abi` and regenerate; never edit\n\
         * this file directly (AGENTS.md sec.2.2, sec.9).\n\
         */\n\n"
    )
}

/// `rustos_error.h` — the stable `abi-v1` error codes.
fn generate_error() -> String {
    use std::fmt::Write as _;
    let mut out = banner("Stable abi-v1 error codes (Errno discriminants).");
    out.push_str("#ifndef ROS_ERROR_H\n#define ROS_ERROR_H\n\n");
    out.push_str("/* Stable abi-v1 error codes (int32_t). */\n");
    for (name, errno) in ERRNO_NAMES {
        let _ = writeln!(out, "#define ROS_E_{name} {}", errno.as_i32());
    }
    out.push_str("\n#endif /* ROS_ERROR_H */\n");
    out
}

/// `rustos_capability.h` — the capability identifiers.
fn generate_capability() -> String {
    use std::fmt::Write as _;
    let mut out = banner("Capability identifiers (AGENTS.md sec.5.2).");
    out.push_str("#ifndef ROS_CAPABILITY_H\n#define ROS_CAPABILITY_H\n\n");
    out.push_str("#include <stdint.h>\n\n");
    out.push_str(
        "/* Capability identifiers (uint16_t, the canonical CapabilityId width;\n   AGENTS.md sec.5.2). Each id carries its type so call sites need no cast. */\n",
    );
    let _ = writeln!(
        out,
        "#define ROS_CAPABILITY_ID_MAX ((uint16_t){CAPABILITY_ID_MAX}u)"
    );
    for raw in 1..=CAPABILITY_ID_MAX {
        if let Some(name) = CapabilityId::from_raw(raw)
            .ok()
            .and_then(CapabilityId::name)
        {
            let _ = writeln!(out, "#define ROS_{name} ((uint16_t){raw}u)");
        }
    }
    out.push_str("\n#endif /* ROS_CAPABILITY_H */\n");
    out
}

/// `rustos_time.h` — the 64-bit-native time types.
///
/// `ros_time64_t` / `ros_duration64_t` mirror the `#[repr(C)]` layout of
/// [`Time64`] / [`Duration64`] (8-byte signed seconds + a 4-byte canonical
/// nanosecond field). Their packed little-endian *wire* size is the separate
/// `*_WIRE_LEN` macro (12 bytes); the in-memory struct is naturally aligned.
fn generate_time() -> String {
    use std::fmt::Write as _;
    let mut out = banner("64-bit-native time types (AGENTS.md sec.21).");
    out.push_str("#ifndef ROS_TIME_H\n#define ROS_TIME_H\n\n");
    out.push_str("#include <stdint.h>\n\n");

    out.push_str("/* Nanoseconds in one second; the sub-second field stays in 0..this. */\n");
    let _ = writeln!(out, "#define ROS_NANOS_PER_SEC {NANOS_PER_SEC}u");
    out.push_str(
        "/* Coarse monotonic-clock granularity, ns, for callers without CAP_TIME_HIRES. */\n",
    );
    let _ = writeln!(
        out,
        "#define ROS_COARSE_CLOCK_GRANULARITY_NS {COARSE_CLOCK_GRANULARITY_NS}ull"
    );
    out.push_str("/* Packed little-endian wire size of each time value, in bytes. */\n");
    let _ = writeln!(out, "#define ROS_TIME64_WIRE_LEN {}u", Time64::WIRE_LEN);
    let _ = writeln!(
        out,
        "#define ROS_DURATION64_WIRE_LEN {}u",
        Duration64::WIRE_LEN
    );
    out.push('\n');

    out.push_str(
        "/* Absolute instant: signed seconds since the Unix epoch + canonical nanos. */\n\
         typedef struct ros_time64 {\n\
         \x20   int64_t secs;\n\
         \x20   uint32_t nanos;\n\
         } ros_time64_t;\n\n",
    );
    out.push_str(
        "/* Span of time: signed seconds + canonical nanos (companion to ros_time64). */\n\
         typedef struct ros_duration64 {\n\
         \x20   int64_t secs;\n\
         \x20   uint32_t nanos;\n\
         } ros_duration64_t;\n\n",
    );

    out.push_str("#endif /* ROS_TIME_H */\n");
    out
}

/// `rustos_random.h` — the canonical random-number ABI.
///
/// Declares the single defined request flag bit (`ROS_RANDOM_FLAG_*`, read
/// from [`RandomFlags`]) and the byte-count limits of a single request. The
/// flag register is a `uint32_t`; the byte counts are register-width
/// quantities (`uintptr_t`), matching the `Len` mapping in [`c_type`].
fn generate_random() -> String {
    use std::fmt::Write as _;
    let mut out = banner("Canonical random-number ABI (AGENTS.md sec.22).");
    out.push_str("#ifndef ROS_RANDOM_H\n#define ROS_RANDOM_H\n\n");
    out.push_str("#include <stdint.h>\n\n");

    out.push_str(
        "/* Request flags (uint32_t). Every undefined bit is reserved and must be zero. */\n",
    );
    let _ = writeln!(
        out,
        "#define ROS_RANDOM_FLAG_NON_BLOCKING {:#x}u",
        RandomFlags::NON_BLOCKING.bits()
    );
    out.push('\n');

    out.push_str("/* Default per-CPU random output reserve, in bytes. */\n");
    let _ = writeln!(
        out,
        "#define ROS_RANDOM_RESERVE_DEFAULT_BYTES ((uintptr_t){RANDOM_RESERVE_DEFAULT_BYTES}u)"
    );
    out.push_str("/* Maximum number of bytes a single random request may ask for. */\n");
    let _ = writeln!(
        out,
        "#define ROS_RANDOM_REQUEST_MAX_BYTES ((uintptr_t){RANDOM_REQUEST_MAX_BYTES}u)"
    );
    out.push('\n');

    out.push_str("#endif /* ROS_RANDOM_H */\n");
    out
}

/// `rustos_log.h` — the `log_emit` diagnostic-record ABI.
///
/// Declares the bounds of a `log_emit` record (the wire image
/// `ros_sys_log_emit` consumes) so a non-Rust program can build one: the
/// highest valid level byte, the message and field-count caps, the per-field
/// key/value caps, the fixed header length, and the maximum encoded size.
/// The byte caps are register-width quantities (`uintptr_t`), matching the
/// `Len` mapping in [`c_type`]; the level cap is a single byte. The wire
/// layout itself is documented inline.
fn generate_log() -> String {
    use std::fmt::Write as _;
    let mut out = banner("log_emit diagnostic-record ABI (AGENTS.md sec.19.4 / sec.20).");
    out.push_str("#ifndef ROS_LOG_H\n#define ROS_LOG_H\n\n");
    out.push_str("#include <stdint.h>\n\n");

    out.push_str(
        "/*\n\
         \x20* Wire layout of a log_emit record (all scalars little-endian):\n\
         \x20*   offset 0: uint8_t  level        (0..=ROS_LOG_LEVEL_MAX)\n\
         \x20*   offset 1: uint8_t  field_count  (<= ROS_LOG_FIELDS_MAX)\n\
         \x20*   offset 2: uint16_t message_len  (<= ROS_LOG_MESSAGE_MAX)\n\
         \x20*   offset 4: uint32_t event_id\n\
         \x20*   offset 8: message bytes (message_len, UTF-8)\n\
         \x20*   then field_count records, each:\n\
         \x20*     uint8_t key_len   (<= ROS_LOG_FIELD_KEY_MAX)\n\
         \x20*     key bytes         (key_len, UTF-8)\n\
         \x20*     a typed field value: a 1-byte ROS_FIELD_TAG_* discriminant\n\
         \x20*       followed by its little-endian payload. The whole encoded\n\
         \x20*       value is <= ROS_LOG_FIELD_VALUE_MAX bytes. Payloads:\n\
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

    out.push_str("/* Highest valid level byte (the Error discriminant). */\n");
    let _ = writeln!(out, "#define ROS_LOG_LEVEL_MAX ((uint8_t){LOG_LEVEL_MAX}u)");
    out.push_str("/* Maximum message length, in bytes. */\n");
    let _ = writeln!(
        out,
        "#define ROS_LOG_MESSAGE_MAX ((uintptr_t){LOG_MESSAGE_MAX}u)"
    );
    out.push_str("/* Maximum number of structured key/value fields. */\n");
    let _ = writeln!(
        out,
        "#define ROS_LOG_FIELDS_MAX ((uintptr_t){LOG_FIELDS_MAX}u)"
    );
    out.push_str("/* Maximum field key length, in bytes. */\n");
    let _ = writeln!(
        out,
        "#define ROS_LOG_FIELD_KEY_MAX ((uintptr_t){LOG_FIELD_KEY_MAX}u)"
    );
    out.push_str("/* Maximum encoded field-value length, in bytes (tag + payload). */\n");
    let _ = writeln!(
        out,
        "#define ROS_LOG_FIELD_VALUE_MAX ((uintptr_t){LOG_FIELD_VALUE_MAX}u)"
    );
    out.push_str("/* Fixed record header length, in bytes. */\n");
    let _ = writeln!(
        out,
        "#define ROS_LOG_RECORD_HEADER_LEN ((uintptr_t){LOG_RECORD_HEADER_LEN}u)"
    );
    out.push_str("/* Maximum encoded record length, in bytes. */\n");
    let _ = writeln!(
        out,
        "#define ROS_LOG_RECORD_MAX ((uintptr_t){LOG_RECORD_MAX}u)"
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
        let _ = writeln!(out, "#define ROS_FIELD_TAG_{name} ((uint8_t){tag}u)");
    }
    out.push('\n');

    out.push_str("#endif /* ROS_LOG_H */\n");
    out
}

/// `rustos_rlimit.h` — the resource-limit ABI.
///
/// Declares the closed [`LimitKind`] discriminants as `ROS_LIMIT_KIND_*`
/// macros, the no-limit sentinel `ROS_RLIMIT_INFINITY`, the wire length, and
/// the `#[repr(C)]` [`ResourceLimit`] pair as a typedef. Every numeric value
/// is read from `lib/abi`; only the C spelling lives here.
fn generate_rlimit() -> String {
    use std::fmt::Write as _;
    let mut out = banner("Resource-limit ABI (AGENTS.md sec.24).");
    out.push_str("#ifndef ROS_RLIMIT_H\n#define ROS_RLIMIT_H\n\n");
    out.push_str("#include <stdint.h>\n\n");

    out.push_str("/* A bound value meaning \"no limit imposed\" (AGENTS.md sec.24.3). */\n");
    let _ = writeln!(
        out,
        "#define ROS_RLIMIT_INFINITY ((uint64_t){RLIMIT_INFINITY}u)"
    );
    out.push('\n');

    out.push_str(
        "/* Resource kinds a ros_resource_limit_t can govern (uint32_t; AGENTS.md sec.24.3). */\n",
    );
    for kind in LimitKind::ALL {
        let raw = kind.as_u32();
        let suffix = kind.name().to_ascii_uppercase().replace('-', "_");
        let _ = writeln!(out, "#define ROS_LIMIT_KIND_{suffix} ((uint32_t){raw}u)");
    }
    let _ = writeln!(
        out,
        "#define ROS_LIMIT_KIND_COUNT ((uint32_t){}u)",
        LimitKind::COUNT
    );
    out.push('\n');

    out.push_str("/* Length, in bytes, of the little-endian ros_resource_limit_t encoding. */\n");
    let _ = writeln!(
        out,
        "#define ROS_RESOURCE_LIMIT_WIRE_LEN {}u",
        ResourceLimit::WIRE_LEN
    );
    out.push('\n');

    out.push_str("/* A soft/hard resource-limit pair (AGENTS.md sec.24.3). */\n");
    out.push_str(
        "typedef struct ros_resource_limit {\n\
         \x20   uint64_t soft;\n\
         \x20   uint64_t hard;\n\
         } ros_resource_limit_t;\n\n",
    );

    out.push_str("#endif /* ROS_RLIMIT_H */\n");
    out
}

/// `rustos_memory.h` — the anonymous-memory `mem_map` flag bits
/// (`plans/SPAWN.md` SP5).
///
/// Declares the single defined `mem_map` flag bit (`ROS_MAP_FLAG_*`, read
/// from [`MapFlags`]). The flag register is a `uint32_t`, matching the `U32`
/// argument the `ros_sys_mem_map` prototype carries.
fn generate_memory() -> String {
    use std::fmt::Write as _;
    let mut out = banner("Anonymous-memory mem_map flag bits (plans/SPAWN.md SP5).");
    out.push_str("#ifndef ROS_MEMORY_H\n#define ROS_MEMORY_H\n\n");
    out.push_str("#include <stdint.h>\n\n");

    out.push_str(
        "/* mem_map flags (uint32_t). Every undefined bit is reserved and must be zero. */\n",
    );
    let _ = writeln!(
        out,
        "#define ROS_MAP_FLAG_FIXED {:#x}u",
        MapFlags::FIXED.bits()
    );
    out.push('\n');

    out.push_str("#endif /* ROS_MEMORY_H */\n");
    out
}

/// `rustos_hwtree.h` — the architecture-neutral hardware tree.
///
/// Declares the hardware-tree version, the root-parent sentinel, the array
/// bounds, the packed little-endian `*_WIRE_LEN` of each record, the
/// closed device-class / match-kind / resource-kind enumerations as
/// `ROS_HW_*` macros, and the `#[repr(C)]` record layouts as typedefs.
/// Every numeric value is read from `lib/abi`; only the C spelling lives
/// here.
fn generate_hwtree() -> String {
    use std::fmt::Write as _;
    let mut out = banner("Architecture-neutral hardware tree (AGENTS.md sec.18.1).");
    out.push_str("#ifndef ROS_HWTREE_H\n#define ROS_HWTREE_H\n\n");
    out.push_str("#include <stdint.h>\n\n");

    out.push_str("/* Hardware-tree ABI version. */\n");
    let _ = writeln!(out, "#define ROS_HWTREE_VERSION {HWTREE_VERSION_V1}u");
    out.push_str("/* Parent id marking a node with no parent (a tree root). */\n");
    let _ = writeln!(out, "#define ROS_HW_NODE_ROOT {HW_NODE_ROOT}u");
    out.push('\n');

    out.push_str("/* Array bounds. */\n");
    let _ = writeln!(
        out,
        "#define ROS_HW_COMPATIBLE_MAX ((uintptr_t){HW_COMPATIBLE_MAX}u)"
    );
    let _ = writeln!(
        out,
        "#define ROS_HW_NODE_MAX_MATCH_KEYS ((uintptr_t){HW_NODE_MAX_MATCH_KEYS}u)"
    );
    let _ = writeln!(
        out,
        "#define ROS_HW_NODE_MAX_RESOURCES ((uintptr_t){HW_NODE_MAX_RESOURCES}u)"
    );
    out.push('\n');

    out.push_str("/* Packed little-endian wire sizes, in bytes. */\n");
    let _ = writeln!(
        out,
        "#define ROS_HW_MATCH_KEY_WIRE_LEN {}u",
        HwMatchKey::WIRE_LEN
    );
    let _ = writeln!(
        out,
        "#define ROS_HW_RESOURCE_WIRE_LEN {}u",
        HwResource::WIRE_LEN
    );
    let _ = writeln!(out, "#define ROS_HW_NODE_WIRE_LEN {}u", HwNode::WIRE_LEN);
    out.push('\n');

    hwtree_enum_macros(&mut out);
    hwtree_structs(&mut out);

    out.push_str("#endif /* ROS_HWTREE_H */\n");
    out
}

/// Emit the closed device-class / match-kind / resource-kind enumerations
/// as `ROS_HW_*` macros, reading every value from `lib/abi`.
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
        ("OTHER", HwDeviceClass::Other),
    ] {
        let _ = writeln!(
            out,
            "#define ROS_HW_CLASS_{name} ((uint16_t){}u)",
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
            "#define ROS_HW_MATCH_{name} ((uint16_t){}u)",
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
    ] {
        let _ = writeln!(
            out,
            "#define ROS_HW_RES_{name} ((uint16_t){}u)",
            kind.as_u16()
        );
    }
    out.push('\n');
}

/// Emit the `#[repr(C)]` hardware-tree record layouts as C typedefs.
fn hwtree_structs(out: &mut String) {
    out.push_str(
        "/* One match key on a node. Mirrors the #[repr(C)] layout; the packed\n\
         * little-endian wire size is ROS_HW_MATCH_KEY_WIRE_LEN. */\n\
         typedef struct ros_hw_match_key {\n\
         \x20   uint16_t kind;\n\
         \x20   uint8_t compatible_len;\n\
         \x20   uint16_t vendor;\n\
         \x20   uint16_t product;\n\
         \x20   uint32_t class_code;\n\
         \x20   uint8_t compatible[ROS_HW_COMPATIBLE_MAX];\n\
         } ros_hw_match_key_t;\n\n",
    );
    out.push_str(
        "/* One resource a node exposes, as a capability-grant request. */\n\
         typedef struct ros_hw_resource {\n\
         \x20   uint16_t kind;\n\
         \x20   uint16_t capability;\n\
         \x20   uint32_t flags;\n\
         \x20   uint64_t base;\n\
         \x20   uint64_t length;\n\
         \x20   uint64_t translated_base;\n\
         } ros_hw_resource_t;\n\n",
    );
    out.push_str(
        "/* One node in the hardware tree. Mirrors the #[repr(C)] layout; the\n\
         * packed little-endian wire size is ROS_HW_NODE_WIRE_LEN. */\n\
         typedef struct ros_hw_node {\n\
         \x20   uint32_t id;\n\
         \x20   uint32_t parent;\n\
         \x20   uint16_t device_class;\n\
         \x20   uint8_t match_key_count;\n\
         \x20   uint8_t resource_count;\n\
         \x20   ros_hw_match_key_t match_keys[ROS_HW_NODE_MAX_MATCH_KEYS];\n\
         \x20   ros_hw_resource_t resources[ROS_HW_NODE_MAX_RESOURCES];\n\
         } ros_hw_node_t;\n\n",
    );
}

/// `rustos_ipc.h` — the IPC message header and port-name wire types.
///
/// `ros_ipc_message_header_t` mirrors the `#[repr(C)]` layout of
/// [`IpcMessageHeader`] and `ros_port_name_t` that of [`PortName`]; each is
/// naturally aligned. Their packed little-endian *wire* size is the separate
/// `*_WIRE_LEN` macro. Every numeric value is read from `lib/abi`, never
/// re-typed; only the C spelling lives here.
fn generate_ipc() -> String {
    use std::fmt::Write as _;
    let mut out = banner("IPC message header and port-name wire types (AGENTS.md sec.4).");
    out.push_str("#ifndef ROS_IPC_H\n#define ROS_IPC_H\n\n");
    out.push_str("#include <stdint.h>\n\n");

    out.push_str("/* Magic word identifying an abi-v1 IPC message (\"IPC1\" little-endian). */\n");
    let _ = writeln!(
        out,
        "#define ROS_IPC_MESSAGE_HEADER_MAGIC {IPC_MESSAGE_HEADER_MAGIC:#x}u"
    );
    out.push_str("/* Maximum payload length, in bytes, an IPC message header may advertise. */\n");
    let _ = writeln!(
        out,
        "#define ROS_IPC_MESSAGE_MAX_PAYLOAD_LEN {}u",
        rustos_abi::ipc::IPC_MESSAGE_MAX_PAYLOAD_LEN
    );
    out.push_str("/* Packed little-endian wire size of an IPC message header, in bytes. */\n");
    let _ = writeln!(
        out,
        "#define ROS_IPC_MESSAGE_HEADER_WIRE_LEN {}u",
        IpcMessageHeader::WIRE_LEN
    );
    out.push('\n');

    out.push_str("/* Maximum length, in bytes, of a port name (excludes the length byte). */\n");
    let _ = writeln!(out, "#define ROS_PORT_NAME_MAX_LEN {PORT_NAME_MAX_LEN}u");
    out.push_str("/* Packed little-endian wire size of a port name, in bytes. */\n");
    let _ = writeln!(
        out,
        "#define ROS_PORT_NAME_WIRE_LEN {}u",
        PortName::WIRE_LEN
    );
    out.push('\n');

    out.push_str(
        "/* IPC message header: prefixes every message; encoded little-endian on the wire. */\n\
         typedef struct ros_ipc_message_header {\n\
         \x20   uint32_t magic;\n\
         \x20   uint16_t version;\n\
         \x20   uint16_t flags;\n\
         \x20   uint64_t endpoint;\n\
         \x20   uint64_t sender;\n\
         \x20   uint32_t payload_len;\n\
         \x20   uint32_t reserved;\n\
         } ros_ipc_message_header_t;\n\n",
    );
    out.push_str(
        "/* Validated well-known IPC port name: NUL-padded name bytes + a length byte. */\n\
         typedef struct ros_port_name {\n\
         \x20   uint8_t bytes[ROS_PORT_NAME_MAX_LEN];\n\
         \x20   uint8_t len;\n\
         } ros_port_name_t;\n\n",
    );

    out.push_str("#endif /* ROS_IPC_H */\n");
    out
}

/// `rustos_stdinfo.h` — the Standard Information Stream ABI.
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
    out.push_str("#ifndef ROS_STDINFO_H\n#define ROS_STDINFO_H\n\n");
    out.push_str("#include <stdint.h>\n\n");

    out.push_str("/* Reserved stdinfo file descriptor; no component may repurpose it. */\n");
    let _ = writeln!(out, "#define ROS_STDINFO_FD {STDINFO_FD}u");
    out.push_str("/* stdinfo framing version tag for the frozen v1 framing. */\n");
    let _ = writeln!(out, "#define ROS_STDINFO_VERSION_V1 {STDINFO_VERSION_V1}u");
    out.push_str("/* stdinfo framing version this header set describes. */\n");
    let _ = writeln!(
        out,
        "#define ROS_STDINFO_VERSION_CURRENT {STDINFO_VERSION_CURRENT}u"
    );
    out.push('\n');

    out.push_str(
        "/* Closed set of record kinds (uint8_t). Wire spelling is the string in parens. */\n",
    );
    let _ = writeln!(
        out,
        "#define ROS_STDINFO_KIND_OMISSION ((uint8_t){}u) /* \"{}\" */",
        StdInfoKind::Omission as u8,
        StdInfoKind::Omission.as_str()
    );
    let _ = writeln!(
        out,
        "#define ROS_STDINFO_KIND_SUMMARY ((uint8_t){}u) /* \"{}\" */",
        StdInfoKind::Summary as u8,
        StdInfoKind::Summary.as_str()
    );
    let _ = writeln!(
        out,
        "#define ROS_STDINFO_KIND_SCHEMA ((uint8_t){}u) /* \"{}\" */",
        StdInfoKind::Schema as u8,
        StdInfoKind::Schema.as_str()
    );
    let _ = writeln!(
        out,
        "#define ROS_STDINFO_KIND_SUGGESTION ((uint8_t){}u) /* \"{}\" */",
        StdInfoKind::Suggestion as u8,
        StdInfoKind::Suggestion.as_str()
    );
    let _ = writeln!(
        out,
        "#define ROS_STDINFO_KIND_CONTEXT ((uint8_t){}u) /* \"{}\" */",
        StdInfoKind::Context as u8,
        StdInfoKind::Context.as_str()
    );
    out.push('\n');

    out.push_str("/* Advisory severity (uint8_t). Security events use lib/log, not fd 3. */\n");
    let _ = writeln!(
        out,
        "#define ROS_STDINFO_SEVERITY_INFO ((uint8_t){}u) /* \"{}\" */",
        Severity::Info as u8,
        Severity::Info.as_str()
    );
    let _ = writeln!(
        out,
        "#define ROS_STDINFO_SEVERITY_DEBUG ((uint8_t){}u) /* \"{}\" */",
        Severity::Debug as u8,
        Severity::Debug.as_str()
    );
    out.push('\n');

    out.push_str("#endif /* ROS_STDINFO_H */\n");
    out
}

/// `rustos_manifest.h` — the signed `rxe` manifest header.
///
/// `ros_manifest_header_t` mirrors the `#[repr(C)]` layout of
/// [`ManifestHeader`]: the fixed-size prefix of the signed manifest section of
/// an `rxe` binary. Its packed little-endian *wire* size is the separate
/// `ROS_MANIFEST_HEADER_WIRE_LEN` macro (equal to the struct size here, as the
/// layout has no trailing padding). Every numeric value is read from
/// `lib/abi`, never re-typed; only the C spelling lives here.
fn generate_manifest() -> String {
    use std::fmt::Write as _;
    let mut out = banner("Signed rxe manifest header (AGENTS.md sec.9).");
    out.push_str("#ifndef ROS_MANIFEST_H\n#define ROS_MANIFEST_H\n\n");
    out.push_str("#include <stdint.h>\n\n");

    out.push_str("/* Magic word identifying an abi-v1 manifest (\"RXM1\" little-endian). */\n");
    let _ = writeln!(out, "#define ROS_MANIFEST_MAGIC {MANIFEST_MAGIC:#x}u");
    out.push_str("/* Maximum number of capability identifiers a manifest may request. */\n");
    let _ = writeln!(
        out,
        "#define ROS_MANIFEST_MAX_CAPABILITIES {MANIFEST_MAX_CAPABILITIES}u"
    );
    out.push_str("/* Length, in bytes, of the linked syscall-table hash (SHA-256). */\n");
    let _ = writeln!(
        out,
        "#define ROS_SYSCALL_TABLE_HASH_LEN {SYSCALL_TABLE_HASH_LEN}u"
    );
    out.push_str("/* Packed little-endian wire size of a manifest header, in bytes. */\n");
    let _ = writeln!(
        out,
        "#define ROS_MANIFEST_HEADER_WIRE_LEN {}u",
        ManifestHeader::WIRE_LEN
    );
    out.push('\n');

    out.push_str(
        "/* Signed rxe manifest prefix; encoded little-endian on the wire. */\n\
         typedef struct ros_manifest_header {\n\
         \x20   uint32_t magic;\n\
         \x20   uint32_t abi_version;\n\
         \x20   uint32_t flags;\n\
         \x20   uint16_t capability_count;\n\
         \x20   uint16_t reserved0;\n\
         \x20   uint8_t syscall_table_hash[ROS_SYSCALL_TABLE_HASH_LEN];\n\
         \x20   uint8_t signer_pubkey[32];\n\
         \x20   uint8_t signature[64];\n\
         } ros_manifest_header_t;\n\n",
    );

    out.push_str("#endif /* ROS_MANIFEST_H */\n");
    out
}

/// `rustos_input.h` — the desktop input event ABI.
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
    out.push_str("#ifndef ROS_INPUT_H\n#define ROS_INPUT_H\n\n");
    out.push_str("#include <stdint.h>\n\n");

    // Record magics ("PIN1" / "KIN1") and their packed little-endian wire sizes.
    let _ = writeln!(
        out,
        "#define ROS_POINTER_INPUT_MAGIC {POINTER_INPUT_MAGIC:#x}u"
    );
    let _ = writeln!(out, "#define ROS_KEY_INPUT_MAGIC {KEY_INPUT_MAGIC:#x}u");
    let pwl = PointerInput::WIRE_LEN;
    let kwl = KeyInput::WIRE_LEN;
    let _ = writeln!(out, "#define ROS_POINTER_INPUT_WIRE_LEN {pwl}u");
    let _ = writeln!(out, "#define ROS_KEY_INPUT_WIRE_LEN {kwl}u");
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
        "Record `kind` codes: pointer moves/clicks then key down/up (uint16_t).",
        &[
            ("ROS_INPUT_KIND_MOVED", KIND_MOVED),
            ("ROS_INPUT_KIND_PRESSED", KIND_PRESSED),
            ("ROS_INPUT_KIND_RELEASED", KIND_RELEASED),
            ("ROS_INPUT_KIND_KEY_PRESSED", KIND_KEY_PRESSED),
            ("ROS_INPUT_KIND_KEY_RELEASED", KIND_KEY_RELEASED),
        ],
        false,
    );
    emit(
        "`button` (motion=none, else a button) and keyboard `key_class` codes (uint16_t).",
        &[
            ("ROS_INPUT_BUTTON_NONE", BUTTON_NONE),
            (
                "ROS_POINTER_BUTTON_PRIMARY",
                PointerButtonCode::Primary.code(),
            ),
            (
                "ROS_POINTER_BUTTON_SECONDARY",
                PointerButtonCode::Secondary.code(),
            ),
            (
                "ROS_POINTER_BUTTON_MIDDLE",
                PointerButtonCode::Middle.code(),
            ),
            ("ROS_KEY_CLASS_CHAR", KEY_CLASS_CHAR),
            ("ROS_KEY_CLASS_NAMED", KEY_CLASS_NAMED),
        ],
        false,
    );
    emit(
        "Modifier bits held while a key event was produced (uint16_t).",
        &[
            ("ROS_MOD_SHIFT", MOD_SHIFT),
            ("ROS_MOD_CTRL", MOD_CTRL),
            ("ROS_MOD_ALT", MOD_ALT),
            ("ROS_MOD_META", MOD_META),
            ("ROS_MOD_MASK", MOD_MASK),
        ],
        true,
    );
    emit(
        "Named non-character key codes carried in a record's `named` field (uint16_t).",
        &NAMED_KEY_CODES,
        false,
    );

    out.push_str("#endif /* ROS_INPUT_H */\n");
    out
}

/// The C spelling of each [`NamedKeyCode`] variant paired with its frozen wire
/// code, read from `lib/abi` via [`NamedKeyCode::code`]. Only the C *name*
/// lives here (Rust offers no variant-name reflection); the numeric value is
/// the source of truth and is pinned by the in-module test.
const NAMED_KEY_CODES: [(&str, u16); 26] = [
    ("ROS_KEY_ENTER", NamedKeyCode::Enter.code()),
    ("ROS_KEY_ESCAPE", NamedKeyCode::Escape.code()),
    ("ROS_KEY_BACKSPACE", NamedKeyCode::Backspace.code()),
    ("ROS_KEY_TAB", NamedKeyCode::Tab.code()),
    ("ROS_KEY_DELETE", NamedKeyCode::Delete.code()),
    ("ROS_KEY_INSERT", NamedKeyCode::Insert.code()),
    ("ROS_KEY_HOME", NamedKeyCode::Home.code()),
    ("ROS_KEY_END", NamedKeyCode::End.code()),
    ("ROS_KEY_PAGE_UP", NamedKeyCode::PageUp.code()),
    ("ROS_KEY_PAGE_DOWN", NamedKeyCode::PageDown.code()),
    ("ROS_KEY_LEFT", NamedKeyCode::Left.code()),
    ("ROS_KEY_RIGHT", NamedKeyCode::Right.code()),
    ("ROS_KEY_UP", NamedKeyCode::Up.code()),
    ("ROS_KEY_DOWN", NamedKeyCode::Down.code()),
    ("ROS_KEY_F1", NamedKeyCode::F1.code()),
    ("ROS_KEY_F2", NamedKeyCode::F2.code()),
    ("ROS_KEY_F3", NamedKeyCode::F3.code()),
    ("ROS_KEY_F4", NamedKeyCode::F4.code()),
    ("ROS_KEY_F5", NamedKeyCode::F5.code()),
    ("ROS_KEY_F6", NamedKeyCode::F6.code()),
    ("ROS_KEY_F7", NamedKeyCode::F7.code()),
    ("ROS_KEY_F8", NamedKeyCode::F8.code()),
    ("ROS_KEY_F9", NamedKeyCode::F9.code()),
    ("ROS_KEY_F10", NamedKeyCode::F10.code()),
    ("ROS_KEY_F11", NamedKeyCode::F11.code()),
    ("ROS_KEY_F12", NamedKeyCode::F12.code()),
];

/// `rustos_appinfo.h` — the application-bundle manifest ABI.
///
/// `ros_appinfo_header_t` mirrors the `#[repr(C)]` layout of [`AppInfoHeader`]
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
    out.push_str("#ifndef ROS_APPINFO_H\n#define ROS_APPINFO_H\n\n");
    out.push_str("#include <stdint.h>\n\n");

    out.push_str(
        "/* Magic word identifying an abi-v1 AppInfo manifest (\"RAI1\" little-endian). */\n",
    );
    let _ = writeln!(out, "#define ROS_APPINFO_MAGIC {APPINFO_MAGIC:#x}u");
    out.push_str("/* Maximum number of capability identifiers a manifest may request. */\n");
    let _ = writeln!(
        out,
        "#define ROS_APPINFO_MAX_CAPABILITIES {APPINFO_MAX_CAPABILITIES}u"
    );
    out.push_str("/* Maximum number of MIME / file-type associations a bundle may declare. */\n");
    let _ = writeln!(out, "#define ROS_APPINFO_MAX_MIME {APPINFO_MAX_MIME}u");
    out.push_str("/* Maximum length, in bytes, of a bundle identifier. */\n");
    let _ = writeln!(out, "#define ROS_BUNDLE_ID_MAX {BUNDLE_ID_MAX}u");
    out.push_str("/* Maximum length, in bytes, of a bundle's human-readable name. */\n");
    let _ = writeln!(out, "#define ROS_BUNDLE_NAME_MAX {BUNDLE_NAME_MAX}u");
    out.push_str("/* Maximum length, in bytes, of a bundle version string. */\n");
    let _ = writeln!(out, "#define ROS_BUNDLE_VERSION_MAX {BUNDLE_VERSION_MAX}u");
    out.push_str("/* Maximum length, in bytes, of one declared MIME-type string. */\n");
    let _ = writeln!(out, "#define ROS_MIME_TYPE_MAX {MIME_TYPE_MAX}u");
    out.push_str("/* Encoded length of one MIME-type body entry (length byte + buffer). */\n");
    let _ = writeln!(out, "#define ROS_MIME_ENTRY_LEN {MIME_ENTRY_LEN}u");
    out.push_str("/* Packed little-endian wire size of an AppInfo header, in bytes. */\n");
    let _ = writeln!(
        out,
        "#define ROS_APPINFO_HEADER_WIRE_LEN {}u",
        AppInfoHeader::WIRE_LEN
    );
    out.push('\n');

    out.push_str("/* Curated, OS-provided shared-library directory (AGENTS.md sec.16.4). */\n");
    let _ = writeln!(
        out,
        "#define ROS_SYSTEM_LIBRARIES_DIR \"{SYSTEM_LIBRARIES_DIR}\""
    );
    out.push('\n');

    out.push_str(
        "/* Fixed set of names permitted at a bundle's top level (AGENTS.md sec.16.5). */\n",
    );
    for entry in BundleEntry::ALL {
        let _ = writeln!(
            out,
            "#define ROS_BUNDLE_ENTRY_{} \"{}\"",
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
        "#define ROS_LIBRARY_SCOPE_BUNDLE ((uint8_t){}u)",
        LibraryScope::Bundle as u8
    );
    let _ = writeln!(
        out,
        "#define ROS_LIBRARY_SCOPE_SYSTEM ((uint8_t){}u)",
        LibraryScope::System as u8
    );
    out.push('\n');

    let _ = writeln!(
        out,
        "/* Signed AppInfo manifest prefix; encoded little-endian on the wire. */\n\
         typedef struct ros_appinfo_header {{\n\
         \x20   uint32_t magic;\n\
         \x20   uint32_t abi_version;\n\
         \x20   uint32_t flags;\n\
         \x20   uint16_t capability_count;\n\
         \x20   uint16_t mime_count;\n\
         \x20   uint8_t id_len;\n\
         \x20   uint8_t name_len;\n\
         \x20   uint8_t version_len;\n\
         \x20   uint8_t reserved0;\n\
         \x20   uint8_t id[ROS_BUNDLE_ID_MAX];\n\
         \x20   uint8_t name[ROS_BUNDLE_NAME_MAX];\n\
         \x20   uint8_t version[ROS_BUNDLE_VERSION_MAX];\n\
         \x20   uint8_t syscall_table_hash[{SYSCALL_TABLE_HASH_LEN}];\n\
         \x20   uint8_t content_hash[32];\n\
         \x20   uint8_t signer_pubkey[32];\n\
         \x20   uint8_t signature[64];\n\
         }} ros_appinfo_header_t;\n",
    );

    out.push_str("#endif /* ROS_APPINFO_H */\n");
    out
}

/// `rustos_rxe.h` — the `rxe` load-image table and load-time hardening
/// policy.
///
/// `ros_load_header_t` mirrors the `#[repr(C)]` layout of [`LoadHeader`] (the
/// fixed image prefix; naturally aligned, so the struct size equals the wire
/// size). A [`Segment`] record is hand-serialised, so the header exports its
/// packed wire size (`ROS_SEGMENT_WIRE_LEN`) and the `ROS_SEG_FLAG_*` field
/// codes rather than a C struct mirror. Alongside them the header declares the
/// `ROS_LOAD_MAGIC` / `ROS_RXE_PAGE_SIZE` / `ROS_LOAD_MAX_SEGMENTS` /
/// `ROS_LOAD_FLAG_PIE` constants and the [`RxePermission`] discriminants.
/// Every numeric value and discriminant is read from `lib/abi`, never
/// re-typed; only the C spelling lives here.
fn generate_rxe() -> String {
    use std::fmt::Write as _;
    let mut out =
        banner("rxe load-image table and load-time hardening (AGENTS.md sec.9, sec.19.2).");
    out.push_str("#ifndef ROS_RXE_H\n#define ROS_RXE_H\n\n");
    out.push_str("#include <stdint.h>\n\n");

    out.push_str("/* Magic word identifying an abi-v1 load header (\"RXEL\" little-endian). */\n");
    let _ = writeln!(out, "#define ROS_LOAD_MAGIC {LOAD_MAGIC:#x}u");
    out.push_str("/* Page size the load image is expressed in, in bytes. */\n");
    let _ = writeln!(
        out,
        "#define ROS_RXE_PAGE_SIZE ((uint64_t){RXE_PAGE_SIZE}ull)"
    );
    out.push_str("/* Maximum number of segment records a single load image may carry. */\n");
    let _ = writeln!(
        out,
        "#define ROS_LOAD_MAX_SEGMENTS ((uintptr_t){LOAD_MAX_SEGMENTS}u)"
    );
    out.push_str("/* Maximum number of needed-library references a load image may declare. */\n");
    let _ = writeln!(
        out,
        "#define ROS_LOAD_MAX_NEEDED ((uintptr_t){LOAD_MAX_NEEDED}u)"
    );
    out.push_str("/* Maximum length, in bytes, of a needed-library reference path. */\n");
    let _ = writeln!(out, "#define ROS_LIBREF_MAX ((uintptr_t){LIBREF_MAX}u)");
    out.push('\n');

    out.push_str("/* Load-header flag bits (uint32_t). Every undefined bit must be zero. */\n");
    out.push_str("/* The image is position-independent (PIE); required by sec.19.2. */\n");
    let _ = writeln!(out, "#define ROS_LOAD_FLAG_PIE {LOAD_FLAG_PIE:#x}u");
    out.push('\n');

    out.push_str("/* Segment flag bits (uint32_t) in a packed segment record. */\n");
    let _ = writeln!(out, "#define ROS_SEG_FLAG_READ {SEG_FLAG_READ:#x}u");
    let _ = writeln!(out, "#define ROS_SEG_FLAG_WRITE {SEG_FLAG_WRITE:#x}u");
    let _ = writeln!(out, "#define ROS_SEG_FLAG_EXEC {SEG_FLAG_EXEC:#x}u");
    out.push('\n');

    out.push_str("/* Packed little-endian wire size of a load header, in bytes. */\n");
    let _ = writeln!(
        out,
        "#define ROS_LOAD_HEADER_WIRE_LEN {}u",
        LoadHeader::WIRE_LEN
    );
    out.push_str("/* Packed little-endian wire size of one segment record, in bytes. */\n");
    let _ = writeln!(out, "#define ROS_SEGMENT_WIRE_LEN {}u", Segment::WIRE_LEN);
    out.push_str("/* Packed little-endian wire size of one needed-library record, in bytes. */\n");
    let _ = writeln!(
        out,
        "#define ROS_NEEDED_LIBRARY_WIRE_LEN {}u",
        NeededLibrary::WIRE_LEN
    );
    out.push('\n');

    out.push_str("/* W^X-clean permission a segment is mapped with (uint8_t). */\n");
    let _ = writeln!(
        out,
        "#define ROS_RXE_PERMISSION_READ_ONLY ((uint8_t){}u)",
        RxePermission::ReadOnly as u8
    );
    let _ = writeln!(
        out,
        "#define ROS_RXE_PERMISSION_READ_EXECUTE ((uint8_t){}u)",
        RxePermission::ReadExecute as u8
    );
    let _ = writeln!(
        out,
        "#define ROS_RXE_PERMISSION_READ_WRITE ((uint8_t){}u)",
        RxePermission::ReadWrite as u8
    );
    out.push('\n');

    out.push_str(
        "/* Fixed rxe load-image prefix; encoded little-endian on the wire. */\n\
         typedef struct ros_load_header {\n\
         \x20   uint32_t magic;\n\
         \x20   uint32_t abi_version;\n\
         \x20   uint32_t flags;\n\
         \x20   uint16_t segment_count;\n\
         \x20   uint16_t needed_count;\n\
         \x20   uint64_t entry;\n",
    );
    let _ = writeln!(out, "\x20   uint8_t cfi_tag[{SYSCALL_TABLE_HASH_LEN}];");
    out.push_str("} ros_load_header_t;\n\n");

    out.push_str("#endif /* ROS_RXE_H */\n");
    out
}

/// `rustos_process.h` — the process startup vector the kernel hands a freshly
/// spawned program (`plans/CCOMPAT.md` CC3).
///
/// `ros_process_start_header_t` mirrors the `#[repr(C)]` layout of
/// [`ProcessStartHeader`] (the fixed block prefix; naturally aligned, so the
/// struct size equals the wire size) and `ros_string_slot_t` that of
/// [`StringSlot`] (one `(offset, len)` reference into the block's string
/// region). Alongside them the header declares the `ROS_PROCESS_START_MAGIC`
/// magic, the `ROS_PROCESS_START_MAX_*` limits, and the packed `*_WIRE_LEN`
/// sizes. Every numeric value is read from `lib/abi`, never re-typed; only the
/// C spelling lives here.
fn generate_process() -> String {
    use std::fmt::Write as _;
    let mut out = banner(
        "Process startup vector handed to a freshly spawned program \
         (AGENTS.md sec.16.5; plans/CCOMPAT.md CC3).",
    );
    out.push_str("#ifndef ROS_PROCESS_H\n#define ROS_PROCESS_H\n\n");
    out.push_str("#include <stdint.h>\n\n");

    out.push_str(
        "/* Magic word identifying an abi-v1 startup-vector block (\"PSV1\" little-endian). */\n",
    );
    let _ = writeln!(
        out,
        "#define ROS_PROCESS_START_MAGIC {PROCESS_START_MAGIC:#x}u"
    );
    out.push_str(
        "/* Maximum number of strings (arguments + environment entries) a vector may carry. */\n",
    );
    let _ = writeln!(
        out,
        "#define ROS_PROCESS_START_MAX_STRINGS {PROCESS_START_MAX_STRINGS}u"
    );
    out.push_str("/* Maximum length, in bytes, of one argument or environment string. */\n");
    let _ = writeln!(
        out,
        "#define ROS_PROCESS_START_MAX_STRING_LEN {PROCESS_START_MAX_STRING_LEN}u"
    );
    out.push_str("/* Maximum total size, in bytes, of a startup-vector block. */\n");
    let _ = writeln!(
        out,
        "#define ROS_PROCESS_START_MAX_TOTAL_LEN ((uint64_t){PROCESS_START_MAX_TOTAL_LEN}ull)"
    );
    out.push('\n');

    out.push_str(
        "/* `console` argument to ros_sys_spawn: attach the child to the caller's own\n\
         \x20* console (any other value names an installed console index, see\n\
         \x20* ros_sys_console_count). */\n",
    );
    let _ = writeln!(
        out,
        "#define ROS_CONSOLE_INHERIT ((uint64_t){CONSOLE_INHERIT:#x}ull)"
    );
    out.push('\n');

    out.push_str("/* Packed little-endian wire size of a startup-vector header, in bytes. */\n");
    let _ = writeln!(
        out,
        "#define ROS_PROCESS_START_HEADER_WIRE_LEN {}u",
        ProcessStartHeader::WIRE_LEN
    );
    out.push_str("/* Packed little-endian wire size of one string slot, in bytes. */\n");
    let _ = writeln!(
        out,
        "#define ROS_STRING_SLOT_WIRE_LEN {}u",
        StringSlot::WIRE_LEN
    );
    out.push('\n');

    out.push_str(
        "/* One string's (offset, len) reference into the block; encoded little-endian. */\n\
         typedef struct ros_string_slot {\n\
         \x20   uint32_t offset;\n\
         \x20   uint32_t len;\n\
         } ros_string_slot_t;\n\n",
    );
    out.push_str(
        "/* Fixed startup-vector block prefix; followed by the slot table then string data. */\n\
         typedef struct ros_process_start_header {\n\
         \x20   uint32_t magic;\n\
         \x20   uint32_t abi_version;\n\
         \x20   uint32_t arg_count;\n\
         \x20   uint32_t env_count;\n\
         \x20   uint64_t total_len;\n\
         \x20   uint64_t canary;\n\
         } ros_process_start_header_t;\n\n",
    );

    out.push_str("#endif /* ROS_PROCESS_H */\n");
    out
}

/// `rustos_sysinfo.h` — the System Information API surface.
///
/// Declares the `sysinfo-v1` framing (`ROS_SYSINFO_VERSION_*` /
/// `ROS_SYSINFO_REQUEST_MAGIC` / `ROS_SYSINFO_MAX_PAYLOAD_LEN`), the
/// [`SysinfoQueryId`] well-known identifiers and their `ROS_SYSINFO_QUERY_ID_MAX`
/// ceiling, the canonical registry-encoding constants
/// (`ROS_SYSINFO_QUERY_NAME_MAX` / `_RECORD_LEN` / `_ENCODED_QUERY_TABLE_LEN`),
/// the [`ProcessState`] `#[repr(u8)]` discriminants, the inline-buffer size
/// limits (`ROS_PROCESS_NAME_MAX`, `ROS_MACHINE_ID_LEN`, `ROS_HOSTNAME_MAX`,
/// `ROS_MOUNT_*_MAX`), and a `#[repr(C)]` C struct mirror plus a packed
/// `*_WIRE_LEN` macro for each of the nine wire types
/// ([`SysinfoRequestHeader`], [`ProcessListRequest`], [`ProcessRecord`],
/// [`KernelMemoryStats`], [`Uptime`], [`SystemIdentity`], [`MountListRequest`],
/// [`MountRecord`], [`ResourceLimitRecord`]). [`Uptime`]'s members are the
/// `ros_duration64_t` / `ros_time64_t` types from `rustos_time.h`; a
/// [`ResourceLimitRecord`]'s `limit` is the `ros_resource_limit_t` from
/// `rustos_rlimit.h`. Every numeric value and
/// discriminant is read from `lib/abi`, never re-typed; only the C spelling
/// lives here.
fn generate_sysinfo() -> String {
    let mut out = banner("System Information API surface (AGENTS.md sec.16.6).");
    out.push_str("#ifndef ROS_SYSINFO_H\n#define ROS_SYSINFO_H\n\n");
    out.push_str("#include <stdint.h>\n");
    out.push_str("#include \"rustos_time.h\"\n");
    out.push_str("#include \"rustos_rlimit.h\"\n\n");

    sysinfo_emit_framing(&mut out);
    sysinfo_emit_record_sizes(&mut out);
    out.push_str(SYSINFO_RECORD_TYPEDEFS);
    out.push_str("#endif /* ROS_SYSINFO_H */\n");
    out
}

/// Emit the sysinfo framing, registry-encoding, query-id, and process-state
/// constants (every value read from `lib/abi`).
fn sysinfo_emit_framing(out: &mut String) {
    use std::fmt::Write as _;
    out.push_str("/* sysinfo protocol version tag for the frozen v1 surface. */\n");
    let _ = writeln!(out, "#define ROS_SYSINFO_VERSION_V1 {SYSINFO_VERSION_V1}u");
    out.push_str("/* sysinfo protocol version this header set describes. */\n");
    let _ = writeln!(
        out,
        "#define ROS_SYSINFO_VERSION_CURRENT {SYSINFO_VERSION_CURRENT}u"
    );
    out.push_str("/* Magic word identifying a sysinfo-v1 request (\"SYI1\" little-endian). */\n");
    let _ = writeln!(
        out,
        "#define ROS_SYSINFO_REQUEST_MAGIC {SYSINFO_REQUEST_MAGIC:#x}u"
    );
    out.push_str(
        "/* Maximum request/response payload length, in bytes, a header may advertise. */\n",
    );
    let _ = writeln!(
        out,
        "#define ROS_SYSINFO_MAX_PAYLOAD_LEN {SYSINFO_MAX_PAYLOAD_LEN}u"
    );
    out.push_str("/* Inclusive upper bound on the sysinfo-v1 query identifier space. */\n");
    let _ = writeln!(
        out,
        "#define ROS_SYSINFO_QUERY_ID_MAX {}u",
        SysinfoQueryId::MAX
    );
    out.push('\n');

    out.push_str(
        "/* Canonical query-registry encoding constants (the hashable registry image). */\n",
    );
    let _ = writeln!(
        out,
        "#define ROS_SYSINFO_QUERY_NAME_MAX {SYSINFO_QUERY_NAME_MAX}u"
    );
    let _ = writeln!(
        out,
        "#define ROS_SYSINFO_QUERY_RECORD_LEN {SYSINFO_QUERY_RECORD_LEN}u"
    );
    let _ = writeln!(
        out,
        "#define ROS_SYSINFO_ENCODED_QUERY_TABLE_LEN {ENCODED_QUERY_TABLE_LEN}u"
    );
    out.push('\n');

    out.push_str("/* Well-known sysinfo-v1 query identifiers (uint16_t). Do not renumber. */\n");
    let query_ids = [
        (
            "ROS_SYSINFO_QUERY_SELF_PROCESS_LIST",
            SysinfoQueryId::SELF_PROCESS_LIST,
        ),
        (
            "ROS_SYSINFO_QUERY_GLOBAL_PROCESS_LIST",
            SysinfoQueryId::GLOBAL_PROCESS_LIST,
        ),
        (
            "ROS_SYSINFO_QUERY_KERNEL_MEMORY_STATS",
            SysinfoQueryId::KERNEL_MEMORY_STATS,
        ),
        (
            "ROS_SYSINFO_QUERY_HARDWARE_TREE",
            SysinfoQueryId::HARDWARE_TREE,
        ),
        (
            "ROS_SYSINFO_QUERY_SYSTEM_IDENTITY",
            SysinfoQueryId::SYSTEM_IDENTITY,
        ),
        ("ROS_SYSINFO_QUERY_UPTIME", SysinfoQueryId::UPTIME),
        ("ROS_SYSINFO_QUERY_MOUNT_LIST", SysinfoQueryId::MOUNT_LIST),
        (
            "ROS_SYSINFO_QUERY_RESOURCE_LIMITS",
            SysinfoQueryId::RESOURCE_LIMITS,
        ),
    ];
    for (name, id) in query_ids {
        let _ = writeln!(out, "#define {name} ((uint16_t){}u)", id.as_u16());
    }
    out.push('\n');

    out.push_str("/* Process lifecycle state carried in a process record (uint8_t). */\n");
    let process_states = [
        ("ROS_PROCESS_STATE_RUNNABLE", ProcessState::Runnable),
        ("ROS_PROCESS_STATE_RUNNING", ProcessState::Running),
        ("ROS_PROCESS_STATE_BLOCKED", ProcessState::Blocked),
        ("ROS_PROCESS_STATE_ZOMBIE", ProcessState::Zombie),
        ("ROS_PROCESS_STATE_STOPPED", ProcessState::Stopped),
    ];
    for (name, state) in process_states {
        let _ = writeln!(out, "#define {name} ((uint8_t){}u)", state.as_u8());
    }
    out.push('\n');
}

/// Emit the inline-buffer capacities and the per-record packed wire sizes.
fn sysinfo_emit_record_sizes(out: &mut String) {
    use std::fmt::Write as _;
    out.push_str("/* Inline fixed-buffer capacities carried in the record types below. */\n");
    let _ = writeln!(out, "#define ROS_PROCESS_NAME_MAX {PROCESS_NAME_MAX}u");
    let _ = writeln!(out, "#define ROS_MACHINE_ID_LEN {MACHINE_ID_LEN}u");
    let _ = writeln!(out, "#define ROS_HOSTNAME_MAX {HOSTNAME_MAX}u");
    let _ = writeln!(out, "#define ROS_MOUNT_SOURCE_MAX {MOUNT_SOURCE_MAX}u");
    let _ = writeln!(out, "#define ROS_MOUNT_TARGET_MAX {MOUNT_TARGET_MAX}u");
    let _ = writeln!(out, "#define ROS_MOUNT_FSTYPE_MAX {MOUNT_FSTYPE_MAX}u");
    out.push('\n');

    out.push_str("/* Packed little-endian wire size of each sysinfo record type, in bytes. */\n");
    let wire_lens = [
        (
            "ROS_SYSINFO_REQUEST_HEADER_WIRE_LEN",
            SysinfoRequestHeader::WIRE_LEN,
        ),
        (
            "ROS_PROCESS_LIST_REQUEST_WIRE_LEN",
            ProcessListRequest::WIRE_LEN,
        ),
        ("ROS_PROCESS_RECORD_WIRE_LEN", ProcessRecord::WIRE_LEN),
        (
            "ROS_KERNEL_MEMORY_STATS_WIRE_LEN",
            KernelMemoryStats::WIRE_LEN,
        ),
        ("ROS_UPTIME_WIRE_LEN", Uptime::WIRE_LEN),
        ("ROS_SYSTEM_IDENTITY_WIRE_LEN", SystemIdentity::WIRE_LEN),
        (
            "ROS_MOUNT_LIST_REQUEST_WIRE_LEN",
            MountListRequest::WIRE_LEN,
        ),
        ("ROS_MOUNT_RECORD_WIRE_LEN", MountRecord::WIRE_LEN),
        (
            "ROS_RESOURCE_LIMIT_RECORD_WIRE_LEN",
            ResourceLimitRecord::WIRE_LEN,
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
        "#define ROS_SYSINFO_RESOURCE_LIMITS_REPORT_LEN {RESOURCE_LIMITS_REPORT_LEN}u"
    );
    out.push('\n');
}

/// The C struct mirrors of the nine `#[repr(C)]` System Information wire
/// types, as static text (the field names/order are part of the frozen ABI
/// view; the in-module pinning test checks the layout against `lib/abi`).
const SYSINFO_RECORD_TYPEDEFS: &str = concat!(
    "/* Envelope prefixing every sysinfo request; encoded little-endian on the wire. */\n\
         typedef struct ros_sysinfo_request_header {\n\
         \x20   uint32_t magic;\n\
         \x20   uint16_t version;\n\
         \x20   uint16_t flags;\n\
         \x20   uint16_t query;\n\
         \x20   uint16_t reserved;\n\
         \x20   uint32_t payload_len;\n\
         \x20   uint64_t request_id;\n\
         } ros_sysinfo_request_header_t;\n\n",
    "/* Process-list request payload (offset/limit paging). */\n\
         typedef struct ros_process_list_request {\n\
         \x20   uint32_t offset;\n\
         \x20   uint16_t limit;\n\
         \x20   uint16_t flags;\n\
         } ros_process_list_request_t;\n\n",
    "/* One process entry; the inline name is valid for name_len bytes. */\n\
         typedef struct ros_process_record {\n\
         \x20   uint64_t pid;\n\
         \x20   uint64_t parent_pid;\n\
         \x20   uint32_t uid;\n\
         \x20   uint32_t gid;\n\
         \x20   uint8_t state;\n\
         \x20   uint8_t cpu;\n\
         \x20   uint8_t name_len;\n\
         \x20   uint8_t name[ROS_PROCESS_NAME_MAX];\n\
         } ros_process_record_t;\n\n",
    "/* Kernel memory statistics response. */\n\
         typedef struct ros_kernel_memory_stats {\n\
         \x20   uint64_t total_bytes;\n\
         \x20   uint64_t free_bytes;\n\
         \x20   uint64_t kernel_heap_bytes;\n\
         \x20   uint64_t user_resident_bytes;\n\
         \x20   uint32_t page_size;\n\
         \x20   uint32_t reserved;\n\
         } ros_kernel_memory_stats_t;\n\n",
    "/* Uptime response: monotonic span since boot + wall-clock boot instant. */\n\
         typedef struct ros_uptime {\n\
         \x20   ros_duration64_t since_boot;\n\
         \x20   ros_time64_t boot_time;\n\
         } ros_uptime_t;\n\n",
    "/* Machine identity response; the inline hostname is valid for hostname_len bytes. */\n\
         typedef struct ros_system_identity {\n\
         \x20   uint8_t machine_id[ROS_MACHINE_ID_LEN];\n\
         \x20   uint16_t version_major;\n\
         \x20   uint16_t version_minor;\n\
         \x20   uint16_t version_patch;\n\
         \x20   uint8_t hostname_len;\n\
         \x20   uint8_t hostname[ROS_HOSTNAME_MAX];\n\
         } ros_system_identity_t;\n\n",
    "/* Mount-list request payload (offset/limit paging). */\n\
         typedef struct ros_mount_list_request {\n\
         \x20   uint32_t offset;\n\
         \x20   uint16_t limit;\n\
         \x20   uint16_t flags;\n\
         } ros_mount_list_request_t;\n\n",
    "/* One mount-table entry. `flags` is a MountFlags bitmap (AGENTS.md sec.5.3);\n\
         * its flag bits are defined by the filesystem driver ABI. The inline source/\n\
         * target/fstype buffers are valid for their respective *_len byte counts. */\n\
         typedef struct ros_mount_record {\n\
         \x20   uint32_t flags;\n\
         \x20   uint8_t source_len;\n\
         \x20   uint8_t target_len;\n\
         \x20   uint8_t fstype_len;\n\
         \x20   uint8_t source[ROS_MOUNT_SOURCE_MAX];\n\
         \x20   uint8_t target[ROS_MOUNT_TARGET_MAX];\n\
         \x20   uint8_t fstype[ROS_MOUNT_FSTYPE_MAX];\n\
         } ros_mount_record_t;\n\n",
    "/* One row of the RESOURCE_LIMITS response: a resource's effective soft/hard\n\
         * bound (a ros_resource_limit_t) and the caller's current live usage of it.\n\
         * The full response is ROS_LIMIT_KIND_COUNT records in LimitKind order. */\n\
         typedef struct ros_resource_limit_record {\n\
         \x20   uint32_t kind;\n\
         \x20   uint32_t reserved;\n\
         \x20   ros_resource_limit_t limit;\n\
         \x20   uint64_t usage;\n\
         } ros_resource_limit_record_t;\n\n",
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
        "#define ROS_DRIVER_MANIFEST_MAGIC {DRIVER_MANIFEST_MAGIC:#x}u"
    );
    out.push_str("/* Maximum number of capability identifiers a driver manifest may request. */\n");
    let _ = writeln!(
        out,
        "#define ROS_DRIVER_MANIFEST_MAX_CAPABILITIES {DRIVER_MANIFEST_MAX_CAPABILITIES}u"
    );
    out.push_str("/* Maximum number of bind-table entries a driver manifest may declare. */\n");
    let _ = writeln!(
        out,
        "#define ROS_DRIVER_MANIFEST_MAX_BIND_KEYS {DRIVER_MANIFEST_MAX_BIND_KEYS}u"
    );
    out.push_str("/* Length, in bytes, of the Ed25519 signer public key. */\n");
    let _ = writeln!(
        out,
        "#define ROS_DRIVER_SIGNER_PUBKEY_LEN {DRIVER_SIGNER_PUBKEY_LEN}u"
    );
    out.push_str("/* Length, in bytes, of the Ed25519 manifest signature. */\n");
    let _ = writeln!(
        out,
        "#define ROS_DRIVER_SIGNATURE_LEN {DRIVER_SIGNATURE_LEN}u"
    );
    out.push_str("/* Packed little-endian wire size of a driver manifest, in bytes. */\n");
    let _ = writeln!(
        out,
        "#define ROS_DRIVER_MANIFEST_WIRE_LEN {}u",
        DriverManifest::WIRE_LEN
    );
    out.push_str("/* Packed little-endian wire size of one bind-table entry, in bytes. */\n");
    let _ = writeln!(
        out,
        "#define ROS_DRIVER_BIND_KEY_WIRE_LEN {}u",
        DriverBindKey::WIRE_LEN
    );
    out.push('\n');

    out.push_str(
        "/* Magic word identifying an abi-v1 driver register reply (\"DRR1\" little-endian). */\n",
    );
    let _ = writeln!(
        out,
        "#define ROS_DRIVER_REGISTER_REPLY_MAGIC {DRIVER_REGISTER_REPLY_MAGIC:#x}u"
    );
    out.push_str("/* `status` value of a successful register reply; any other value is a\n * ROS_DRIVER_ERROR_* code. */\n");
    let _ = writeln!(
        out,
        "#define ROS_DRIVER_REGISTER_STATUS_OK ((int32_t){DRIVER_REGISTER_STATUS_OK})"
    );
    out.push_str("/* Packed little-endian wire size of a driver register reply, in bytes. */\n");
    let _ = writeln!(
        out,
        "#define ROS_DRIVER_REGISTER_REPLY_WIRE_LEN {}u",
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
        "#define ROS_DRIVER_KIND_USER_SPACE ((uint8_t){}u)",
        DriverKind::UserSpace.as_u8()
    );
    let _ = writeln!(
        out,
        "#define ROS_DRIVER_KIND_IN_KERNEL ((uint8_t){}u)",
        DriverKind::InKernel.as_u8()
    );
    out.push('\n');

    out.push_str("/* Payload sensitivity hint (uint8_t); SENSITIVE requires zero-on-free. */\n");
    let _ = writeln!(
        out,
        "#define ROS_BUFFER_CLASS_NON_SENSITIVE ((uint8_t){}u)",
        BufferClass::NonSensitive.as_u8()
    );
    let _ = writeln!(
        out,
        "#define ROS_BUFFER_CLASS_SENSITIVE ((uint8_t){}u)",
        BufferClass::Sensitive.as_u8()
    );
    out.push('\n');

    out.push_str("/* Sentinel \"no driver handle\"; a live handle travels as a uint64_t. */\n");
    let _ = writeln!(
        out,
        "#define ROS_DRIVER_HANDLE_NONE ((uint64_t){}ull)",
        DriverHandle::NONE.as_u64()
    );
    out.push('\n');

    out.push_str("/* Stable driver-ABI error codes (int32_t), disjoint from ROS_E_* errno. */\n");
    for (name, err) in [
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
    ] {
        let _ = writeln!(
            out,
            "#define ROS_DRIVER_ERROR_{name} ((int32_t){})",
            err.as_i32()
        );
    }
    out.push('\n');
}

/// Emit the driver-submodule POD constants: the `VIRTIO_PCI_*` ids, the
/// Ethernet `MAC_ADDRESS_LEN`, the [`MountFlags`] bit set, and the
/// [`NodeId`] sentinel (every value read from `lib/abi`).
///
/// The [`MountFlags`] bits live here — not in `rustos_sysinfo.h` where the
/// `MountRecord.flags` field is a bare `uint32_t` — because the flag
/// semantics are owned by the filesystem driver ABI.
///
/// [`MountFlags`]: rustos_abi::driver::filesystem::MountFlags
/// [`NodeId`]: rustos_abi::driver::filesystem::NodeId
fn driver_emit_submodule_constants(out: &mut String) {
    use rustos_abi::driver::filesystem::{MountFlags, NodeId};
    use rustos_abi::driver::net::MAC_ADDRESS_LEN;
    use rustos_abi::{
        VIRTIO_PCI_CFG_COMMON, VIRTIO_PCI_CFG_DEVICE, VIRTIO_PCI_CFG_ISR, VIRTIO_PCI_CFG_NOTIFY,
        VIRTIO_PCI_CFG_PCI, VIRTIO_PCI_VENDOR_ID,
    };
    use std::fmt::Write as _;

    out.push_str(
        "/* PCI vendor ID assigned to virtio devices (uint16_t; virtio 1.1 sec.4.1.2). */\n",
    );
    let _ = writeln!(
        out,
        "#define ROS_VIRTIO_PCI_VENDOR_ID ((uint16_t){VIRTIO_PCI_VENDOR_ID:#x}u)"
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
        let _ = writeln!(out, "#define ROS_VIRTIO_PCI_CFG_{name} ((uint8_t){value}u)");
    }
    out.push('\n');

    out.push_str("/* Length, in bytes, of an Ethernet MAC address. */\n");
    let _ = writeln!(out, "#define ROS_MAC_ADDRESS_LEN {MAC_ADDRESS_LEN}u");
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
            "#define ROS_MOUNT_FLAG_{name} ((uint32_t){:#x}u)",
            flag.bits()
        );
    }
    out.push('\n');

    out.push_str("/* Sentinel \"no node\"; a live NodeId travels as a uint64_t. */\n");
    let _ = writeln!(
        out,
        "#define ROS_NODE_ID_NONE ((uint64_t){}ull)",
        NodeId::NONE.raw()
    );
    out.push('\n');
}

/// Emit the driver-submodule enum discriminants: [`DisplayFormat`],
/// [`NodeKind`], and the driver-class [`InputEventKind`] (every value read
/// from `lib/abi`).
///
/// The driver input-event kinds are spelled `ROS_INPUT_EVENT_KIND_*` to
/// stay disjoint from the windowing `ROS_INPUT_KIND_*` codes in
/// `rustos_input.h`; they are different ABIs that happen to share the word
/// "input".
///
/// [`DisplayFormat`]: rustos_abi::driver::display::DisplayFormat
/// [`NodeKind`]: rustos_abi::driver::filesystem::NodeKind
/// [`InputEventKind`]: rustos_abi::driver::input::InputEventKind
fn driver_emit_submodule_discriminants(out: &mut String) {
    use rustos_abi::driver::display::DisplayFormat;
    use rustos_abi::driver::filesystem::NodeKind;
    use rustos_abi::driver::input::InputEventKind;
    use std::fmt::Write as _;

    out.push_str(
        "/* Display pixel encoding (uint8_t); named by the byte order of the first pixel. */\n",
    );
    for (name, fmt) in [
        ("RGBA8888", DisplayFormat::Rgba8888),
        ("BGRA8888", DisplayFormat::Bgra8888),
    ] {
        let _ = writeln!(
            out,
            "#define ROS_DISPLAY_FORMAT_{name} ((uint8_t){}u)",
            fmt.as_u8()
        );
    }
    out.push('\n');

    out.push_str("/* Filesystem node kind (uint8_t). */\n");
    let _ = writeln!(
        out,
        "#define ROS_NODE_KIND_DIRECTORY ((uint8_t){}u)",
        NodeKind::Directory as u8
    );
    let _ = writeln!(
        out,
        "#define ROS_NODE_KIND_REGULAR_FILE ((uint8_t){}u)",
        NodeKind::RegularFile as u8
    );
    out.push('\n');

    out.push_str(
        "/* Driver input-event kind (uint8_t); distinct from the windowing ROS_INPUT_KIND_*. */\n",
    );
    for (name, kind) in [
        ("KEY", InputEventKind::Key),
        ("POINTER", InputEventKind::Pointer),
        ("SCROLL", InputEventKind::Scroll),
    ] {
        let _ = writeln!(
            out,
            "#define ROS_INPUT_EVENT_KIND_{name} ((uint8_t){}u)",
            kind.as_u8()
        );
    }
    out.push('\n');
}

/// `rustos_driver.h` — the driver-class ABI.
///
/// `ros_driver_manifest_t` mirrors the `#[repr(C)]` layout of
/// [`DriverManifest`] (the signed driver-manifest prefix; naturally aligned,
/// so the struct size equals the wire size), `ros_driver_bind_key_t` mirrors
/// [`DriverBindKey`] (one bind-table entry: a `ros_hw_match_key_t` from
/// `rustos_hwtree.h` plus the bind priority), and
/// `ros_driver_register_reply_t`
/// mirrors [`DriverRegisterReply`] (the register-handshake outcome a spawned
/// driver process reports to its host over IPC) with its
/// `ROS_DRIVER_REGISTER_REPLY_MAGIC` / `_STATUS_OK` / `_WIRE_LEN` constants.
/// Alongside them the header declares
/// the `ROS_DRIVER_MANIFEST_MAGIC` / `_MAX_CAPABILITIES` / `_MAX_BIND_KEYS` /
/// `_WIRE_LEN` / `ROS_DRIVER_BIND_KEY_WIRE_LEN` and
/// signer-key/signature length constants, the [`DriverKind`] / [`BufferClass`]
/// `#[repr(u8)]` and [`DriverError`] `#[repr(i32)]` discriminant sets, and the
/// [`DriverHandle`] `ROS_DRIVER_HANDLE_NONE` sentinel (a live driver handle
/// travels as a `uint64_t`). The syscall-table-hash length is shared with the
/// application manifest, so the struct reuses `ROS_SYSCALL_TABLE_HASH_LEN` from
/// `rustos_manifest.h` rather than re-declaring it.
///
/// The header also carries the driver-class **submodule** POD surface: the
/// `VIRTIO_PCI_*` / `MAC_ADDRESS_LEN` / [`MountFlags`] / [`NodeId`] constants
/// (see [`driver_emit_submodule_constants`]), the [`DisplayFormat`] /
/// [`NodeKind`] / [`InputEventKind`] discriminants (see
/// [`driver_emit_submodule_discriminants`]), and the struct mirrors in
/// [`DRIVER_SUBMODULE_TYPEDEFS`]. `NodeTimes` is built from `ros_time64_t`, so
/// the header `#include`s `rustos_time.h`. Every numeric value and discriminant
/// is read from `lib/abi`, never re-typed; only the C spelling lives here.
///
/// [`MountFlags`]: rustos_abi::driver::filesystem::MountFlags
/// [`NodeId`]: rustos_abi::driver::filesystem::NodeId
/// [`DisplayFormat`]: rustos_abi::driver::display::DisplayFormat
/// [`NodeKind`]: rustos_abi::driver::filesystem::NodeKind
/// [`InputEventKind`]: rustos_abi::driver::input::InputEventKind
fn generate_driver() -> String {
    let mut out =
        banner("Driver-class ABI core: manifest, kinds, errors (AGENTS.md sec.8, sec.9).");
    out.push_str("#ifndef ROS_DRIVER_H\n#define ROS_DRIVER_H\n\n");
    out.push_str("#include <stdint.h>\n");
    out.push_str("#include \"rustos_hwtree.h\"\n");
    out.push_str("#include \"rustos_manifest.h\"\n");
    out.push_str("#include \"rustos_time.h\"\n\n");

    driver_emit_constants(&mut out);
    driver_emit_discriminants(&mut out);
    driver_emit_submodule_constants(&mut out);
    driver_emit_submodule_discriminants(&mut out);

    out.push_str(
        "/* Signed driver-manifest prefix; encoded little-endian on the wire. */\n\
         typedef struct ros_driver_manifest {\n\
         \x20   uint32_t magic;\n\
         \x20   uint32_t abi_version;\n\
         \x20   uint8_t kind;\n\
         \x20   uint8_t bind_key_count;\n\
         \x20   uint16_t capability_count;\n\
         \x20   uint8_t syscall_table_hash[ROS_SYSCALL_TABLE_HASH_LEN];\n\
         \x20   uint8_t signer_pubkey[ROS_DRIVER_SIGNER_PUBKEY_LEN];\n\
         \x20   uint8_t signature[ROS_DRIVER_SIGNATURE_LEN];\n\
         } ros_driver_manifest_t;\n\n",
    );

    out.push_str(
        "/* One bind-table entry: a hardware-tree match key plus the manifest's\n\
         \x20* bind priority (AGENTS.md sec.18.3). bind_key_count entries follow the\n\
         \x20* capability body; all are covered by the manifest signature. */\n\
         typedef struct ros_driver_bind_key {\n\
         \x20   uint16_t priority;\n\
         \x20   uint16_t reserved0;\n\
         \x20   ros_hw_match_key_t key;\n\
         } ros_driver_bind_key_t;\n\n",
    );

    out.push_str(
        "/* Outcome of a spawned driver process's register() entry, sent to the\n\
         \x20* driver host over IPC; encoded little-endian on the wire. `status` is\n\
         \x20* ROS_DRIVER_REGISTER_STATUS_OK or a ROS_DRIVER_ERROR_* code; `handle` is\n\
         \x20* non-zero exactly when `status` is OK (informational only — the host\n\
         \x20* mints its own unforgeable handle). */\n\
         typedef struct ros_driver_register_reply {\n\
         \x20   uint32_t magic;\n\
         \x20   uint32_t abi_version;\n\
         \x20   int32_t status;\n\
         \x20   uint32_t reserved0;\n\
         \x20   uint64_t handle;\n\
         } ros_driver_register_reply_t;\n\n",
    );

    out.push_str(DRIVER_SUBMODULE_TYPEDEFS);

    out.push_str("#endif /* ROS_DRIVER_H */\n");
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
    "/* Block-device geometry (drivers/storage/*). */\n\
         typedef struct ros_block_geometry {\n\
         \x20   uint32_t block_size;\n\
         \x20   uint64_t block_count;\n\
         } ros_block_geometry_t;\n\n",
    "/* Discard (TRIM/unmap) capability a block device reports. */\n\
         typedef struct ros_discard_capability {\n\
         \x20   uint8_t supported;\n\
         \x20   uint64_t granularity_blocks;\n\
         \x20   uint64_t max_blocks_per_request;\n\
         } ros_discard_capability_t;\n\n",
    "/* Point-in-time device-health snapshot (SMART / NVMe telemetry). */\n\
         typedef struct ros_health_snapshot {\n\
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
         } ros_health_snapshot_t;\n\n",
    "/* Identifying tuple for a discovered device (drivers/bus/*).\n\
         * `device_class` mirrors the Rust `class` field (renamed for C++). */\n\
         typedef struct ros_bus_device {\n\
         \x20   uint32_t vendor;\n\
         \x20   uint32_t device;\n\
         \x20   uint16_t device_class;\n\
         \x20   uint16_t reserved0;\n\
         \x20   uint64_t address;\n\
         } ros_bus_device_t;\n\n",
    "/* Active display mode (drivers/display/*); `format` is a ROS_DISPLAY_FORMAT_*. */\n\
         typedef struct ros_display_mode {\n\
         \x20   uint32_t width_px;\n\
         \x20   uint32_t height_px;\n\
         \x20   uint32_t stride_bytes;\n\
         \x20   uint8_t format;\n\
         } ros_display_mode_t;\n\n",
    "/* What a hardware compositor back-end can do this frame. */\n\
         typedef struct ros_accel_caps {\n\
         \x20   uint32_t max_layers;\n\
         \x20   uint32_t max_width_px;\n\
         \x20   uint32_t max_height_px;\n\
         \x20   uint8_t per_layer_opacity;\n\
         } ros_accel_caps_t;\n\n",
    "/* Structural metadata about a filesystem node; `kind` is a ROS_NODE_KIND_*. */\n\
         typedef struct ros_node_info {\n\
         \x20   uint8_t kind;\n\
         \x20   uint64_t size;\n\
         } ros_node_info_t;\n\n",
    "/* One directory entry; `node` is a NodeId (uint64_t), `kind` a ROS_NODE_KIND_*. */\n\
         typedef struct ros_dir_entry {\n\
         \x20   uint64_t node;\n\
         \x20   uint8_t kind;\n\
         \x20   uintptr_t name_len;\n\
         } ros_dir_entry_t;\n\n",
    "/* The four AGENTS.md sec.21 timestamps stored for a filesystem node. */\n\
         typedef struct ros_node_times {\n\
         \x20   ros_time64_t created;\n\
         \x20   ros_time64_t modified;\n\
         \x20   ros_time64_t accessed;\n\
         \x20   ros_time64_t changed;\n\
         } ros_node_times_t;\n\n",
    "/* A single input event; `kind` is a ROS_INPUT_EVENT_KIND_*. */\n\
         typedef struct ros_input_event {\n\
         \x20   uint8_t kind;\n\
         \x20   uint8_t reserved0;\n\
         \x20   uint16_t code;\n\
         \x20   int32_t value;\n\
         } ros_input_event_t;\n\n",
    "/* A 48-bit IEEE 802 link-layer address (drivers/network/*). */\n\
         typedef struct ros_mac_address {\n\
         \x20   uint8_t octets[ROS_MAC_ADDRESS_LEN];\n\
         } ros_mac_address_t;\n\n",
);

/// `rustos_syscall.h` — the syscall numbers and C entry-point prototypes.
fn generate_syscall() -> String {
    use std::fmt::Write as _;
    let mut out = banner("Syscall numbers and C entry-point prototypes (AGENTS.md sec.9).");
    out.push_str("#ifndef ROS_SYSCALL_H\n#define ROS_SYSCALL_H\n\n");
    out.push_str("#include <stdint.h>\n\n");
    out.push_str("#ifdef __cplusplus\nextern \"C\" {\n#endif\n\n");

    out.push_str("/* Syscall numbers (AGENTS.md sec.9). */\n");
    let _ = writeln!(out, "#define ROS_SYSCALL_MAX_ARGS {SYSCALL_MAX_ARGS}u");
    for spec in SYSCALLS {
        let _ = writeln!(
            out,
            "#define ROS_SYS_{} {}u",
            spec.name.to_ascii_uppercase(),
            spec.number.as_u16()
        );
    }
    out.push('\n');

    out.push_str("/* Syscall entry points, implemented by the user-space stub library. */\n");
    for spec in SYSCALLS {
        let _ = writeln!(out, "{}", prototype(spec));
    }
    out.push('\n');

    out.push_str("#ifdef __cplusplus\n} /* extern \"C\" */\n#endif\n\n");
    out.push_str("#endif /* ROS_SYSCALL_H */\n");
    out
}

/// `rustos_abi.h` — the umbrella header that includes every module header.
fn generate_umbrella() -> String {
    use std::fmt::Write as _;
    let mut out = banner(
        "Umbrella header: the whole abi-v1 C surface in one include.\n\
         * Each syscall is exported by the user-space stub library under the\n\
         * symbol `ros_sys_<name>` (e.g. `ros_sys_ipc_send`); link against\n\
         * that library to call the kernel from a non-Rust program.",
    );
    out.push_str("#ifndef ROS_ABI_H\n#define ROS_ABI_H\n\n");
    out.push_str("/* ABI version this header set describes (AGENTS.md sec.9). */\n");
    let _ = writeln!(out, "#define ROS_ABI_VERSION {ABI_VERSION_V1}u\n");
    out.push_str("#include \"rustos_error.h\"\n");
    out.push_str("#include \"rustos_capability.h\"\n");
    out.push_str("#include \"rustos_time.h\"\n");
    out.push_str("#include \"rustos_random.h\"\n");
    out.push_str("#include \"rustos_log.h\"\n");
    out.push_str("#include \"rustos_rlimit.h\"\n");
    out.push_str("#include \"rustos_memory.h\"\n");
    out.push_str("#include \"rustos_hwtree.h\"\n");
    out.push_str("#include \"rustos_ipc.h\"\n");
    out.push_str("#include \"rustos_stdinfo.h\"\n");
    out.push_str("#include \"rustos_manifest.h\"\n");
    out.push_str("#include \"rustos_input.h\"\n");
    out.push_str("#include \"rustos_appinfo.h\"\n");
    out.push_str("#include \"rustos_rxe.h\"\n");
    out.push_str("#include \"rustos_process.h\"\n");
    out.push_str("#include \"rustos_sysinfo.h\"\n");
    out.push_str("#include \"rustos_driver.h\"\n");
    out.push_str("#include \"rustos_syscall.h\"\n\n");
    out.push_str("#endif /* ROS_ABI_H */\n");
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
            file_name: "rustos_error.h",
            body: generate_error(),
        },
        GeneratedHeader {
            file_name: "rustos_capability.h",
            body: generate_capability(),
        },
        GeneratedHeader {
            file_name: "rustos_time.h",
            body: generate_time(),
        },
        GeneratedHeader {
            file_name: "rustos_random.h",
            body: generate_random(),
        },
        GeneratedHeader {
            file_name: "rustos_log.h",
            body: generate_log(),
        },
        GeneratedHeader {
            file_name: "rustos_rlimit.h",
            body: generate_rlimit(),
        },
        GeneratedHeader {
            file_name: "rustos_memory.h",
            body: generate_memory(),
        },
        GeneratedHeader {
            file_name: "rustos_hwtree.h",
            body: generate_hwtree(),
        },
        GeneratedHeader {
            file_name: "rustos_ipc.h",
            body: generate_ipc(),
        },
        GeneratedHeader {
            file_name: "rustos_stdinfo.h",
            body: generate_stdinfo(),
        },
        GeneratedHeader {
            file_name: "rustos_manifest.h",
            body: generate_manifest(),
        },
        GeneratedHeader {
            file_name: "rustos_input.h",
            body: generate_input(),
        },
        GeneratedHeader {
            file_name: "rustos_appinfo.h",
            body: generate_appinfo(),
        },
        GeneratedHeader {
            file_name: "rustos_rxe.h",
            body: generate_rxe(),
        },
        GeneratedHeader {
            file_name: "rustos_process.h",
            body: generate_process(),
        },
        GeneratedHeader {
            file_name: "rustos_sysinfo.h",
            body: generate_sysinfo(),
        },
        GeneratedHeader {
            file_name: "rustos_driver.h",
            body: generate_driver(),
        },
        GeneratedHeader {
            file_name: "rustos_syscall.h",
            body: generate_syscall(),
        },
        GeneratedHeader {
            file_name: "rustos_abi.h",
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
    use rustos_abi::SYSCALL_NAME_MAX;

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
        let h = body("rustos_abi.h");
        assert!(h.contains("#ifndef ROS_ABI_H"), "guard present");
        assert!(h.contains("#define ROS_ABI_VERSION 1u"), "version macro");
        for module in [
            "rustos_error.h",
            "rustos_capability.h",
            "rustos_time.h",
            "rustos_random.h",
            "rustos_log.h",
            "rustos_rlimit.h",
            "rustos_memory.h",
            "rustos_hwtree.h",
            "rustos_ipc.h",
            "rustos_stdinfo.h",
            "rustos_manifest.h",
            "rustos_input.h",
            "rustos_appinfo.h",
            "rustos_rxe.h",
            "rustos_process.h",
            "rustos_sysinfo.h",
            "rustos_driver.h",
            "rustos_syscall.h",
        ] {
            assert!(
                h.contains(&format!("#include \"{module}\"")),
                "umbrella must include {module}: {h}"
            );
        }
    }

    #[test]
    fn error_header_has_codes() {
        let h = body("rustos_error.h");
        assert!(h.contains("#ifndef ROS_ERROR_H"), "guard present");
        assert!(h.contains("#define ROS_E_PERMISSION_DENIED 6"), "errno");
    }

    #[test]
    fn capability_header_has_ids() {
        let h = body("rustos_capability.h");
        assert!(h.contains("#ifndef ROS_CAPABILITY_H"), "guard present");
        assert!(h.contains("#include <stdint.h>"), "stdint included");
        assert!(
            h.contains("#define ROS_CAP_USER_ADMIN ((uint16_t)5u)"),
            "capability id carries its canonical uint16_t type: {h}"
        );
    }

    #[test]
    fn syscall_header_has_numbers_and_prototypes() {
        let h = body("rustos_syscall.h");
        assert!(h.contains("#ifndef ROS_SYSCALL_H"), "guard present");
        assert!(h.contains("#include <stdint.h>"), "stdint included");
        assert!(h.contains("extern \"C\""), "C++ guard present");
        assert!(h.contains("#define ROS_SYS_EXIT 1u"), "syscall number");
        assert!(
            h.contains("void ros_sys_yield(void);"),
            "nullary prototype: {h}"
        );
        assert!(
            h.contains("int32_t ros_sys_ipc_send(uint64_t a0, void * a1, uintptr_t a2);"),
            "typed prototype: {h}"
        );
    }

    #[test]
    fn rlimit_header_pins_kinds_and_struct() {
        let h = body("rustos_rlimit.h");
        assert!(h.contains("#ifndef ROS_RLIMIT_H"), "guard present");
        assert!(h.contains("#include <stdint.h>"), "stdint included");
        // The no-limit sentinel and a representative kind are read from
        // lib/abi, never re-typed.
        assert!(
            h.contains(&format!(
                "#define ROS_RLIMIT_INFINITY ((uint64_t){RLIMIT_INFINITY}u)"
            )),
            "infinity sentinel: {h}"
        );
        assert!(
            h.contains("#define ROS_LIMIT_KIND_PROCESSES ((uint32_t)2u)"),
            "processes kind macro: {h}"
        );
        assert!(
            h.contains(&format!(
                "#define ROS_LIMIT_KIND_COUNT ((uint32_t){}u)",
                LimitKind::COUNT
            )),
            "kind count: {h}"
        );
        assert!(
            h.contains("typedef struct ros_resource_limit {"),
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
        let h = body("rustos_time.h");
        assert!(h.contains("#ifndef ROS_TIME_H"), "guard present");
        assert!(h.contains("typedef struct ros_time64 {"), "time struct");
        assert!(
            h.contains("typedef struct ros_duration64 {"),
            "duration struct"
        );
        // Values are read from lib/abi, never re-typed: assert they match.
        assert!(h.contains(&format!("#define ROS_NANOS_PER_SEC {NANOS_PER_SEC}u")));
        assert!(h.contains(&format!(
            "#define ROS_TIME64_WIRE_LEN {}u",
            Time64::WIRE_LEN
        )));
        // The C struct mirrors the #[repr(C)] Rust layout (8 + 4 + 4 pad).
        assert_eq!(core::mem::size_of::<Time64>(), 16, "Time64 repr(C) size");
        assert_eq!(core::mem::align_of::<Time64>(), 8, "Time64 repr(C) align");
        assert_eq!(core::mem::size_of::<Duration64>(), 16, "Duration64 size");
    }

    #[test]
    fn random_header_pins_flags_and_limits() {
        let h = body("rustos_random.h");
        assert!(h.contains("#ifndef ROS_RANDOM_H"), "guard present");
        assert!(h.contains("#include <stdint.h>"), "stdint included");
        // Values are read from lib/abi, never re-typed: assert they match.
        assert!(
            h.contains(&format!(
                "#define ROS_RANDOM_FLAG_NON_BLOCKING {:#x}u",
                RandomFlags::NON_BLOCKING.bits()
            )),
            "non-blocking flag bit: {h}"
        );
        assert!(
            h.contains(&format!(
                "#define ROS_RANDOM_RESERVE_DEFAULT_BYTES ((uintptr_t){RANDOM_RESERVE_DEFAULT_BYTES}u)"
            )),
            "reserve default: {h}"
        );
        assert!(
            h.contains(&format!(
                "#define ROS_RANDOM_REQUEST_MAX_BYTES ((uintptr_t){RANDOM_REQUEST_MAX_BYTES}u)"
            )),
            "request max: {h}"
        );
    }

    #[test]
    fn memory_header_pins_map_flags() {
        let h = body("rustos_memory.h");
        assert!(h.contains("#ifndef ROS_MEMORY_H"), "guard present");
        assert!(h.contains("#include <stdint.h>"), "stdint included");
        // The flag bit value is read from lib/abi, never re-typed.
        assert!(
            h.contains(&format!(
                "#define ROS_MAP_FLAG_FIXED {:#x}u",
                MapFlags::FIXED.bits()
            )),
            "fixed flag bit: {h}"
        );
    }

    #[test]
    fn hwtree_header_pins_enums_and_layout() {
        let h = body("rustos_hwtree.h");
        assert!(h.contains("#ifndef ROS_HWTREE_H"), "guard present");
        assert!(h.contains("#include <stdint.h>"), "stdint included");
        // Values are read from lib/abi, never re-typed: assert they match.
        assert!(h.contains(&format!("#define ROS_HWTREE_VERSION {HWTREE_VERSION_V1}u")));
        assert!(h.contains(&format!("#define ROS_HW_NODE_ROOT {HW_NODE_ROOT}u")));
        assert!(
            h.contains(&format!(
                "#define ROS_HW_NODE_WIRE_LEN {}u",
                HwNode::WIRE_LEN
            )),
            "node wire len: {h}"
        );
        assert!(
            h.contains(&format!(
                "#define ROS_HW_CLASS_NETWORK ((uint16_t){}u)",
                HwDeviceClass::Network.as_u16()
            )),
            "device class macro: {h}"
        );
        assert!(
            h.contains(&format!(
                "#define ROS_HW_MATCH_PCI ((uint16_t){}u)",
                HwMatchKind::Pci.as_u16()
            )),
            "match kind macro: {h}"
        );
        assert!(
            h.contains(&format!(
                "#define ROS_HW_RES_IRQ ((uint16_t){}u)",
                HwResourceKind::Irq.as_u16()
            )),
            "resource kind macro: {h}"
        );
        assert!(h.contains("typedef struct ros_hw_node {"), "node struct");
        // The flat record structs mirror their #[repr(C)] layout exactly,
        // so their wire size equals their in-memory size.
        assert_eq!(core::mem::size_of::<HwMatchKey>(), HwMatchKey::WIRE_LEN);
        assert_eq!(core::mem::size_of::<HwResource>(), HwResource::WIRE_LEN);
    }

    #[test]
    fn ipc_header_pins_layout_and_values() {
        use rustos_abi::ipc::IPC_MESSAGE_MAX_PAYLOAD_LEN;
        let h = body("rustos_ipc.h");
        assert!(h.contains("#ifndef ROS_IPC_H"), "guard present");
        assert!(h.contains("#include <stdint.h>"), "stdint included");
        assert!(
            h.contains("typedef struct ros_ipc_message_header {"),
            "message-header struct"
        );
        assert!(
            h.contains("typedef struct ros_port_name {"),
            "port-name struct"
        );
        // Values are read from lib/abi, never re-typed: assert they match.
        assert!(
            h.contains(&format!(
                "#define ROS_IPC_MESSAGE_HEADER_MAGIC {IPC_MESSAGE_HEADER_MAGIC:#x}u"
            )),
            "magic word: {h}"
        );
        assert!(
            h.contains(&format!(
                "#define ROS_IPC_MESSAGE_MAX_PAYLOAD_LEN {IPC_MESSAGE_MAX_PAYLOAD_LEN}u"
            )),
            "max payload: {h}"
        );
        assert!(
            h.contains(&format!(
                "#define ROS_IPC_MESSAGE_HEADER_WIRE_LEN {}u",
                IpcMessageHeader::WIRE_LEN
            )),
            "header wire len: {h}"
        );
        assert!(
            h.contains(&format!(
                "#define ROS_PORT_NAME_MAX_LEN {PORT_NAME_MAX_LEN}u"
            )),
            "port-name max len: {h}"
        );
        assert!(
            h.contains(&format!(
                "#define ROS_PORT_NAME_WIRE_LEN {}u",
                PortName::WIRE_LEN
            )),
            "port-name wire len: {h}"
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
        let h = body("rustos_stdinfo.h");
        assert!(h.contains("#ifndef ROS_STDINFO_H"), "guard present");
        assert!(h.contains("#include <stdint.h>"), "stdint included");
        // Values are read from lib/abi, never re-typed: assert they match.
        assert!(
            h.contains(&format!("#define ROS_STDINFO_FD {STDINFO_FD}u")),
            "reserved fd: {h}"
        );
        assert!(
            h.contains(&format!(
                "#define ROS_STDINFO_VERSION_V1 {STDINFO_VERSION_V1}u"
            )),
            "version v1: {h}"
        );
        assert!(
            h.contains(&format!(
                "#define ROS_STDINFO_VERSION_CURRENT {STDINFO_VERSION_CURRENT}u"
            )),
            "current version: {h}"
        );
        assert!(
            h.contains(&format!(
                "#define ROS_STDINFO_KIND_OMISSION ((uint8_t){}u)",
                StdInfoKind::Omission as u8
            )),
            "omission discriminant: {h}"
        );
        assert!(
            h.contains(&format!(
                "#define ROS_STDINFO_KIND_CONTEXT ((uint8_t){}u)",
                StdInfoKind::Context as u8
            )),
            "context discriminant: {h}"
        );
        assert!(
            h.contains(&format!(
                "#define ROS_STDINFO_SEVERITY_INFO ((uint8_t){}u)",
                Severity::Info as u8
            )),
            "info severity: {h}"
        );
        assert!(
            h.contains(&format!(
                "#define ROS_STDINFO_SEVERITY_DEBUG ((uint8_t){}u)",
                Severity::Debug as u8
            )),
            "debug severity: {h}"
        );
    }

    #[test]
    fn manifest_header_pins_layout_and_values() {
        let h = body("rustos_manifest.h");
        assert!(h.contains("#ifndef ROS_MANIFEST_H"), "guard present");
        assert!(h.contains("#include <stdint.h>"), "stdint included");
        assert!(
            h.contains("typedef struct ros_manifest_header {"),
            "manifest-header struct"
        );
        // Values are read from lib/abi, never re-typed: assert they match.
        assert!(
            h.contains(&format!("#define ROS_MANIFEST_MAGIC {MANIFEST_MAGIC:#x}u")),
            "magic word: {h}"
        );
        assert!(
            h.contains(&format!(
                "#define ROS_MANIFEST_MAX_CAPABILITIES {MANIFEST_MAX_CAPABILITIES}u"
            )),
            "max capabilities: {h}"
        );
        assert!(
            h.contains(&format!(
                "#define ROS_SYSCALL_TABLE_HASH_LEN {SYSCALL_TABLE_HASH_LEN}u"
            )),
            "hash length: {h}"
        );
        assert!(
            h.contains(&format!(
                "#define ROS_MANIFEST_HEADER_WIRE_LEN {}u",
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
        use rustos_abi::{
            KeyInput, NamedKeyCode, PointerButtonCode, PointerInput, BUTTON_NONE, KEY_CLASS_CHAR,
            KEY_CLASS_NAMED, KEY_INPUT_MAGIC, KIND_KEY_PRESSED, KIND_KEY_RELEASED, KIND_MOVED,
            KIND_PRESSED, KIND_RELEASED, MOD_ALT, MOD_CTRL, MOD_MASK, MOD_META, MOD_SHIFT,
            POINTER_INPUT_MAGIC,
        };
        let h = body("rustos_input.h");
        assert!(h.contains("#ifndef ROS_INPUT_H"), "guard present");
        assert!(h.contains("#include <stdint.h>"), "stdint included");

        // Values are read from lib/abi, never re-typed: assert they match.
        let mut expected = vec![
            format!("#define ROS_POINTER_INPUT_MAGIC {POINTER_INPUT_MAGIC:#x}u"),
            format!("#define ROS_KEY_INPUT_MAGIC {KEY_INPUT_MAGIC:#x}u"),
            format!(
                "#define ROS_POINTER_INPUT_WIRE_LEN {}u",
                PointerInput::WIRE_LEN
            ),
            format!("#define ROS_KEY_INPUT_WIRE_LEN {}u", KeyInput::WIRE_LEN),
        ];
        for (name, value) in [
            ("ROS_INPUT_KIND_MOVED", KIND_MOVED),
            ("ROS_INPUT_KIND_PRESSED", KIND_PRESSED),
            ("ROS_INPUT_KIND_RELEASED", KIND_RELEASED),
            ("ROS_INPUT_KIND_KEY_PRESSED", KIND_KEY_PRESSED),
            ("ROS_INPUT_KIND_KEY_RELEASED", KIND_KEY_RELEASED),
            ("ROS_INPUT_BUTTON_NONE", BUTTON_NONE),
            (
                "ROS_POINTER_BUTTON_PRIMARY",
                PointerButtonCode::Primary.code(),
            ),
            (
                "ROS_POINTER_BUTTON_SECONDARY",
                PointerButtonCode::Secondary.code(),
            ),
            (
                "ROS_POINTER_BUTTON_MIDDLE",
                PointerButtonCode::Middle.code(),
            ),
            ("ROS_KEY_CLASS_CHAR", KEY_CLASS_CHAR),
            ("ROS_KEY_CLASS_NAMED", KEY_CLASS_NAMED),
        ] {
            expected.push(format!("#define {name} ((uint16_t){value}u)"));
        }
        for (name, bits) in [
            ("ROS_MOD_SHIFT", MOD_SHIFT),
            ("ROS_MOD_CTRL", MOD_CTRL),
            ("ROS_MOD_ALT", MOD_ALT),
            ("ROS_MOD_META", MOD_META),
            ("ROS_MOD_MASK", MOD_MASK),
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

    #[test]
    fn appinfo_header_pins_layout_constants_and_names() {
        use rustos_abi::{
            AppInfoHeader, BundleEntry, LibraryScope, APPINFO_MAGIC, APPINFO_MAX_CAPABILITIES,
            APPINFO_MAX_MIME, BUNDLE_ID_MAX, BUNDLE_NAME_MAX, BUNDLE_VERSION_MAX, MIME_ENTRY_LEN,
            MIME_TYPE_MAX, SYSTEM_LIBRARIES_DIR,
        };
        let h = body("rustos_appinfo.h");
        assert!(h.contains("#ifndef ROS_APPINFO_H"), "guard present");
        assert!(h.contains("#include <stdint.h>"), "stdint included");
        assert!(
            h.contains("typedef struct ros_appinfo_header {"),
            "appinfo-header struct"
        );
        // Values are read from lib/abi, never re-typed: assert they match.
        let expected = [
            format!("#define ROS_APPINFO_MAGIC {APPINFO_MAGIC:#x}u"),
            format!("#define ROS_APPINFO_MAX_CAPABILITIES {APPINFO_MAX_CAPABILITIES}u"),
            format!("#define ROS_APPINFO_MAX_MIME {APPINFO_MAX_MIME}u"),
            format!("#define ROS_BUNDLE_ID_MAX {BUNDLE_ID_MAX}u"),
            format!("#define ROS_BUNDLE_NAME_MAX {BUNDLE_NAME_MAX}u"),
            format!("#define ROS_BUNDLE_VERSION_MAX {BUNDLE_VERSION_MAX}u"),
            format!("#define ROS_MIME_TYPE_MAX {MIME_TYPE_MAX}u"),
            format!("#define ROS_MIME_ENTRY_LEN {MIME_ENTRY_LEN}u"),
            format!(
                "#define ROS_APPINFO_HEADER_WIRE_LEN {}u",
                AppInfoHeader::WIRE_LEN
            ),
            format!("#define ROS_SYSTEM_LIBRARIES_DIR \"{SYSTEM_LIBRARIES_DIR}\""),
            format!(
                "#define ROS_LIBRARY_SCOPE_BUNDLE ((uint8_t){}u)",
                LibraryScope::Bundle as u8
            ),
            format!(
                "#define ROS_LIBRARY_SCOPE_SYSTEM ((uint8_t){}u)",
                LibraryScope::System as u8
            ),
        ];
        for line in &expected {
            assert!(h.contains(line), "missing `{line}` in:\n{h}");
        }
        // Every permitted bundle entry name is exported, read from lib/abi.
        for entry in BundleEntry::ALL {
            let line = format!(
                "#define ROS_BUNDLE_ENTRY_{} \"{}\"",
                entry.as_str().to_ascii_uppercase(),
                entry.as_str()
            );
            assert!(h.contains(&line), "missing `{line}` in:\n{h}");
        }
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
        use rustos_abi::{
            LoadHeader, RxePermission, Segment, LOAD_FLAG_PIE, LOAD_MAGIC, LOAD_MAX_SEGMENTS,
            RXE_PAGE_SIZE, SEG_FLAG_EXEC, SEG_FLAG_READ, SEG_FLAG_WRITE, SYSCALL_TABLE_HASH_LEN,
        };
        let h = body("rustos_rxe.h");
        assert!(h.contains("#ifndef ROS_RXE_H"), "guard present");
        assert!(h.contains("#include <stdint.h>"), "stdint included");
        assert!(
            h.contains("typedef struct ros_load_header {"),
            "load-header struct"
        );
        // Values are read from lib/abi, never re-typed: assert they match.
        let expected = [
            format!("#define ROS_LOAD_MAGIC {LOAD_MAGIC:#x}u"),
            format!("#define ROS_RXE_PAGE_SIZE ((uint64_t){RXE_PAGE_SIZE}ull)"),
            format!("#define ROS_LOAD_MAX_SEGMENTS ((uintptr_t){LOAD_MAX_SEGMENTS}u)"),
            format!("#define ROS_LOAD_FLAG_PIE {LOAD_FLAG_PIE:#x}u"),
            format!("#define ROS_SEG_FLAG_READ {SEG_FLAG_READ:#x}u"),
            format!("#define ROS_SEG_FLAG_WRITE {SEG_FLAG_WRITE:#x}u"),
            format!("#define ROS_SEG_FLAG_EXEC {SEG_FLAG_EXEC:#x}u"),
            format!("#define ROS_LOAD_HEADER_WIRE_LEN {}u", LoadHeader::WIRE_LEN),
            format!("#define ROS_SEGMENT_WIRE_LEN {}u", Segment::WIRE_LEN),
            format!(
                "#define ROS_RXE_PERMISSION_READ_ONLY ((uint8_t){}u)",
                RxePermission::ReadOnly as u8
            ),
            format!(
                "#define ROS_RXE_PERMISSION_READ_EXECUTE ((uint8_t){}u)",
                RxePermission::ReadExecute as u8
            ),
            format!(
                "#define ROS_RXE_PERMISSION_READ_WRITE ((uint8_t){}u)",
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
        use rustos_abi::{
            ProcessStartHeader, StringSlot, PROCESS_START_MAGIC, PROCESS_START_MAX_STRINGS,
            PROCESS_START_MAX_STRING_LEN, PROCESS_START_MAX_TOTAL_LEN,
        };
        let h = body("rustos_process.h");
        assert!(h.contains("#ifndef ROS_PROCESS_H"), "guard present");
        assert!(h.contains("#include <stdint.h>"), "stdint included");
        assert!(
            h.contains("typedef struct ros_process_start_header {"),
            "start-header struct"
        );
        assert!(
            h.contains("typedef struct ros_string_slot {"),
            "string-slot struct"
        );
        // Values are read from lib/abi, never re-typed: assert they match.
        let expected = [
            format!("#define ROS_PROCESS_START_MAGIC {PROCESS_START_MAGIC:#x}u"),
            format!("#define ROS_PROCESS_START_MAX_STRINGS {PROCESS_START_MAX_STRINGS}u"),
            format!("#define ROS_PROCESS_START_MAX_STRING_LEN {PROCESS_START_MAX_STRING_LEN}u"),
            format!(
                "#define ROS_PROCESS_START_MAX_TOTAL_LEN ((uint64_t){PROCESS_START_MAX_TOTAL_LEN}ull)"
            ),
            format!(
                "#define ROS_PROCESS_START_HEADER_WIRE_LEN {}u",
                ProcessStartHeader::WIRE_LEN
            ),
            format!("#define ROS_STRING_SLOT_WIRE_LEN {}u", StringSlot::WIRE_LEN),
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
        use rustos_abi::{
            KernelMemoryStats, MountListRequest, MountRecord, ProcessListRequest, ProcessRecord,
            ProcessState, ResourceLimitRecord, SysinfoQueryId, SysinfoRequestHeader,
            SystemIdentity, Uptime, ENCODED_QUERY_TABLE_LEN, HOSTNAME_MAX, MACHINE_ID_LEN,
            MOUNT_FSTYPE_MAX, MOUNT_SOURCE_MAX, MOUNT_TARGET_MAX, PROCESS_NAME_MAX,
            RESOURCE_LIMITS_REPORT_LEN, SYSINFO_MAX_PAYLOAD_LEN, SYSINFO_QUERY_NAME_MAX,
            SYSINFO_QUERY_RECORD_LEN, SYSINFO_REQUEST_MAGIC, SYSINFO_VERSION_CURRENT,
            SYSINFO_VERSION_V1,
        };
        let h = body("rustos_sysinfo.h");
        assert!(h.contains("#ifndef ROS_SYSINFO_H"), "guard present");
        assert!(h.contains("#include <stdint.h>"), "stdint included");
        assert!(
            h.contains("#include \"rustos_time.h\""),
            "time header included for ros_uptime"
        );
        assert!(
            h.contains("#include \"rustos_rlimit.h\""),
            "rlimit header included for ros_resource_limit_t"
        );
        // Values are read from lib/abi, never re-typed: assert they match.
        let expected = [
            format!("#define ROS_SYSINFO_VERSION_V1 {SYSINFO_VERSION_V1}u"),
            format!("#define ROS_SYSINFO_VERSION_CURRENT {SYSINFO_VERSION_CURRENT}u"),
            format!("#define ROS_SYSINFO_REQUEST_MAGIC {SYSINFO_REQUEST_MAGIC:#x}u"),
            format!("#define ROS_SYSINFO_MAX_PAYLOAD_LEN {SYSINFO_MAX_PAYLOAD_LEN}u"),
            format!("#define ROS_SYSINFO_QUERY_ID_MAX {}u", SysinfoQueryId::MAX),
            format!("#define ROS_SYSINFO_QUERY_NAME_MAX {SYSINFO_QUERY_NAME_MAX}u"),
            format!("#define ROS_SYSINFO_QUERY_RECORD_LEN {SYSINFO_QUERY_RECORD_LEN}u"),
            format!("#define ROS_SYSINFO_ENCODED_QUERY_TABLE_LEN {ENCODED_QUERY_TABLE_LEN}u"),
            format!(
                "#define ROS_SYSINFO_QUERY_SELF_PROCESS_LIST ((uint16_t){}u)",
                SysinfoQueryId::SELF_PROCESS_LIST.as_u16()
            ),
            format!(
                "#define ROS_SYSINFO_QUERY_MOUNT_LIST ((uint16_t){}u)",
                SysinfoQueryId::MOUNT_LIST.as_u16()
            ),
            format!(
                "#define ROS_PROCESS_STATE_RUNNABLE ((uint8_t){}u)",
                ProcessState::Runnable as u8
            ),
            format!(
                "#define ROS_PROCESS_STATE_STOPPED ((uint8_t){}u)",
                ProcessState::Stopped as u8
            ),
            format!("#define ROS_PROCESS_NAME_MAX {PROCESS_NAME_MAX}u"),
            format!("#define ROS_MACHINE_ID_LEN {MACHINE_ID_LEN}u"),
            format!("#define ROS_HOSTNAME_MAX {HOSTNAME_MAX}u"),
            format!("#define ROS_MOUNT_SOURCE_MAX {MOUNT_SOURCE_MAX}u"),
            format!("#define ROS_MOUNT_TARGET_MAX {MOUNT_TARGET_MAX}u"),
            format!("#define ROS_MOUNT_FSTYPE_MAX {MOUNT_FSTYPE_MAX}u"),
            format!(
                "#define ROS_SYSINFO_REQUEST_HEADER_WIRE_LEN {}u",
                SysinfoRequestHeader::WIRE_LEN
            ),
            format!(
                "#define ROS_PROCESS_LIST_REQUEST_WIRE_LEN {}u",
                ProcessListRequest::WIRE_LEN
            ),
            format!(
                "#define ROS_PROCESS_RECORD_WIRE_LEN {}u",
                ProcessRecord::WIRE_LEN
            ),
            format!(
                "#define ROS_KERNEL_MEMORY_STATS_WIRE_LEN {}u",
                KernelMemoryStats::WIRE_LEN
            ),
            format!("#define ROS_UPTIME_WIRE_LEN {}u", Uptime::WIRE_LEN),
            format!(
                "#define ROS_SYSTEM_IDENTITY_WIRE_LEN {}u",
                SystemIdentity::WIRE_LEN
            ),
            format!(
                "#define ROS_MOUNT_LIST_REQUEST_WIRE_LEN {}u",
                MountListRequest::WIRE_LEN
            ),
            format!(
                "#define ROS_MOUNT_RECORD_WIRE_LEN {}u",
                MountRecord::WIRE_LEN
            ),
            format!(
                "#define ROS_SYSINFO_QUERY_RESOURCE_LIMITS ((uint16_t){}u)",
                SysinfoQueryId::RESOURCE_LIMITS.as_u16()
            ),
            format!(
                "#define ROS_RESOURCE_LIMIT_RECORD_WIRE_LEN {}u",
                ResourceLimitRecord::WIRE_LEN
            ),
            format!("#define ROS_SYSINFO_RESOURCE_LIMITS_REPORT_LEN {RESOURCE_LIMITS_REPORT_LEN}u"),
        ];
        for line in &expected {
            assert!(h.contains(line), "missing `{line}` in:\n{h}");
        }
    }

    #[test]
    fn sysinfo_header_declares_every_record_typedef() {
        let h = body("rustos_sysinfo.h");
        for typedef in [
            "typedef struct ros_sysinfo_request_header {",
            "typedef struct ros_process_list_request {",
            "typedef struct ros_process_record {",
            "typedef struct ros_kernel_memory_stats {",
            "typedef struct ros_uptime {",
            "typedef struct ros_system_identity {",
            "typedef struct ros_mount_list_request {",
            "typedef struct ros_mount_record {",
            "typedef struct ros_resource_limit_record {",
        ] {
            assert!(h.contains(typedef), "missing `{typedef}` in:\n{h}");
        }
    }

    #[test]
    fn sysinfo_header_struct_layout_matches_lib_abi() {
        use rustos_abi::{
            KernelMemoryStats, MountListRequest, MountRecord, ProcessListRequest, ProcessRecord,
            ProcessState, ResourceLimitRecord, SysinfoQueryId, SysinfoRequestHeader,
            SystemIdentity, Uptime,
        };
        // The C struct mirrors are the naturally-aligned #[repr(C)] in-memory
        // layout (the separate *_WIRE_LEN macros give the packed wire size).
        let sizes_aligns = [
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
                64,
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
                152,
                core::mem::align_of::<MountRecord>(),
                4,
            ),
            (
                "ResourceLimitRecord",
                core::mem::size_of::<ResourceLimitRecord>(),
                32,
                core::mem::align_of::<ResourceLimitRecord>(),
                8,
            ),
        ];
        for (name, size, want_size, align, want_align) in sizes_aligns {
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
        let h = body("rustos_driver.h");
        assert!(h.contains("#ifndef ROS_DRIVER_H"), "guard present");
        assert!(h.contains("#include <stdint.h>"), "stdint included");
        // Reuses the syscall-table-hash length from the manifest header (no
        // re-declaration;).
        assert!(
            h.contains("#include \"rustos_manifest.h\""),
            "manifest header included for ROS_SYSCALL_TABLE_HASH_LEN: {h}"
        );
        assert!(
            h.contains("typedef struct ros_driver_manifest {"),
            "manifest struct mirror: {h}"
        );
        // The bind-table entry embeds the hwtree match key, so the header
        // pulls in rustos_hwtree.h (: no re-declaration).
        assert!(
            h.contains("#include \"rustos_hwtree.h\""),
            "hwtree header included for ros_hw_match_key_t: {h}"
        );
        assert!(
            h.contains("typedef struct ros_driver_bind_key {"),
            "bind-key struct mirror: {h}"
        );
        // Values are read from lib/abi, never re-typed: assert they match.
        let expected = [
            format!("#define ROS_DRIVER_MANIFEST_MAGIC {DRIVER_MANIFEST_MAGIC:#x}u"),
            format!(
                "#define ROS_DRIVER_MANIFEST_MAX_CAPABILITIES {DRIVER_MANIFEST_MAX_CAPABILITIES}u"
            ),
            format!("#define ROS_DRIVER_MANIFEST_MAX_BIND_KEYS {DRIVER_MANIFEST_MAX_BIND_KEYS}u"),
            format!("#define ROS_DRIVER_SIGNER_PUBKEY_LEN {DRIVER_SIGNER_PUBKEY_LEN}u"),
            format!("#define ROS_DRIVER_SIGNATURE_LEN {DRIVER_SIGNATURE_LEN}u"),
            format!(
                "#define ROS_DRIVER_MANIFEST_WIRE_LEN {}u",
                DriverManifest::WIRE_LEN
            ),
            format!(
                "#define ROS_DRIVER_BIND_KEY_WIRE_LEN {}u",
                DriverBindKey::WIRE_LEN
            ),
            format!(
                "#define ROS_DRIVER_KIND_USER_SPACE ((uint8_t){}u)",
                DriverKind::UserSpace.as_u8()
            ),
            format!(
                "#define ROS_DRIVER_KIND_IN_KERNEL ((uint8_t){}u)",
                DriverKind::InKernel.as_u8()
            ),
            format!(
                "#define ROS_BUFFER_CLASS_NON_SENSITIVE ((uint8_t){}u)",
                BufferClass::NonSensitive.as_u8()
            ),
            format!(
                "#define ROS_BUFFER_CLASS_SENSITIVE ((uint8_t){}u)",
                BufferClass::Sensitive.as_u8()
            ),
            format!(
                "#define ROS_DRIVER_HANDLE_NONE ((uint64_t){}ull)",
                DriverHandle::NONE.as_u64()
            ),
            format!(
                "#define ROS_DRIVER_ERROR_PERMISSION_DENIED ((int32_t){})",
                DriverError::PermissionDenied.as_i32()
            ),
            format!(
                "#define ROS_DRIVER_ERROR_NO_SPACE ((int32_t){})",
                DriverError::NoSpace.as_i32()
            ),
            format!("#define ROS_DRIVER_REGISTER_REPLY_MAGIC {DRIVER_REGISTER_REPLY_MAGIC:#x}u"),
            format!("#define ROS_DRIVER_REGISTER_STATUS_OK ((int32_t){DRIVER_REGISTER_STATUS_OK})"),
            format!(
                "#define ROS_DRIVER_REGISTER_REPLY_WIRE_LEN {}u",
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
        use rustos_abi::driver::display::DisplayFormat;
        use rustos_abi::driver::filesystem::{MountFlags, NodeId, NodeKind};
        use rustos_abi::driver::input::InputEventKind;
        use rustos_abi::driver::net::MAC_ADDRESS_LEN;
        use rustos_abi::{VIRTIO_PCI_CFG_COMMON, VIRTIO_PCI_CFG_PCI, VIRTIO_PCI_VENDOR_ID};

        let h = body("rustos_driver.h");
        // NodeTimes mirrors ros_time64_t, so the header must pull in time.
        assert!(
            h.contains("#include \"rustos_time.h\""),
            "time header included for ros_time64_t: {h}"
        );
        // Every value/discriminant is read from lib/abi, never re-typed.
        let expected = [
            format!("#define ROS_VIRTIO_PCI_VENDOR_ID ((uint16_t){VIRTIO_PCI_VENDOR_ID:#x}u)"),
            format!("#define ROS_VIRTIO_PCI_CFG_COMMON ((uint8_t){VIRTIO_PCI_CFG_COMMON}u)"),
            format!("#define ROS_VIRTIO_PCI_CFG_PCI ((uint8_t){VIRTIO_PCI_CFG_PCI}u)"),
            format!("#define ROS_MAC_ADDRESS_LEN {MAC_ADDRESS_LEN}u"),
            format!(
                "#define ROS_MOUNT_FLAG_READ_ONLY ((uint32_t){:#x}u)",
                MountFlags::READ_ONLY.bits()
            ),
            format!(
                "#define ROS_MOUNT_FLAG_NOEXEC ((uint32_t){:#x}u)",
                MountFlags::NOEXEC.bits()
            ),
            format!(
                "#define ROS_MOUNT_FLAG_KNOWN_MASK ((uint32_t){:#x}u)",
                MountFlags::KNOWN_MASK.bits()
            ),
            format!(
                "#define ROS_NODE_ID_NONE ((uint64_t){}ull)",
                NodeId::NONE.raw()
            ),
            format!(
                "#define ROS_DISPLAY_FORMAT_RGBA8888 ((uint8_t){}u)",
                DisplayFormat::Rgba8888.as_u8()
            ),
            format!(
                "#define ROS_DISPLAY_FORMAT_BGRA8888 ((uint8_t){}u)",
                DisplayFormat::Bgra8888.as_u8()
            ),
            format!(
                "#define ROS_NODE_KIND_DIRECTORY ((uint8_t){}u)",
                NodeKind::Directory as u8
            ),
            format!(
                "#define ROS_NODE_KIND_REGULAR_FILE ((uint8_t){}u)",
                NodeKind::RegularFile as u8
            ),
            format!(
                "#define ROS_INPUT_EVENT_KIND_KEY ((uint8_t){}u)",
                InputEventKind::Key.as_u8()
            ),
            format!(
                "#define ROS_INPUT_EVENT_KIND_SCROLL ((uint8_t){}u)",
                InputEventKind::Scroll.as_u8()
            ),
        ];
        for line in &expected {
            assert!(h.contains(line), "missing `{line}` in:\n{h}");
        }
        // The driver input-event kinds must stay disjoint from the windowing
        // input kinds in rustos_input.h (different ABIs).
        assert!(
            !body("rustos_input.h").contains("ROS_INPUT_EVENT_KIND_"),
            "driver input-event kinds must not leak into rustos_input.h"
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
        use rustos_abi::driver::block::{BlockGeometry, DiscardCapability, HealthSnapshot};
        use rustos_abi::driver::bus::BusDevice;
        use rustos_abi::driver::display::{AccelCaps, DisplayMode};
        use rustos_abi::driver::filesystem::{DirEntry, NodeInfo, NodeTimes};
        use rustos_abi::driver::input::InputEvent;
        use rustos_abi::driver::net::MacAddress;
        use rustos_abi::{
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
            ("rustos_time.h", "} ros_time64_t;", size_of::<Time64>(), 16, align_of::<Time64>(), 8),
            ("rustos_time.h", "} ros_duration64_t;", size_of::<Duration64>(), 16, align_of::<Duration64>(), 8),
            ("rustos_ipc.h", "} ros_ipc_message_header_t;", size_of::<IpcMessageHeader>(), 32, align_of::<IpcMessageHeader>(), 8),
            ("rustos_ipc.h", "} ros_port_name_t;", size_of::<PortName>(), 32, align_of::<PortName>(), 1),
            ("rustos_manifest.h", "} ros_manifest_header_t;", size_of::<ManifestHeader>(), 144, align_of::<ManifestHeader>(), 4),
            ("rustos_appinfo.h", "} ros_appinfo_header_t;", size_of::<AppInfoHeader>(), 340, align_of::<AppInfoHeader>(), 4),
            ("rustos_rxe.h", "} ros_load_header_t;", size_of::<LoadHeader>(), 56, align_of::<LoadHeader>(), 8),
            ("rustos_process.h", "} ros_process_start_header_t;", size_of::<ProcessStartHeader>(), 32, align_of::<ProcessStartHeader>(), 8),
            ("rustos_process.h", "} ros_string_slot_t;", size_of::<StringSlot>(), 8, align_of::<StringSlot>(), 4),
            ("rustos_sysinfo.h", "} ros_sysinfo_request_header_t;", size_of::<SysinfoRequestHeader>(), 24, align_of::<SysinfoRequestHeader>(), 8),
            ("rustos_sysinfo.h", "} ros_process_list_request_t;", size_of::<ProcessListRequest>(), 8, align_of::<ProcessListRequest>(), 4),
            ("rustos_sysinfo.h", "} ros_process_record_t;", size_of::<ProcessRecord>(), 64, align_of::<ProcessRecord>(), 8),
            ("rustos_sysinfo.h", "} ros_kernel_memory_stats_t;", size_of::<KernelMemoryStats>(), 40, align_of::<KernelMemoryStats>(), 8),
            ("rustos_sysinfo.h", "} ros_uptime_t;", size_of::<Uptime>(), 32, align_of::<Uptime>(), 8),
            ("rustos_sysinfo.h", "} ros_system_identity_t;", size_of::<SystemIdentity>(), 88, align_of::<SystemIdentity>(), 2),
            ("rustos_sysinfo.h", "} ros_mount_list_request_t;", size_of::<MountListRequest>(), 8, align_of::<MountListRequest>(), 4),
            ("rustos_sysinfo.h", "} ros_mount_record_t;", size_of::<MountRecord>(), 152, align_of::<MountRecord>(), 4),
            ("rustos_driver.h", "} ros_driver_manifest_t;", size_of::<DriverManifest>(), 140, align_of::<DriverManifest>(), 4),
            ("rustos_driver.h", "} ros_driver_bind_key_t;", size_of::<DriverBindKey>(), 80, align_of::<DriverBindKey>(), 4),
            ("rustos_driver.h", "} ros_driver_register_reply_t;", size_of::<DriverRegisterReply>(), 24, align_of::<DriverRegisterReply>(), 8),
            ("rustos_driver.h", "} ros_block_geometry_t;", size_of::<BlockGeometry>(), 16, align_of::<BlockGeometry>(), 8),
            ("rustos_driver.h", "} ros_discard_capability_t;", size_of::<DiscardCapability>(), 24, align_of::<DiscardCapability>(), 8),
            ("rustos_driver.h", "} ros_health_snapshot_t;", size_of::<HealthSnapshot>(), 64, align_of::<HealthSnapshot>(), 8),
            ("rustos_driver.h", "} ros_bus_device_t;", size_of::<BusDevice>(), 24, align_of::<BusDevice>(), 8),
            ("rustos_driver.h", "} ros_display_mode_t;", size_of::<DisplayMode>(), 16, align_of::<DisplayMode>(), 4),
            ("rustos_driver.h", "} ros_accel_caps_t;", size_of::<AccelCaps>(), 16, align_of::<AccelCaps>(), 4),
            ("rustos_driver.h", "} ros_node_info_t;", size_of::<NodeInfo>(), 16, align_of::<NodeInfo>(), 8),
            ("rustos_driver.h", "} ros_dir_entry_t;", size_of::<DirEntry>(), 24, align_of::<DirEntry>(), 8),
            ("rustos_driver.h", "} ros_node_times_t;", size_of::<NodeTimes>(), 64, align_of::<NodeTimes>(), 8),
            ("rustos_driver.h", "} ros_input_event_t;", size_of::<InputEvent>(), 8, align_of::<InputEvent>(), 4),
            ("rustos_driver.h", "} ros_mac_address_t;", size_of::<MacAddress>(), 6, align_of::<MacAddress>(), 1),
            ("rustos_rlimit.h", "} ros_resource_limit_t;", size_of::<ResourceLimit>(), 16, align_of::<ResourceLimit>(), 8),
            ("rustos_sysinfo.h", "} ros_resource_limit_record_t;", size_of::<ResourceLimitRecord>(), 32, align_of::<ResourceLimitRecord>(), 8),
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
        let h = body("rustos_syscall.h");
        for spec in SYSCALLS {
            let upper = spec.name.to_ascii_uppercase();
            assert!(
                h.contains(&format!("#define ROS_SYS_{upper} ")),
                "missing number macro for {}",
                spec.name
            );
            assert!(
                h.contains(&format!("ros_sys_{}(", spec.name)),
                "missing prototype for {}",
                spec.name
            );
        }
    }

    /// The C errno table must mirror the frozen [`Errno`] enum exactly: a
    /// dense `1..=N` numbering with no gaps, so appending a new `Errno`
    /// variant without listing it here fails this test rather than silently
    /// dropping it from the header.
    #[test]
    fn errno_table_matches_the_frozen_enum() {
        for (idx, (_name, errno)) in ERRNO_NAMES.iter().enumerate() {
            let expected = i32::try_from(idx + 1).expect("small index");
            assert_eq!(errno.as_i32(), expected, "errno values must be dense 1..=N");
        }
        // OutOfMemory is the last appended abi-v1 variant (discriminant 20).
        assert_eq!(
            ERRNO_NAMES.last().map(|(_, e)| e.as_i32()),
            Some(Errno::OutOfMemory.as_i32()),
            "errno table must end at the last frozen variant"
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
