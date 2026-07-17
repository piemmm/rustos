//! Build script: enable the `freestanding` cfg when the file-mapping fixture
//! program is built for a bare-metal target (`target_os = "none"`), so
//! `src/main.rs` compiles as a freestanding pure-Rust program there and as an
//! inert host stub everywhere else (mirrors `tests/integration/wait_program`).
//!
//! It is deliberately self-contained — it does not depend on the
//! `tests/integration` harness — and keys only off the OS component of the
//! target (bare-metal vs hosted), never the instruction set, so `cargo xtask
//! cfg-check` stays clean.
//!
//! The consuming verticals (`file_map_qemu_aarch64` / `…_riscv64`) pin the
//! fixture geometry through the `TAIRIX_FM_*` environment variables when they
//! compile this program, so a changed geometry must force a recompile;
//! declare those dependencies here.

/// The fixture-geometry environment variables the consuming vertical pins.
const GEOMETRY_ENV: &[&str] = &[
    "TAIRIX_FM_FILE_LEN",
    "TAIRIX_FM_PATH",
    "TAIRIX_FM_PATH_OFFSET",
    "TAIRIX_FM_INTERIOR_OFFSET",
    "TAIRIX_FM_BYTE_FIRST",
    "TAIRIX_FM_BYTE_INTERIOR",
    "TAIRIX_FM_BYTE_LAST",
];

fn main() {
    println!("cargo:rustc-check-cfg=cfg(freestanding)");
    let target_os = std::env::var("CARGO_CFG_TARGET_OS").unwrap_or_default();
    if target_os == "none" {
        println!("cargo:rustc-cfg=freestanding");
    }
    for name in GEOMETRY_ENV {
        println!("cargo:rerun-if-env-changed={name}");
    }
    println!("cargo:rerun-if-changed=build.rs");
}
