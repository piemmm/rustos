//! `cargo xtask abi-check` implementation (Stage 2.7 of `PLAN.md`).
//!
//! Per `AGENTS.md` §9 the kernel-side dispatch table
//! (`kernel/syscall/src/table.rs`) is generated from the source-of-truth
//! syscall list (`lib/abi/src/syscalls.rs`). `abi-check` enforces that
//! contract in two independent ways so a divergence cannot survive a
//! `cargo xtask ci` run:
//!
//! 1. **Pair existence.** Both files must be present. Adding one
//!    without the other is a hard error — the very situation that
//!    motivated the watch logic added in Stage 0.
//! 2. **Hash cross-check.** The kernel-side `SYSCALL_TABLE_HASH`
//!    literal is parsed from the on-disk table source, SHA-256 of
//!    `rustos_abi::ENCODED_TABLE` is recomputed here, and the two are
//!    compared byte for byte. A second comparison against the
//!    *linked* `rustos_kernel_syscall::SYSCALL_TABLE_HASH` catches the
//!    pathological case where the kernel source was edited but the
//!    workspace is being built against a stale crate cache.
//!
//! The check is intentionally implemented in ordinary Rust without
//! spawning sub-processes: a future ABI change that forgets to update
//! the hash literal fails `cargo build` long before it reaches CI.

use std::path::Path;

use rustos_abi::ENCODED_TABLE;
use rustos_crypto::{sha256, Sha256Digest, SHA256_OUTPUT_LEN};

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
        (true, true) => verify_hash(workspace_root, table_path),
    }
}

