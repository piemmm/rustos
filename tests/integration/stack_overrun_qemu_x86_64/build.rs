//! Build script for the `plans/PI.md` x86_64 guard-page fault-form
//! (stage G3c) production vertical.
//!
//! Two jobs:
//!
//! 1. Declare/enable the freestanding conditional-compilation flags
//!    (`itest_x86_64` on the `x86_64-unknown-none` target, nothing on a
//!    host build) via [`tairix_itest_harness::emit_target_cfg`].
//! 2. On the freestanding x86_64 target, hand the production x86_64 kernel
//!    linker script to the test kernel (it boots the real `tairix-kernel`
//!    pipeline, so it links exactly like the other freestanding x86_64
//!    integration binaries).
//!
//! Like the sibling `mem_map_qemu_x86_64` this test spawns no ring-3
//! program, so it needs no `rxe` fixture: the arena split / unmap /
//! scheduler / overrun work happens entirely in supervisor mode on a guard
//! arena static.

use std::env;

use tairix_itest_harness::pie::PieArch;

/// Rust target triple of the freestanding x86_64 build.
/// Freestanding target this vertical cross-compiles for.
const ARCH: PieArch = PieArch::X86_64;

fn main() {
    tairix_itest_harness::emit_target_cfg();
    println!("cargo:rerun-if-changed=build.rs");

    let target = env::var("TARGET").unwrap_or_default();
    if target == ARCH.target_triple() {
        let manifest_dir = env::var("CARGO_MANIFEST_DIR").expect("CARGO_MANIFEST_DIR");
        let manifest_dir = manifest_dir.trim_end_matches('/');
        // The single per-arch script the architecture port owns;
        // mirrors `kernel/tairix-kernel/build.rs` and the sibling x86_64
        // integration binaries.
        let linker = format!("{manifest_dir}/../../../kernel/arch/x86_64/linker.ld");
        println!("cargo:rerun-if-changed={linker}");
        println!("cargo:rustc-link-arg=-T{linker}");
    }
}
