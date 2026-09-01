//! Build script for the x86_64 real-time-clock live-boot QEMU vertical
//! (`plans/TIMESYNC.md` TS-3).
//!
//! One job on the freestanding `x86_64-unknown-none` target: hand the
//! production x86_64 kernel linker script to `rustc`. QEMU's PVH `-kernel`
//! loader enters the kernel directly and the CMOS clock has no device-tree
//! node, so unlike the aarch64 sibling there is no fixture blob to embed:
//! the clock node is synthesised on the port pair every PC-compatible
//! machine carries.

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
