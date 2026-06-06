//! Build script for the `rustos-kernel` crate.
//!
//! Two responsibilities, both build glue (`AGENTS.md` §17.2 confines
//! target-conditional decisions to the architecture ports and the build
//! glue; a build script is build glue):
//!
//! 1. Hand the per-board boot linker script to `rustc` on each
//!    freestanding bare-metal target. The x86_64 image links
//!    `arch/x86_64/linker.ld`; the aarch64 image links the Raspberry
//!    Pi 4 boot script `arch/aarch64/link/aarch64-rpi4.ld` (load address
//!    `0x8_0000`). The QEMU `virt` board's `aarch64-virt.ld` is used only
//!    by the per-test bins, which carry their own build scripts
//!    (`AGENTS.md` §2.2 — no duplication; the one legitimate per-board
//!    artefact is the boot stub + linker script per `plans/PI.md` §0.2).
//!
//! 2. Emit the conditional-compilation names the crate body gates on:
//!    * `freestanding` when the crate is built as a bare-metal production
//!      kernel (a supported instruction set with `target_os = "none"`).
//!    * `kernel_isa = "<isa>"` — the chosen instruction set — for *every*
//!      build, host included. The crate body gates each architecture's
//!      modules (the x86_64 boot pipeline, the aarch64 boot pipeline) on
//!      these names rather than the target instruction set inline, so
//!      the choice lives in this one audited place (`AGENTS.md` §17.2;
//!      `cargo xtask cfg-check` forbids the target-conditional predicate
//!      in the crate body).
//!
//! The pure selection logic lives in `src/build_support.rs` (also unit
//! tested by the crate's host test build); this script only reads the
//! Cargo-provided target strings and emits the directives.

// The pure, unit-tested selection logic, shared with the crate's host
// test build. Pulled in as a module (not a crate dependency) so the
// build script stays dependency-free.
#[path = "src/build_support.rs"]
mod build_support;

use std::fmt::Write as _;
use std::fs;
use std::path::{Path, PathBuf};
use std::process::Command;

use build_support::{is_freestanding, kernel_isa, linker_script_for};

/// Rust target triple of the freestanding aarch64 (Raspberry Pi 4) build.
const AARCH64_TARGET: &str = "aarch64-unknown-none";

/// Virtual base the `init` (`Run`) program image is mapped at when the
/// aarch64 boot path spawns PID 1 (`plans/PI.md` P6c-3).
///
/// 64 GiB — far above the boot path's identity map and within the 39-bit
/// (512 GiB) TTBR0 region — so the program's pages land on freshly walked
/// stage-1 tables instead of colliding with an identity gigapage block.
/// `boot_aarch64`'s `InitSpawn` passes the same bias to `spawn_and_enter`,
/// and `elf_to_rxe` relocates the image for it, so the in-memory pointers
/// match where the image is mapped. Mirrors the proven
/// `spawn_program_qemu_aarch64` fixture's bias (`AGENTS.md` §2.2).
const INIT_USER_BIAS: u64 = 0x10_0000_0000;

fn main() {
    println!("cargo:rustc-check-cfg=cfg(freestanding)");
    println!("cargo:rustc-check-cfg=cfg(kernel_isa, values(\"x86_64\", \"aarch64\"))");

    let target = std::env::var("TARGET").unwrap_or_default();
    let target_os = std::env::var("CARGO_CFG_TARGET_OS").unwrap_or_default();
    let target_arch = std::env::var("CARGO_CFG_TARGET_ARCH").unwrap_or_default();

    if let Some(linker_script) = linker_script_for(&target) {
        let manifest_dir = std::env::var("CARGO_MANIFEST_DIR").expect("CARGO_MANIFEST_DIR");
        let linker_script = format!("{}/{linker_script}", manifest_dir.trim_end_matches('/'));
        println!("cargo:rerun-if-changed={linker_script}");
        println!("cargo:rustc-link-arg=-T{linker_script}");
    }

    if let Some(isa) = kernel_isa(&target_arch) {
        println!("cargo:rustc-cfg=kernel_isa=\"{isa}\"");
    }

    if is_freestanding(&target_os, &target_arch) {
        println!("cargo:rustc-cfg=freestanding");
    }

    emit_init_rxe(&target);
}

/// Build the `init` (`Run`) program PIE and embed its `rxe` image so the
/// aarch64 boot path can spawn PID 1 into EL0 (`plans/PI.md` P6c-3).
///
/// On the freestanding aarch64 target it compiles `rustos-init-run`
/// position-independent against its own `Run.ld` into a private target
/// directory under `OUT_DIR` (so it never collides with the outer kernel
/// build — `AGENTS.md` §2.2, one program source built two ways), then
/// converts the linked PIE ELF into an `rxe` blob with
/// [`rustos_itest_harness::elf2rxe::elf_to_rxe`], baking relocations for
/// [`INIT_USER_BIAS`] and stamping the kernel's compiled-in syscall CFI
/// tag (`rustos_kernel_syscall::SYSCALL_TABLE_HASH`) so
/// [`rustos_abi::rxe::LoadImage::parse`] accepts it (§9 / §19.2).
///
/// On every other target (host `cargo build --workspace`, clippy, the
/// x86_64 image) it emits an inert empty blob: the boot-path module that
/// consumes `INIT_RXE` compiles only for the freestanding aarch64 target.
fn emit_init_rxe(target: &str) {
    let manifest_dir = std::env::var("CARGO_MANIFEST_DIR").expect("CARGO_MANIFEST_DIR");
    let out_dir = std::env::var("OUT_DIR").expect("OUT_DIR");
    let manifest_dir = manifest_dir.trim_end_matches('/');

    let init_dir = format!("{manifest_dir}/../../userland/system/init");
    println!("cargo:rerun-if-changed={init_dir}/src/run.rs");
    println!("cargo:rerun-if-changed={init_dir}/src/startup.rs");
    println!("cargo:rerun-if-changed={init_dir}/Run.ld");
    println!("cargo:rerun-if-changed={init_dir}/Cargo.toml");

    let rxe_path = PathBuf::from(&out_dir).join("init_rxe.rs");

    let rxe = if target == AARCH64_TARGET {
        build_and_convert_init(manifest_dir, &out_dir, &init_dir)
    } else {
        Vec::new()
    };
    write_init_fixture(&rxe_path, &rxe);
}

