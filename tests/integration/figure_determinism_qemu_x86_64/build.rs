//! Build script: hand the kernel linker script to `rustc` *only* on the
//! freestanding `x86_64-unknown-none` target. On host builds we do
//! nothing so the crate still compiles for `cargo check`/IDE indexing.
//!
//! The layout is the shared one under `kernel/arch/x86_64/linker.ld`; a
//! per-test copy would be the duplication the charter forbids. This image
//! links a two-line script of its own that states its boot-stack size and
//! includes that layout.

fn main() {
    tairix_itest_harness::emit_target_cfg();
    println!("cargo:rerun-if-changed=build.rs");

    let target = std::env::var("TARGET").unwrap_or_default();
    if target == "x86_64-unknown-none" {
        let manifest_dir = std::env::var("CARGO_MANIFEST_DIR").expect("CARGO_MANIFEST_DIR");
        let linker_script = format!(
            "{}/../../../kernel/arch/x86_64/linker.ld",
            manifest_dir.trim_end_matches('/')
        );
        println!("cargo:rerun-if-changed={linker_script}");
        // The whole figure grid is drawn on the boot stack, and it needs
        // more than the boot pipeline's own 64 KiB: this matches the
        // aarch64 and riscv64 `virt` scripts, which size theirs for it. The
        // size is stated ahead of the shared layout rather than by
        // `--defsym`, which the linker applies only after the layout it
        // would have sized.
        let out_dir = std::env::var("OUT_DIR").expect("OUT_DIR");
        let image_script = format!("{out_dir}/figure-determinism.ld");
        std::fs::write(
            &image_script,
            format!("BOOT_STACK_BYTES = 256K;\nINCLUDE \"{linker_script}\"\n"),
        )
        .expect("the image's linker script is written");
        println!("cargo:rustc-link-arg=-T{image_script}");
    }
}
