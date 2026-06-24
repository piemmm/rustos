//! Build script: hand the riscv64 `virt` linker script to `rustc` only
//! on the freestanding `riscv64gc-unknown-none-elf` target. Mirrors
//! `tests/integration/timer_preempt_qemu_riscv64/build.rs`; both crates
//! reference the single per-arch linker script the architecture port
//! owns (no duplication).

fn main() {
    rustos_itest_harness::emit_target_cfg();

    let target = std::env::var("TARGET").unwrap_or_default();
    if target == "riscv64gc-unknown-none-elf" {
        let manifest_dir = std::env::var("CARGO_MANIFEST_DIR").expect("CARGO_MANIFEST_DIR");
        let linker_script = format!(
            "{}/../../../kernel/arch/riscv64/link/riscv64-virt.ld",
            manifest_dir.trim_end_matches('/')
        );
        println!("cargo:rerun-if-changed={linker_script}");
        println!("cargo:rustc-link-arg=-T{linker_script}");
    }
}
