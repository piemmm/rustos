//! Build script: enable the `freestanding` cfg when the mem-map fixture
//! program is built for a bare-metal target (`target_os = "none"`), so
//! `src/main.rs` compiles as a freestanding pure-Rust program there and as an
//! inert host stub everywhere else (mirrors `el0_yielder_program/build.rs`).
//!
//! It is deliberately self-contained — it does not depend on the
//! `tests/integration` harness — and keys only off the OS component of the
//! target (bare-metal vs hosted), never the instruction set, so `cargo xtask
//! cfg-check` (`AGENTS.md` §17.2) stays clean.
//!
//! The consuming vertical (`mem_map_qemu_aarch64`) sets `RUSTOS_MEM_MAP_ADDR`
//! and `RUSTOS_MEM_MAP_LEN` when it compiles this program, so a changed region
//! must force a recompile; declare those dependencies here.

fn main() {
    println!("cargo:rustc-check-cfg=cfg(freestanding)");
    let target_os = std::env::var("CARGO_CFG_TARGET_OS").unwrap_or_default();
    if target_os == "none" {
        println!("cargo:rustc-cfg=freestanding");
    }
    println!("cargo:rerun-if-env-changed=RUSTOS_MEM_MAP_ADDR");
    println!("cargo:rerun-if-env-changed=RUSTOS_MEM_MAP_LEN");
    println!("cargo:rerun-if-changed=build.rs");
}
