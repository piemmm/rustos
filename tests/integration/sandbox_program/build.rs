//! Build script: enable the `freestanding` cfg when the parser-sandbox
//! fixture program is built for a bare-metal target (`target_os = "none"`),
//! so `src/main.rs` compiles as a freestanding pure-Rust program there and
//! as an inert host stub everywhere else (mirrors
//! `tests/integration/file_map_program`).
//!
//! It is deliberately self-contained — it does not depend on the
//! `tests/integration` harness — and keys only off the OS component of the
//! target (bare-metal vs hosted), never the instruction set, so `cargo
//! xtask cfg-check` stays clean.

fn main() {
    println!("cargo:rustc-check-cfg=cfg(freestanding)");
    let target_os = std::env::var("CARGO_CFG_TARGET_OS").unwrap_or_default();
    if target_os == "none" {
        println!("cargo:rustc-cfg=freestanding");
    }
    println!("cargo:rerun-if-changed=build.rs");
}
