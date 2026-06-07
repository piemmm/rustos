//! Build script: enable the `freestanding` cfg when the EL0 yielder program
//! is built for a bare-metal target (`target_os = "none"`), so `src/main.rs`
//! compiles as a freestanding pure-Rust program there and as an inert host
//! stub everywhere else (mirrors `userland/system/init/build.rs`).
//!
//! It is deliberately self-contained — it does not depend on the
//! `tests/integration` harness — and keys only off the OS component of the
//! target (bare-metal vs hosted), never the instruction set, so `cargo xtask
//! cfg-check` (`AGENTS.md` §17.2) stays clean.
//!
//! The consuming vertical (`spawn_el0_timeshare_qemu_aarch64`) sets
//! `RUSTOS_EL0_YIELDS` when it compiles this program, so a changed yield count
//! must force a recompile; declare that dependency here.

fn main() {
    println!("cargo:rustc-check-cfg=cfg(freestanding)");
    let target_os = std::env::var("CARGO_CFG_TARGET_OS").unwrap_or_default();
    if target_os == "none" {
        println!("cargo:rustc-cfg=freestanding");
    }
    println!("cargo:rerun-if-env-changed=RUSTOS_EL0_YIELDS");
    println!("cargo:rerun-if-changed=build.rs");
}
