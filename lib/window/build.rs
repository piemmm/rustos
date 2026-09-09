//! Build script: enable the `freestanding` cfg when the crate is built for a
//! bare-metal target (`target_os = "none"`), so the app-side shell
//! (`src/app.rs`) — which links the production memory-pressure seam that only
//! exists there — compiles only where it can run, and is absent from host
//! builds and from the server half the desktop session composes.
//!
//! Keys only off the OS component of the target (bare-metal vs hosted), never
//! the instruction set, so `cargo xtask cfg-check` stays clean. It mirrors
//! `lib/procinfo/build.rs`.

fn main() {
    println!("cargo:rustc-check-cfg=cfg(freestanding)");
    let target_os = std::env::var("CARGO_CFG_TARGET_OS").unwrap_or_default();
    if target_os == "none" {
        println!("cargo:rustc-cfg=freestanding");
    }
    println!("cargo:rerun-if-changed=build.rs");
}
