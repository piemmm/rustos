//! Build script: enable the `freestanding` cfg when the wait fixture program
//! is built for a bare-metal target (`target_os = "none"`), so `src/main.rs`
//! compiles as a freestanding pure-Rust program there and as an inert host
//! stub everywhere else (mirrors `tests/integration/el0_yielder_program`).
//!
//! It is deliberately self-contained — it does not depend on the
//! `tests/integration` harness — and keys only off the OS component of the
//! target (bare-metal vs hosted), never the instruction set, so `cargo xtask
//! cfg-check` (`AGENTS.md` §17.2) stays clean.
//!
//! The consuming vertical (`wait_qemu_aarch64`) sets `RUSTOS_WAIT_ROLE` and
//! `RUSTOS_WAIT_CHILD_CODE` when it compiles this program (once per role), so a
//! changed role or code must force a recompile; declare those dependencies
//! here.

fn main() {
    println!("cargo:rustc-check-cfg=cfg(freestanding)");
    let target_os = std::env::var("CARGO_CFG_TARGET_OS").unwrap_or_default();
    if target_os == "none" {
        println!("cargo:rustc-cfg=freestanding");
    }
    println!("cargo:rerun-if-env-changed=RUSTOS_WAIT_ROLE");
    println!("cargo:rerun-if-env-changed=RUSTOS_WAIT_CHILD_CODE");
    println!("cargo:rerun-if-changed=build.rs");
}