/// Compile `rustos-init-run` PIE for the freestanding aarch64 target and
/// convert the linked ELF into an `rxe` blob.
fn build_and_convert_init(manifest_dir: &str, out_dir: &str, init_dir: &str) -> Vec<u8> {
    let run_ld = format!("{init_dir}/Run.ld");
    let target_dir = format!("{out_dir}/init-target");

    // Cargo fingerprints the RUSTFLAGS *string* (which names the linker
    // script by path) but not the script's *content*, so a `Run.ld` edit
    // would not by itself trigger a relink and the converter could read a
    // stale ELF. `build.rs` only reruns when its `rerun-if-changed` inputs
    // (including `Run.ld`) actually change, so wiping the private target
    // directory here forces a clean rebuild against the current script
    // without churning ordinary incremental builds.
    let _ = fs::remove_dir_all(&target_dir);

    // `init` links no architecture crate, so `Run.ld`'s `ENTRY(_start)`
    // roots the `rustos-rt` runtime trampoline; it is built
    // position-independent (`AGENTS.md` §19.2), with `core` /
    // `compiler_builtins` / `alloc` built PIC alongside it (`-Z
    // build-std`). `alloc` is required because the `init` package's
    // dependencies (`rustos-log`, `rustos-abi`) name `alloc`, even though
    // the banner-printing `Run` binary itself never allocates (the
    // unreachable allocating paths are dead-stripped, so no global
    // allocator is needed). Scope the PIE link flags to the aarch64 target
    // so the program's own host build script is unaffected.
    let cargo = std::env::var_os("CARGO").unwrap_or_else(|| "cargo".into());
    let status = Command::new(cargo)
        .current_dir(manifest_dir)
        // The outer build exports `CARGO_ENCODED_RUSTFLAGS` / `RUSTFLAGS`
        // into this build script's environment; both outrank the
        // target-scoped var below, so a nested cargo would inherit the
        // outer kernel's flags and drop the PIE link recipe. Clear them so
        // the target-scoped flags win and apply only to the aarch64 program
        // crates (not the program's own host build script).
        .env_remove("CARGO_ENCODED_RUSTFLAGS")
        .env_remove("RUSTFLAGS")
        .env(
            "CARGO_TARGET_AARCH64_UNKNOWN_NONE_RUSTFLAGS",
            format!("-C relocation-model=pie -C link-arg=-pie -C link-arg=-T{run_ld}"),
        )
        .args([
            "build",
            "-p",
            "rustos-init",
            "--bin",
            "rustos-init-run",
            "--target",
            AARCH64_TARGET,
            "-Z",
            "build-std=core,compiler_builtins,alloc",
            "--target-dir",
            &target_dir,
        ])
        .status()
        .expect("spawn cargo to build the init Run program");
    assert!(status.success(), "building the init Run program failed");

    let elf_path = format!("{target_dir}/{AARCH64_TARGET}/debug/rustos-init-run");
    let elf = fs::read(&elf_path).unwrap_or_else(|e| panic!("read {elf_path}: {e}"));

    rustos_itest_harness::elf2rxe::elf_to_rxe(
        &elf,
        &rustos_kernel_syscall::SYSCALL_TABLE_HASH,
        INIT_USER_BIAS,
    )
    .expect("convert the init Run program ELF into an rxe image")
}

/// Emit `INIT_RXE` and `INIT_USER_BIAS` as a Rust source the boot path
/// `include!`s.
fn write_init_fixture(path: &Path, rxe: &[u8]) {
    let mut out = String::new();
    out.push_str("// Auto-generated by build.rs. DO NOT EDIT.\n");
    let _ = writeln!(out, "/// Virtual base the `init` image is mapped at.");
    let _ = writeln!(out, "pub const INIT_USER_BIAS: u64 = {INIT_USER_BIAS:#x};");
    out.push_str("/// The converted `rxe` image of the `init` `Run` program.\n");
    out.push_str("pub const INIT_RXE: &[u8] = &[");
    for (i, b) in rxe.iter().enumerate() {
        if i % 16 == 0 {
            out.push_str("\n    ");
        }
        let _ = write!(out, "0x{b:02x}, ");
    }
    out.push_str("\n];\n");
    fs::write(path, out).expect("write init_rxe.rs");
}
