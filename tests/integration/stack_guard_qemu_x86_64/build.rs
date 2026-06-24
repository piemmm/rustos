//! Build script for the `plans/PI.md` x86_64 guard-page fault-form
//! (G1/G2) vertical.
//!
//! Two jobs:
//!
//! 1. Declare/enable the freestanding conditional-compilation flags
//!    (`itest_x86_64` on the `x86_64-unknown-none` target, nothing on a
//!    host build) via [`rustos_itest_harness::emit_target_cfg`].
//! 2. On the freestanding x86_64 target, hand the production x86_64 kernel
//!    linker script to the test kernel (it boots the real `rustos-kernel`
//!    pipeline, so it links exactly like the other freestanding x86_64
//!    integration binaries).
//!
//! Unlike the `mem_map` / `spawn` x86_64 verticals this test spawns no
//! ring-3 program, so it needs no `rxe` fixture: the split / unmap / fault
//! work happens entirely in supervisor mode on a guard static.

use std::env;

/// Rust target triple of the freestanding x86_64 build.
const X86_64_TARGET: &str = "x86_64-unknown-none";

fn main() {
    rustos_itest_harness::emit_target_cfg();
    println!("cargo:rerun-if-changed=build.rs");

    let target = env::var("TARGET").unwrap_or_default();
    if target == X86_64_TARGET {
        let manifest_dir = env::var("CARGO_MANIFEST_DIR").expect("CARGO_MANIFEST_DIR");
        let manifest_dir = manifest_dir.trim_end_matches('/');
        // The single per-arch script the architecture port owns;
        // mirrors `kernel/rustos-kernel/build.rs` and the sibling x86_64
        // integration binaries.
        let linker = format!("{manifest_dir}/../../../kernel/arch/x86_64/linker.ld");
        println!("cargo:rerun-if-changed={linker}");
        println!("cargo:rustc-link-arg=-T{linker}");
    }
}
