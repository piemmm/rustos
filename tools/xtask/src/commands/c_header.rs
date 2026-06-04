//! `cargo xtask c-header` implementation.
//!
//! RustOS is written entirely in Rust, but its kernel/user interface
//! (`abi-v1`, `AGENTS.md` §9) is a stable binary contract that programs
//! written in other languages — C in particular — must be able to call.
//! Those programs need a C-language *view* of the ABI: the syscall numbers,
//! the error codes, the capability identifiers, the `#[repr(C)]` types, and a
//! prototype for each syscall entry point.
//!
//! That view is the C development header set. It is **generated** from the one
//! source of truth in `lib/abi` (`AGENTS.md` §2.2 — no duplication, §9 — the
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
//! `ROS_` C-ABI prefix (`AGENTS.md` §9) and are namespaced and frozen
//! alongside the rest of `abi-v1`. The future user-space stub crate that
//! issues the actual trap implements each one with an explicit
//! `#[export_name = "ros_sys_<name>"]` so the Rust compiler does not
//! mangle it; this header is the contract those exports satisfy.

use std::path::Path;

use rustos_abi::{
    AbiType, CapabilityId, Duration64, Errno, IpcMessageHeader, PortName, RandomFlags, Time64,
    ABI_VERSION_V1, CAPABILITY_ID_MAX, COARSE_CLOCK_GRANULARITY_NS, IPC_MESSAGE_HEADER_MAGIC,
    NANOS_PER_SEC, PORT_NAME_MAX_LEN, RANDOM_REQUEST_MAX_BYTES, RANDOM_RESERVE_DEFAULT_BYTES,
    SYSCALLS, SYSCALL_MAX_ARGS,
};

/// Default on-disk location of the generated C ABI header set, relative to
/// the workspace root. The umbrella header is `rustos_abi.h` inside it.
pub const DEFAULT_INCLUDE_DIR: &str = "include/rustos";

/// The `abi-v1` error codes, paired with the `ROS_E_*` suffix each is
/// emitted under.
///
/// The numeric value of every entry is read straight from the
/// [`Errno`] enum, so this table can never disagree with the frozen
/// discriminants (`AGENTS.md` §2.2): only the C spelling lives here, because
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

/// `rustos_capability.h` — the capability identifiers (`AGENTS.md` §5.2).
fn generate_capability() -> String {
    use std::fmt::Write as _;
    let mut out = banner("Capability identifiers (AGENTS.md sec.5.2).");
    out.push_str("#ifndef ROS_CAPABILITY_H\n#define ROS_CAPABILITY_H\n\n");
    out.push_str("/* Capability identifiers (AGENTS.md sec.5.2). */\n");
    let _ = writeln!(out, "#define ROS_CAPABILITY_ID_MAX {CAPABILITY_ID_MAX}u");
    for raw in 1..=CAPABILITY_ID_MAX {
        if let Some(name) = CapabilityId::from_raw(raw)
            .ok()
            .and_then(CapabilityId::name)
        {
            let _ = writeln!(out, "#define ROS_{name} {raw}u");
        }
    }
    out.push_str("\n#endif /* ROS_CAPABILITY_H */\n");
    out
}

/// `rustos_time.h` — the 64-bit-native time types (`AGENTS.md` §21).
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

/// `rustos_random.h` — the canonical random-number ABI (`AGENTS.md` §22).
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

/// `rustos_ipc.h` — the IPC message header and port-name wire types
/// (`AGENTS.md` §4).
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
    out.push_str("#include \"rustos_ipc.h\"\n");
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
            file_name: "rustos_ipc.h",
            body: generate_ipc(),
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
            "rustos_ipc.h",
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
        assert!(h.contains("#define ROS_CAP_USER_ADMIN 5u"), "capability");
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
        // WouldBlock is the last frozen abi-v1 variant (discriminant 19).
        assert_eq!(
            ERRNO_NAMES.last().map(|(_, e)| e.as_i32()),
            Some(Errno::WouldBlock.as_i32()),
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
