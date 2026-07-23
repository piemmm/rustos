//! Build script: hand the aarch64 `virt` linker script to `rustc` *only*
//! on the freestanding `aarch64-unknown-none` target. On host builds we
//! do nothing so the crate still compiles for `cargo check`/IDE indexing.
//!
//! the charter forbids duplicating the linker script per test, so every
//! `tests/integration/*` aarch64 crate points at the *same* file under
//! `kernel/arch/aarch64/link/aarch64-virt.ld`.

fn main() {
    tairix_itest_harness::emit_target_cfg();
    println!("cargo:rerun-if-changed=build.rs");

    let target = std::env::var("TARGET").unwrap_or_default();
    if target == "aarch64-unknown-none" {
        let manifest_dir = std::env::var("CARGO_MANIFEST_DIR").expect("CARGO_MANIFEST_DIR");
        let linker = format!(
            "{}/../../../kernel/arch/aarch64/link/aarch64-virt.ld",
            manifest_dir.trim_end_matches('/')
        );
        println!("cargo:rerun-if-changed={linker}");
        println!("cargo:rustc-link-arg=-T{linker}");
    }
}
