//! Build script: hand the x86_64 linker script to `rustc` *only* on the
//! freestanding `x86_64-unknown-none` target. On host builds we do nothing so the
//! crate still compiles for `cargo check` / IDE indexing.
//!
//! the charter forbids duplicating the linker script per test, so this
//! crate points at the same file every other x86_64 vertical uses — which
//! is also the script whose guard reservation this test exists to check.

fn main() {
    tairix_itest_harness::emit_target_cfg();
    println!("cargo:rerun-if-changed=build.rs");

    let target = std::env::var("TARGET").unwrap_or_default();
    if target == "x86_64-unknown-none" {
        let manifest_dir = std::env::var("CARGO_MANIFEST_DIR").expect("CARGO_MANIFEST_DIR");
        let linker = format!(
            "{}/../../../kernel/arch/x86_64/linker.ld",
            manifest_dir.trim_end_matches('/')
        );
        println!("cargo:rerun-if-changed={linker}");
        println!("cargo:rustc-link-arg=-T{linker}");
    }
}
