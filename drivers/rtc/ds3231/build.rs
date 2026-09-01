//! Build script: enable the `freestanding` cfg when the crate is built for a
//! bare-metal target (`target_os = "none"`), so the ds3231 driver's `Run`
//! binary (`src/main.rs`) compiles as a freestanding pure-Rust program there
//! and as an inert host stub everywhere else.
//!
//! Keys only off the OS component of the target (bare-metal vs hosted), never
//! the instruction set, so `cargo xtask cfg-check` stays clean. It mirrors
//! `drivers/input/virtio_kbd/build.rs`.

fn main() {
    println!("cargo:rustc-check-cfg=cfg(freestanding)");
    let target_os = std::env::var("CARGO_CFG_TARGET_OS").unwrap_or_default();
    if target_os == "none" {
        println!("cargo:rustc-cfg=freestanding");
    }
    println!("cargo:rerun-if-changed=build.rs");
}
