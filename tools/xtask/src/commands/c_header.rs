//! `cargo xtask c-header` implementation.
//!
//! RustOS is written entirely in Rust, but its kernel/user interface
//! (`abi-v1`, `AGENTS.md` §9) is a stable binary contract that programs
//! written in other languages — C in particular — must be able to call.
//! Those programs need a C-language *view* of the ABI: the syscall numbers,
//! the error codes, the capability identifiers, and a prototype for each
//! syscall entry point.
//!
//! That view is the C development header. It is **generated** from the one
//! source of truth in `lib/abi` (`AGENTS.md` §2.2 — no duplication, §9 — the
//! ABI is versioned and a C surface is a view of the existing definition,
//! never a hand-maintained parallel one). The committed header lives in its
//! own top-level [`include/`](DEFAULT_HEADER_PATH) folder so it can be handed
//! to developers building non-Rust programs without shipping the whole
//! workspace.
//!
//! Like `abi-check` (`commands/abi_check.rs`), the generator doubles as a
//! drift guard:
//!
//! - `cargo xtask c-header` (no arguments) regenerates the header in memory
//!   and compares it byte for byte with the committed copy, failing closed on
//!   any mismatch. It runs as part of `cargo xtask ci`.
//! - `cargo xtask c-header --write` regenerates the committed copy (reviewed
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
    AbiType, CapabilityId, Errno, ABI_VERSION_V1, CAPABILITY_ID_MAX, SYSCALLS, SYSCALL_MAX_ARGS,
};

/// Default on-disk location of the generated C ABI header, relative to the
/// workspace root.
pub const DEFAULT_HEADER_PATH: &str = "include/rustos/rustos_abi.h";

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

/// Generate the full C ABI header text from the `lib/abi` source of truth.
///
/// The output is deterministic: the same workspace always produces the same
/// bytes, which is what lets [`check_sync`] use a byte-for-byte comparison as
/// a drift guard.
#[must_use]
pub fn generate() -> String {
    use std::fmt::Write as _;
    let mut out = String::with_capacity(4096);

    out.push_str(
        "/*\n\
         * RustOS abi-v1 C development header.\n\
         *\n\
         * GENERATED FILE - DO NOT EDIT BY HAND.\n\
         *\n\
         * This is the C-language view of the RustOS kernel/user ABI. It is\n\
         * generated from the single source of truth in `lib/abi` by\n\
         * `cargo xtask c-header --write` and verified on every CI run by\n\
         * `cargo xtask c-header`. Edit `lib/abi` and regenerate; never edit\n\
         * this file directly (AGENTS.md sec.2.2, sec.9).\n\
         *\n\
         * Each syscall is exported by the user-space stub library under the\n\
         * symbol `ros_sys_<name>` (e.g. `ros_sys_ipc_send`); link\n\
         * against that library to call the kernel from a non-Rust program.\n\
         */\n\n",
    );

    out.push_str("#ifndef ROS_ABI_H\n#define ROS_ABI_H\n\n");
    out.push_str("#include <stdint.h>\n\n");
    out.push_str("#ifdef __cplusplus\nextern \"C\" {\n#endif\n\n");

    // ABI version.
    out.push_str("/* ABI version this header describes (AGENTS.md sec.9). */\n");
    let _ = writeln!(out, "#define ROS_ABI_VERSION {ABI_VERSION_V1}u\n");

    // Error codes.
    out.push_str("/* Stable abi-v1 error codes (int32_t). */\n");
    for (name, errno) in ERRNO_NAMES {
        let _ = writeln!(out, "#define ROS_E_{name} {}", errno.as_i32());
    }
    out.push('\n');

    // Capability identifiers.
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
    out.push('\n');

    // Syscall numbers.
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

    // Syscall stub prototypes.
    out.push_str("/* Syscall entry points, implemented by the user-space stub library. */\n");
    for spec in SYSCALLS {
        let _ = writeln!(out, "{}", prototype(spec));
    }
    out.push('\n');

    out.push_str("#ifdef __cplusplus\n} /* extern \"C\" */\n#endif\n\n");
    out.push_str("#endif /* ROS_ABI_H */\n");

    out
}

