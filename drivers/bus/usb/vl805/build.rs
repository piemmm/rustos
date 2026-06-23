//! Build script: enable the `freestanding` cfg when the crate is built for a
//! bare-metal target (`target_os = "none"`), so the VL805 USB bus driver's
//! `Run` binary (`src/main.rs`) compiles as a freestanding pure-Rust program
//! there and as an inert host stub everywhere else (`plans/PI.md` P10 D5c).
//!
//! This is deliberately self-contained and keys only off the OS component of
//! the target (bare-metal vs hosted), never the instruction set, so `cargo
//! xtask cfg-check` (`AGENTS.md` §17.2) stays clean. It mirrors
//! `drivers/bus/pcie_brcm/build.rs` (`AGENTS.md` §2.2).

fn main() {
    println!("cargo:rustc-check-cfg=cfg(freestanding)");
    let target_os = std::env::var("CARGO_CFG_TARGET_OS").unwrap_or_default();
    if target_os == "none" {
        println!("cargo:rustc-cfg=freestanding");
    }
    println!("cargo:rerun-if-changed=build.rs");
}
