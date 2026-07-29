//! Build script for the x86_64 pre-boot-Supervisor `memtest` takeover QEMU
//! vertical (`plans/NEW-SUPERVISOR.md` §9 Stage E).
//!
//! One job on the freestanding `x86_64-unknown-none` target, identical to the
//! x86_64 ESC boot-screen vertical this mirrors: hand the production x86_64
//! kernel linker script to `rustc` — the single per-arch script the
//! architecture port owns (no duplication). On a host build the bin compiles
//! to a no-op `main`, so no linker script is needed.

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