/// Verify that the committed header matches freshly generated output.
///
/// `workspace_root` is recorded in error messages; `header_path` points at
/// the committed copy (callers default it to [`DEFAULT_HEADER_PATH`]). A
/// missing or stale header is a hard error directing the developer to
/// `cargo xtask c-header --write`.
pub fn check_sync(workspace_root: &Path, header_path: &Path) -> Result<(), String> {
    let rel = relative(workspace_root, header_path);
    let on_disk = match std::fs::read_to_string(header_path) {
        Ok(text) => text,
        Err(err) if err.kind() == std::io::ErrorKind::NotFound => {
            return Err(format!(
                "c-header: `{rel}` is missing; run `cargo xtask c-header --write` \
                 to generate it from lib/abi (AGENTS.md sec.9)."
            ));
        }
        Err(err) => return Err(format!("c-header: cannot read {rel}: {err}")),
    };

    let expected = generate();
    if on_disk != expected {
        return Err(format!(
            "c-header: `{rel}` is out of date with the lib/abi source of truth; \
             run `cargo xtask c-header --write` and commit the result (AGENTS.md sec.2.2, sec.9)."
        ));
    }
    Ok(())
}

/// Regenerate the committed header at `header_path`, creating any missing
/// parent directories.
pub fn write(workspace_root: &Path, header_path: &Path) -> Result<(), String> {
    if let Some(parent) = header_path.parent() {
        std::fs::create_dir_all(parent).map_err(|e| {
            format!(
                "c-header: cannot create {}: {e}",
                relative(workspace_root, parent)
            )
        })?;
    }
    std::fs::write(header_path, generate()).map_err(|e| {
        format!(
            "c-header: cannot write {}: {e}",
            relative(workspace_root, header_path)
        )
    })?;
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

    #[test]
    fn generation_is_deterministic() {
        assert_eq!(generate(), generate());
    }

    #[test]
    fn header_contains_expected_anchors() {
        let h = generate();
        assert!(h.contains("#ifndef ROS_ABI_H"), "guard present");
        assert!(h.contains("#include <stdint.h>"), "stdint included");
        assert!(h.contains("extern \"C\""), "C++ guard present");
        assert!(h.contains("#define ROS_ABI_VERSION 1u"), "version macro");
        assert!(h.contains("#define ROS_E_PERMISSION_DENIED 6"), "errno");
        assert!(h.contains("#define ROS_CAP_USER_ADMIN 5u"), "capability");
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
    fn every_syscall_has_a_number_and_a_prototype() {
        let h = generate();
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
    fn committed_header_is_in_sync() {
        let root = workspace_root();
        let header = root.join(DEFAULT_HEADER_PATH);
        check_sync(&root, &header).expect("committed header must match lib/abi");
    }

    #[test]
    fn missing_header_is_an_error() {
        let root = workspace_root();
        let absent = root.join("include/rustos/__nope__.h");
        let err = check_sync(&root, &absent).unwrap_err();
        assert!(err.contains("is missing"), "{err}");
    }

    #[test]
    fn stale_header_is_detected() {
        let root = workspace_root();
        // Write a deliberately wrong header under the workspace scratch area
        // (target/tmp) so a failed test never leaks into /tmp.
        let tmp = root.join("target").join("tmp").join("xtask_c_header_stale");
        std::fs::create_dir_all(&tmp).expect("tmpdir");
        let stale = tmp.join("rustos_abi.h");
        std::fs::write(&stale, "/* not the generated header */\n").expect("write stale");
        let err = check_sync(&root, &stale).unwrap_err();
        assert!(err.contains("out of date"), "{err}");
    }
}
