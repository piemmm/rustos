//! Build script for the riscv64 bond-failover netstack live-boot QEMU
//! vertical (`plans/NETWORK.md` N9b-3-2-β-2-ii-b-bond).
//!
//! One job on the freestanding `riscv64gc-unknown-none-elf` target: hand the
//! riscv64 `virt` linker script to `rustc` — the single per-board linker
//! script the architecture port owns, exactly as the sibling
//! `netstack_autoload_qemu_riscv64` / `netstack_static_qemu_riscv64` boot
//! verticals do (no duplication). QEMU's OpenSBI firmware hands the boot hart
//! a real device-tree pointer in `a1`, so no embedded DTB fixture is needed
//! here — the planted virtio-blk disk and the two attached `virtio-net-device`
//! NICs populate the board's `virtio,mmio` transport slots, which the
//! bootstrap-floor enumeration probes.
//!
//! On any non-riscv64 target (host `cargo build --workspace`, clippy) it emits
//! only the target cfg; the kernel body that consumes the boot pipeline
//! compiles only for the freestanding riscv64 target.

fn main() {
    tairix_itest_harness::emit_target_cfg();
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
