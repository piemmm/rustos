//! Build script: hand the kernel linker script to `rustc` *only* on
//! the freestanding `x86_64-unknown-none` target. Mirrors
//! `kernel/tairix-kernel/build.rs` and the sibling
//! `tests/integration/enter_user_qemu_x86_64/build.rs` exactly — all the
//! freestanding x86_64 integration binaries share the same linker script
//! (no duplication).

fn main() {
    tairix_itest_harness::emit_target_cfg();

    let target = std::env::var("TARGET").unwrap_or_default();
    if target == "x86_64-unknown-none" {
        let manifest_dir = std::env::var("CARGO_MANIFEST_DIR").expect("CARGO_MANIFEST_DIR");
        let linker_script = format!(
            "{}/../../../kernel/arch/x86_64/linker.ld",
            manifest_dir.trim_end_matches('/')
        );
        println!("cargo:rerun-if-changed={linker_script}");
        println!("cargo:rustc-link-arg=-T{linker_script}");
    }
}
