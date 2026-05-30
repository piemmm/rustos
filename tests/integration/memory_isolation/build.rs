//! Build script: hand the kernel linker script to `rustc` *only* on the
//! freestanding `x86_64-unknown-none` target. On host builds we do
//! nothing so the crate still compiles for `cargo check`/IDE indexing.
//!
//! `AGENTS.md` §2.2 forbids duplicating the linker script per test, so
//! both `tests/integration/*` crates point at the *same* file under
//! `kernel/arch/x86_64/linker.ld`.

fn main() {
    rustos_itest_harness::emit_target_cfg();

    let target = std::env::var("TARGET").unwrap_or_default();
    if target == "x86_64-unknown-none" {
        let manifest_dir = std::env::var("CARGO_MANIFEST_DIR").expect("CARGO_MANIFEST_DIR");
        let linker_script = format!(
            "{}/../../../kernel/arch/x86_64/linker.ld",
            manifest_dir.trim_end_matches('/')
        );
        println!("cargo:rerun-if-changed={linker_script}");
        println!("cargo:rustc-link-arg=-T{linker_script}");
        // rust-lld is the default linker for `x86_64-unknown-none`; the
        // linker script + the `OUTPUT_FORMAT(elf64-x86-64)` directive in it
        // are all the constraints we need. We deliberately don't pass GCC
        // driver flags (`-nostartfiles`, `-no-pie`) — those are unknown to
        // rust-lld and would be ignored or rejected.
    }
}
