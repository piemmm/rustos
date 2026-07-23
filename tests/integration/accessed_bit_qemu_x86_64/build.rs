//! Build script: hand the kernel linker script to `rustc` *only* on the
//! freestanding `x86_64-unknown-none` target. On host builds we do
//! nothing so the crate still compiles for `cargo check`/IDE indexing.
//!
//! the charter forbids duplicating the linker script per test, so
//! every `tests/integration/*` x86_64 crate points at the *same* file
//! under `kernel/arch/x86_64/linker.ld`.

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
