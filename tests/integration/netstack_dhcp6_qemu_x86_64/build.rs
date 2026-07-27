//! Build script for the x86_64 DHCPv6 netstack live-boot QEMU vertical
//! (`plans/DHCP.md` D4c).
//!
//! One job on the freestanding `x86_64-unknown-none` target: hand the
//! production x86_64 kernel linker script to `rustc` — the single per-arch
//! script the architecture port owns, exactly as the sibling
//! `netstack_static_qemu_x86_64` / `netstack_autoload_qemu_x86_64` boot
//! verticals do (no duplication). QEMU's PVH `-kernel` loader enters the
//! kernel directly; the planted virtio-blk-pci disk and the attached
//! `virtio-net-pci` device populate the PCI bus the bootstrap-floor
//! virtio-PCI enumeration probes, so no boot media or embedded fixture is
//! needed.
//!
//! On any non-x86_64 target (host `cargo build --workspace`, clippy) it emits
//! only the target cfg; the kernel body that consumes the boot pipeline
//! compiles only for the freestanding x86_64 target.

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
        println!("cargo:rustc-link-arg=-T{linker_script}");
    }
}
