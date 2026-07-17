//! `cargo xtask abi-check` implementation (Stage 2.7 of `PLAN.md`).
//!
//! Per the kernel-side dispatch table
//! (`kernel/syscall/src/table.rs`) is generated from the source-of-truth
//! syscall list (`lib/abi/src/syscalls.rs`). `abi-check` enforces that
//! contract in two independent ways so a divergence cannot survive a
//! `cargo xtask ci` run:
//!
//! 1. **Pair existence.** Both files must be present. Adding one
//!    without the other is a hard error — the very situation that
//!    motivated the watch logic added in Stage 0.
//! 2. **Hash cross-check.** `SYSCALL_TABLE_HASH` is no longer a
//!    hand-maintained literal — `kernel/syscall/build.rs` derives it
//!    from `tairix_abi::ENCODED_TABLE` at build time, so there is
//!    nothing on disk to parse or to drift. This check recomputes
//!    SHA-256 of `tairix_abi::ENCODED_TABLE` here and compares it to the
//!    *linked* `tairix_kernel_syscall::SYSCALL_TABLE_HASH`, catching the
//!    pathological case where the workspace is being built against a
//!    stale crate cache or a mismatched `tairix-abi`.
//!
//! The check is intentionally implemented in ordinary Rust without
//! spawning sub-processes.

use std::path::Path;

use tairix_abi::ENCODED_TABLE;
use tairix_crypto::{sha256, Sha256Digest};

/// Default on-disk location of the `lib/abi` half of the cross-check,
/// relative to the workspace root.
pub const DEFAULT_SYSCALLS_PATH: &str = "lib/abi/src/syscalls.rs";
/// Default on-disk location of the kernel half of the cross-check,
/// relative to the workspace root.
pub const DEFAULT_TABLE_PATH: &str = "kernel/syscall/src/table.rs";

/// Run the full ABI cross-check against an explicit pair of source
/// files.
///
/// `workspace_root` is recorded in error messages so a developer can
/// `cd` directly to the offending file. The remaining two paths point
/// at the two halves of the contract — callers default them to the
/// constants above; the integration tests substitute fixtures from a
/// temporary directory to verify both the positive and negative paths
/// (Stage 2.7 deliverable).
pub fn check_sync(
    workspace_root: &Path,
    syscalls_path: &Path,
    table_path: &Path,
) -> Result<(), String> {
    let syscalls_exists = syscalls_path.exists();
    let table_exists = table_path.exists();
    match (syscalls_exists, table_exists) {
        (false, false) => Err(format!(
            "abi-check: neither `{}` nor `{}` exists; \
             both halves of the syscall contract must ship together (AGENTS.md §9).",
            relative(workspace_root, syscalls_path),
            relative(workspace_root, table_path),
        )),
        (true, false) | (false, true) => Err(format!(
            "abi-check: `{}` and `{}` must be added together (AGENTS.md §9); \
             one half is missing.",
            relative(workspace_root, syscalls_path),
            relative(workspace_root, table_path),
        )),
        (true, true) => verify_hash(),
    }
}

/// Cross-check the linked kernel `SYSCALL_TABLE_HASH` against a freshly
/// computed SHA-256 of `tairix_abi::ENCODED_TABLE`.
///
/// The kernel constant is derived from `ENCODED_TABLE` at build time
/// (`kernel/syscall/build.rs`), so this can only diverge if the linked
/// `tairix-abi` differs from the one this command links — e.g. a stale
/// `target/` cache or a mismatched dependency graph.
fn verify_hash() -> Result<(), String> {
    let expected: Sha256Digest = sha256(&ENCODED_TABLE);
    if tairix_kernel_syscall::SYSCALL_TABLE_HASH != expected {
        return Err(format!(
            "abi-check: linked `tairix_kernel_syscall::SYSCALL_TABLE_HASH` \
             does not match sha256(tairix_abi::ENCODED_TABLE).\n  linked : {}\n  expected: {}",
            hex(&tairix_kernel_syscall::SYSCALL_TABLE_HASH),
            hex(&expected),
        ));
    }

    Ok(())
}

fn relative(base: &Path, path: &Path) -> String {
    path.strip_prefix(base)
        .unwrap_or(path)
        .display()
        .to_string()
}

fn hex(bytes: &[u8]) -> String {
    let mut out = String::with_capacity(bytes.len() * 2);
    for b in bytes {
        // The `unwrap` is on `write!` into a `String`; infallible.
        use std::fmt::Write as _;
        let _ = write!(out, "{b:02x}");
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    fn workspace_root() -> std::path::PathBuf {
        // CARGO_MANIFEST_DIR points at tools/xtask; the workspace root
        // is its great-grandparent.
        let mut p = std::path::PathBuf::from(env!("CARGO_MANIFEST_DIR"));
        p.pop(); // tools
        p.pop(); // workspace
        p
    }

    #[test]
    fn positive_check_passes_against_real_sources() {
        let root = workspace_root();
        let syscalls = root.join(DEFAULT_SYSCALLS_PATH);
        let table = root.join(DEFAULT_TABLE_PATH);
        check_sync(&root, &syscalls, &table).expect("real sources must agree");
    }

    #[test]
    fn missing_half_is_an_error() {
        let root = workspace_root();
        let syscalls = root.join(DEFAULT_SYSCALLS_PATH);
        let absent = root.join("kernel/syscall/src/__nope__.rs");
        let err = check_sync(&root, &syscalls, &absent).unwrap_err();
        assert!(err.contains("must be added together"), "{err}");
    }

    #[test]
    fn both_missing_is_an_error() {
        let root = workspace_root();
        let a = root.join("__nope_a__.rs");
        let b = root.join("__nope_b__.rs");
        let err = check_sync(&root, &a, &b).unwrap_err();
        assert!(err.contains("both halves"), "{err}");
    }

    #[test]
    fn linked_hash_matches_encoded_table() {
        // The build-time-derived kernel constant must equal a freshly
        // computed digest of the source-of-truth table. This is the
        // structural guarantee that replaces the old hand-maintained
        // literal: there is nothing to edit, so the only way these can
        // diverge is a stale cache / mismatched `tairix-abi`.
        verify_hash().expect("linked SYSCALL_TABLE_HASH must match sha256(ENCODED_TABLE)");
        assert_eq!(
            tairix_kernel_syscall::SYSCALL_TABLE_HASH,
            sha256(&ENCODED_TABLE),
        );
    }
}
