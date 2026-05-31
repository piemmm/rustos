//! Build script: hand the aarch64 `virt` linker script to `rustc` only
//! on the freestanding `aarch64-unknown-none` target. Mirrors
//! `tests/integration/kernel_arch_boot_aarch64/build.rs`; both crates
//! reference the single per-arch linker script the architecture port
//! owns (`AGENTS.md` §2.2 — no duplication).

fn main() {
    rustos_itest_harness::emit_target_cfg();

    let target = std::env::var("TARGET").unwrap_or_default();
    if target == "aarch64-unknown-none" {
        let manifest_dir = std::env::var("CARGO_MANIFEST_DIR").expect("CARGO_MANIFEST_DIR");
        let linker_script = format!(
            "{}/../../../kernel/arch/aarch64/link/aarch64-virt.ld",
            manifest_dir.trim_end_matches('/')
        );
        println!("cargo:rerun-if-changed={linker_script}");
        println!("cargo:rustc-link-arg=-T{linker_script}");
    }
}
