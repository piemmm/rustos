//! Build script: hand the riscv64 `virt` linker script to `rustc` *only*
//! on the freestanding `riscv64gc-unknown-none-elf` target. On host builds we
//! do nothing so the crate still compiles for `cargo check`/IDE indexing.
//!
//! The linker script is the shared one under
//! `kernel/arch/riscv64/link/riscv64-virt.ld`; a per-test copy would be
//! the duplication the charter forbids.

fn main() {
    tairix_itest_harness::emit_target_cfg();
    println!("cargo:rerun-if-changed=build.rs");

    let target = std::env::var("TARGET").unwrap_or_default();
    if target == "riscv64gc-unknown-none-elf" {
        let manifest_dir = std::env::var("CARGO_MANIFEST_DIR").expect("CARGO_MANIFEST_DIR");
        let linker = format!(
            "{}/../../../kernel/arch/riscv64/link/riscv64-virt.ld",
            manifest_dir.trim_end_matches('/')
        );
        println!("cargo:rerun-if-changed={linker}");
        println!("cargo:rustc-link-arg=-T{linker}");
    }
}
