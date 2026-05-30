//! Build script for the `rustos-kernel` crate.
//!
//! Two responsibilities, both build glue (`AGENTS.md` §17.2 confines
//! target-conditional decisions to the architecture ports and the build
//! glue; a build script is build glue):
//!
//! 1. Hand the kernel linker script to `rustc` *only* on the
//!    freestanding `x86_64-unknown-none` target. Mirrors
//!    `tests/integration/scheduler_stress_qemu/build.rs` and
//!    `tests/integration/memory_isolation/build.rs` exactly — all three
//!    crates share the same linker script (`AGENTS.md` §2.2 — no
//!    duplication).
//!
//! 2. Emit the `freestanding` conditional-compilation name when the crate
//!    is being built as the bare-metal production kernel
//!    (`x86_64-unknown-none`). The crate's source gates its freestanding
//!    body — the `boot`/`panic_ctx`/`serial_sink` modules, the
//!    `#[no_std]`/`#[no_main]` attributes, the IO-APIC publication slot,
//!    and the production `halt` — on this name rather than naming the
//!    target instruction set inline, so the instruction-set choice lives
//!    in this one audited place (`AGENTS.md` §17.2). The host build
//!    (`cargo build --workspace` / `cargo test`) leaves the name unset and
//!    compiles the inert host stubs instead.

fn main() {
    println!("cargo:rustc-check-cfg=cfg(freestanding)");

    let target = std::env::var("TARGET").unwrap_or_default();
    if target == "x86_64-unknown-none" {
        let manifest_dir = std::env::var("CARGO_MANIFEST_DIR").expect("CARGO_MANIFEST_DIR");
        let linker_script = format!(
            "{}/../arch/x86_64/linker.ld",
            manifest_dir.trim_end_matches('/')
        );
        println!("cargo:rerun-if-changed={linker_script}");
        println!("cargo:rustc-link-arg=-T{linker_script}");
    }

    if is_freestanding() {
        println!("cargo:rustc-cfg=freestanding");
    }
}

/// True when the crate is being built as the bare-metal production
/// kernel: the `x86_64` instruction set with no host operating system.
fn is_freestanding() -> bool {
    let os = std::env::var("CARGO_CFG_TARGET_OS").unwrap_or_default();
    let arch = std::env::var("CARGO_CFG_TARGET_ARCH").unwrap_or_default();
    os == "none" && arch == "x86_64"
}
