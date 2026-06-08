//! Build-time derivation of [`SYSCALL_TABLE_HASH`].
//!
//! `AGENTS.md` §9 makes `rustos_abi::ENCODED_TABLE` the single source of
//! truth for the `abi-v1` syscall table, and §2.2 forbids a second,
//! hand-maintained definition of anything derived from it. The table's
//! SHA-256 fingerprint is exactly such a derived value, so it is computed
//! here at build time rather than committed as a literal: there is no
//! constant for anyone to hand-edit, and any change to the table
//! re-derives the fingerprint on the next build. The generated expression
//! is `include!`d by `src/table.rs`.
//!
//! Running on the host, this script depends on `rustos-abi` and
//! `rustos-crypto` as `[build-dependencies]`; both are the same path
//! crates the library half links, so the digest computed here is the
//! digest the kernel sees. `verify_table_hash` and `cargo xtask abi-check`
//! still recompute and cross-check the linked value as defence in depth.

use std::env;
use std::fmt::Write as _;
use std::fs;
use std::path::Path;

use rustos_abi::ENCODED_TABLE;
use rustos_crypto::sha256;

fn main() {
    let digest = sha256(&ENCODED_TABLE);

    let mut body = String::with_capacity(digest.len() * 6 + 4);
    body.push_str("[\n");
    for (i, byte) in digest.iter().enumerate() {
        if i % 8 == 0 {
            body.push_str("    ");
        }
        let _ = write!(body, "0x{byte:02x}, ");
        if i % 8 == 7 {
            body.push('\n');
        }
    }
    body.push(']');

    let out_dir = env::var("OUT_DIR").expect("OUT_DIR is set by cargo for build scripts");
    let path = Path::new(&out_dir).join("syscall_table_hash.rs");
    fs::write(&path, body).expect("write generated syscall-table-hash expression");
}