fn verify_hash(workspace_root: &Path, table_path: &Path) -> Result<(), String> {
    let table_src = std::fs::read_to_string(table_path)
        .map_err(|e| format!("abi-check: cannot read {}: {e}", table_path.display()))?;
    let on_disk_hash = parse_table_hash_literal(&table_src).map_err(|reason| {
        format!(
            "abi-check: could not extract SYSCALL_TABLE_HASH from {}: {reason}",
            relative(workspace_root, table_path),
        )
    })?;

    let expected: Sha256Digest = sha256(&ENCODED_TABLE);
    if on_disk_hash != expected {
        return Err(format!(
            "abi-check: SYSCALL_TABLE_HASH in `{}` does not match \
             sha256(rustos_abi::ENCODED_TABLE).\n  on-disk : {}\n  expected: {}",
            relative(workspace_root, table_path),
            hex(&on_disk_hash),
            hex(&expected),
        ));
    }

    // Defence in depth: the *linked* kernel constant must also agree.
    // Catches the pathological "table.rs was edited but the workspace
    // is being built against a stale `target/` cache" case.
    if rustos_kernel_syscall::SYSCALL_TABLE_HASH != expected {
        return Err(format!(
            "abi-check: linked `rustos_kernel_syscall::SYSCALL_TABLE_HASH` \
             does not match sha256(rustos_abi::ENCODED_TABLE).\n  linked : {}\n  expected: {}",
            hex(&rustos_kernel_syscall::SYSCALL_TABLE_HASH),
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

/// Parse the byte literal of `pub const SYSCALL_TABLE_HASH: ... = [
/// ... ];` from the kernel-table source code.
///
/// Returns the 32-byte digest, or a human-readable reason for failure.
/// The parser is intentionally strict: it accepts only the exact shape
/// the file ships with (a single byte-array literal containing 32
/// `0xNN` entries) so a silent reformatting of the constant cannot
/// produce a "still parses but means nothing" false positive.
pub(crate) fn parse_table_hash_literal(source: &str) -> Result<Sha256Digest, &'static str> {
    let needle = "SYSCALL_TABLE_HASH";
    let Some(name_start) = source.find(needle) else {
        return Err("SYSCALL_TABLE_HASH identifier not found");
    };
    let after_name = &source[name_start + needle.len()..];
    let Some(eq_pos) = after_name.find('=') else {
        return Err("`=` not found after SYSCALL_TABLE_HASH");
    };
    let body = &after_name[eq_pos + 1..];
    let Some(open) = body.find('[') else {
        return Err("opening `[` not found");
    };
    let body = &body[open + 1..];
    let Some(close) = body.find(']') else {
        return Err("closing `]` not found");
    };
    let literal = &body[..close];

    let mut bytes = [0u8; SHA256_OUTPUT_LEN];
    let mut idx = 0;
    for token in literal.split(',') {
        let trimmed = token.trim();
        if trimmed.is_empty() {
            continue;
        }
        if idx >= SHA256_OUTPUT_LEN {
            return Err("hash literal contains too many bytes");
        }
        let stripped = trimmed
            .strip_prefix("0x")
            .or_else(|| trimmed.strip_prefix("0X"))
            .ok_or("hash byte missing `0x` prefix")?;
        bytes[idx] = u8::from_str_radix(stripped, 16).map_err(|_| "hash byte is not valid hex")?;
        idx += 1;
    }
    if idx != SHA256_OUTPUT_LEN {
        return Err("hash literal does not contain exactly 32 bytes");
    }
    Ok(bytes)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;

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
    fn desync_in_table_hash_is_detected() {
        let root = workspace_root();
        let syscalls = root.join(DEFAULT_SYSCALLS_PATH);
        let original = root.join(DEFAULT_TABLE_PATH);

        // Mutate one byte of the hash literal and write the result
        // somewhere outside the source tree. We isolate the per-run
        // directory under `target/tmp` (the workspace's own scratch
        // area) so a failed test does not leak files into `/tmp`.
        let tmp = root
            .join("target")
            .join("tmp")
            .join("xtask_abi_check_desync");
        fs::create_dir_all(&tmp).expect("tmpdir");
        let mutated_path = tmp.join("table.rs");
        let original_src = fs::read_to_string(&original).expect("read original");
        // Locate the SYSCALL_TABLE_HASH literal, then flip the first
        // hex byte token after it. This is robust against any future
        // refresh of the hash content (AGENTS.md §7 — no flaky
        // tests; a previous version of this fixture hard-coded a
        // specific byte that drifted out of the hash and silently
        // broke the test).
        let anchor_pos = original_src
            .find("SYSCALL_TABLE_HASH")
            .expect("anchor present");
        let first_byte_pos = anchor_pos
            + original_src[anchor_pos..]
                .find("0x")
                .expect("hash literal has at least one 0x byte");
        let token = &original_src[first_byte_pos..first_byte_pos + 4];
        // Token is `0xHH`; we flip the low nibble by one (wrapping)
        // so the substitution is guaranteed to change the literal
        // and to remain a valid hex byte.
        let mut bytes = token.as_bytes().to_vec();
        bytes[3] = match bytes[3] {
            b'0'..=b'8' | b'a'..=b'e' | b'A'..=b'E' => bytes[3] + 1,
            b'9' => b'a',
            b'f' | b'F' => b'0',
            other => panic!("unexpected hex char {other:#x}"),
        };
        let replacement = core::str::from_utf8(&bytes).expect("ascii");
        let mutated_src = format!(
            "{}{}{}",
            &original_src[..first_byte_pos],
            replacement,
            &original_src[first_byte_pos + 4..],
        );
        assert_ne!(
            original_src, mutated_src,
            "fixture mutation must change the source"
        );
        fs::write(&mutated_path, &mutated_src).expect("write mutated");

        let err = check_sync(&root, &syscalls, &mutated_path).unwrap_err();
        assert!(
            err.contains("does not match"),
            "expected hash-mismatch error, got: {err}"
        );
    }

    #[test]
    fn parser_extracts_exactly_thirty_two_bytes() {
        // Hand-rolled fixture so the parser is exercised independently
        // of the live source file.
        let synthetic = "
            pub const SYSCALL_TABLE_HASH: [u8; 32] = [
                0x00, 0x01, 0x02, 0x03, 0x04, 0x05, 0x06, 0x07,
                0x08, 0x09, 0x0a, 0x0b, 0x0c, 0x0d, 0x0e, 0x0f,
                0x10, 0x11, 0x12, 0x13, 0x14, 0x15, 0x16, 0x17,
                0x18, 0x19, 0x1a, 0x1b, 0x1c, 0x1d, 0x1e, 0x1f,
            ];
        ";
        let bytes = parse_table_hash_literal(synthetic).expect("parse synthetic");
        for (i, b) in bytes.iter().enumerate() {
            assert_eq!(usize::from(*b), i);
        }
    }

    #[test]
    fn parser_rejects_truncated_literal() {
        let synthetic = "
            pub const SYSCALL_TABLE_HASH: [u8; 32] = [
                0x00, 0x01,
            ];
        ";
        let err = parse_table_hash_literal(synthetic).unwrap_err();
        assert!(err.contains("32 bytes"), "{err}");
    }
}
