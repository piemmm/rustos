//! Build script for the riscv64 spawn-PID-1 QEMU vertical (`plans/PI.md`
//! RV-P3).
//!
//! One job on the freestanding `riscv64gc-unknown-none-elf` target: hand
//! the riscv64 `virt` linker script to `rustc` — the single per-board
//! linker script the architecture port owns, exactly as
//! the sibling `kernel_arch_boot_riscv64` boot vertical does. Unlike the
//! aarch64 `-kernel` path (which passes `x0 = 0` and so embeds a DTB), the
//! riscv64 OpenSBI firmware hands the boot hart a real device-tree pointer
//! in `a1`, so no embedded DTB fixture is needed here.
//!
//! On any non-riscv64 target (host `cargo build --workspace`, clippy) it
//! emits only the target cfg; the kernel body that consumes the boot
//! pipeline compiles only for the freestanding riscv64 target.

fn main() {
    rustos_itest_harness::emit_target_cfg();
    println!("cargo:rerun-if-changed=build.rs");

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
