//! Build-time fixture generator for the SPAWN stage `SP5b-2` aarch64
//! `mem_map`/`mem_unmap` vertical.
//!
//! Three jobs on the freestanding `aarch64-unknown-none` target:
//!
//! 1. Hand the aarch64 `virt` linker script to the test kernel (the single
//!    per-board script the architecture port owns — `AGENTS.md` §2.2) and dump
//!    the canonical QEMU `virt` flattened device tree, embedding it so the test
//!    discovers the GICv2 base and the generic-timer rate from the firmware
//!    tree (`plans/PI.md` P3/P4). QEMU's `-kernel <ELF>` aarch64 path passes no
//!    DTB pointer (`x0 = 0`), so the board tree is embedded at build time; the
//!    dump helper lives in the shared harness so no aarch64 build script
//!    re-rolls it (`AGENTS.md` §2.2).
//! 2. Compile the pure-Rust EL0 fixture program (`tests/integration/
//!    mem_map_program`) **position-independent** for the freestanding aarch64
//!    target (its own `program.ld` roots `rustos-rt`'s `_start`), into a
//!    private target directory under `OUT_DIR`, pinning the anonymous-region
//!    base + length through the `RUSTOS_MEM_MAP_ADDR` / `RUSTOS_MEM_MAP_LEN`
//!    environment variables so this script is the single source of truth for
//!    the region the program maps *and* the kernel's fault check verifies
//!    (`AGENTS.md` §2.2).
//! 3. Convert the linked PIE ELF to an `rxe` blob with
//!    [`rustos_itest_harness::elf2rxe::elf_to_rxe`], baking relocations for the
//!    [`USER_BIAS`] the kernel maps the image at and stamping the kernel's
//!    compiled-in syscall CFI tag (`rustos_kernel_syscall::SYSCALL_TABLE_HASH`)
//!    so [`rustos_abi::rxe::LoadImage::parse`] accepts it (§9 / §19.2); emit the
//!    bytes, the bias, and the matching [`REGION_VA`] / [`REGION_LEN`]
//!    constants as a Rust source the test `include!`s.
//!
//! On any non-aarch64 target (host `cargo build --workspace`, clippy) it emits
//! inert stubs so the crate still builds; the kernel body that consumes them
//! compiles only for the freestanding aarch64 target.
//!
//! Re-running `build.rs` produces byte-identical output, so the test is
//! deterministic (`AGENTS.md` §7).

use std::env;
use std::fmt::Write as _;
use std::fs;
use std::path::PathBuf;
use std::process::Command;

/// Virtual base the program image is mapped at. Chosen at 64 GiB — far above
/// the kernel's 2 GiB identity map and within the 39-bit (512 GiB) TTBR0
/// region — so the image lands on freshly walked stage-1 tables instead of
/// colliding with an identity gigapage block (`AGENTS.md` §2.2 — the proven
/// spawn layout).
const USER_BIAS: u64 = 0x10_0000_0000;

/// Virtual base of the anonymous region the program maps with `mem_map`
/// (FIXED). 16 MiB above [`USER_BIAS`] — clear of the program image, its user
/// stack, and the startup-vector block — so the region lands on fresh stage-1
/// tables and never overlaps the spawn-time image. The single source of truth:
/// passed to the program build via `RUSTOS_MEM_MAP_ADDR` *and* emitted as the
/// `REGION_VA` constant the kernel's fault handler checks the faulting address
/// against, so the two halves can never disagree (`AGENTS.md` §2.2).
const REGION_VA: u64 = USER_BIAS + (16 << 20);

/// Length in bytes of the anonymous region (two pages). Passed to the program
/// build via `RUSTOS_MEM_MAP_LEN` and emitted as the `REGION_LEN` constant the
/// kernel sizes its fault-range check from (`AGENTS.md` §2.2).
const REGION_LEN: u64 = 2 * 4096;

/// Rust target triple of the freestanding aarch64 build.
const AARCH64_TARGET: &str = "aarch64-unknown-none";

fn main() {
    rustos_itest_harness::emit_target_cfg();
    println!("cargo:rerun-if-changed=build.rs");

    let manifest_dir = env::var("CARGO_MANIFEST_DIR").expect("CARGO_MANIFEST_DIR");
    let out_dir = env::var("OUT_DIR").expect("OUT_DIR");
    let manifest_dir = manifest_dir.trim_end_matches('/');

    let program_dir = format!("{manifest_dir}/../mem_map_program");
    println!("cargo:rerun-if-changed={program_dir}/src/main.rs");
    println!("cargo:rerun-if-changed={program_dir}/program.ld");
    println!("cargo:rerun-if-changed={program_dir}/Cargo.toml");

    let rxe_path = PathBuf::from(&out_dir).join("program_rxe.rs");
    let dtb_path = PathBuf::from(&out_dir).join("dtb_fixture.rs");

    let target = env::var("TARGET").unwrap_or_default();
    if target == AARCH64_TARGET {
        // The test kernel itself links with the aarch64 `virt` script the
        // architecture port owns (the single per-board script, §2.2).
        let linker = format!("{manifest_dir}/../../../kernel/arch/aarch64/link/aarch64-virt.ld");
        println!("cargo:rerun-if-changed={linker}");
        println!("cargo:rustc-link-arg=-T{linker}");

        // One CPU: this is the single-core live-scheduler slice.
        let out_dir_os = std::ffi::OsString::from(&out_dir);
        let dtb = rustos_itest_harness::dump_aarch64_virt_dtb(&out_dir_os, 1);
        write_dtb_fixture(&dtb_path, &dtb);

        let rxe = build_and_convert_program(manifest_dir, &out_dir, &program_dir);
        write_program_fixture(&rxe_path, &rxe);
    } else {
        // Inert stubs for host / other targets; the kernel body that uses these
        // compiles only for the freestanding aarch64 target.
        write_dtb_fixture(&dtb_path, &[]);
        write_program_fixture(&rxe_path, &[]);
    }
}

/// Compile the EL0 fixture program PIE for the freestanding aarch64 target and
/// convert the linked ELF into an `rxe` blob.
fn build_and_convert_program(manifest_dir: &str, out_dir: &str, program_dir: &str) -> Vec<u8> {
    let program_ld = format!("{program_dir}/program.ld");
    let target_dir = format!("{out_dir}/mem-map-target");

    // Cargo fingerprints the RUSTFLAGS *string* (which names the linker script
    // by path) but not the script's *content*, so a `program.ld` edit would not
    // by itself trigger a relink and the converter could read a stale ELF.
    // `build.rs` only reruns when its `rerun-if-changed` inputs (including
    // `program.ld`) actually change, so wiping the private target directory
    // here forces a clean rebuild against the current script without churning
    // ordinary incremental builds.
    let _ = fs::remove_dir_all(&target_dir);

    // The program links no architecture crate, so `program.ld`'s
    // `ENTRY(_start)` roots `rustos-rt`'s trampoline; it is built
    // position-independent (`AGENTS.md` §19.2). Scope the PIE link flags to the
    // aarch64 target so the program's own host build script is unaffected, and
    // build `core` / `alloc` / `compiler_builtins` as PIC alongside it
    // (`-Z build-std`). `alloc` is required because `rustos-rt` registers a
    // `#[global_allocator]` (its `mem_map`-backed heap), so the program names
    // `alloc`; omitting it would pull `alloc` from the prebuilt sysroot while
    // `core` is built fresh, a duplicate-lang-item link error.
    let cargo = env::var_os("CARGO").unwrap_or_else(|| "cargo".into());
    let status = Command::new(cargo)
        .current_dir(manifest_dir)
        // The outer build exports `CARGO_ENCODED_RUSTFLAGS` / `RUSTFLAGS` into
        // this build script's environment; both outrank the target-scoped var
        // below, so a nested cargo would inherit the outer kernel's flags and
        // drop the PIE link recipe. Clear them so the target-scoped flags win
        // and apply only to the aarch64 program crates (not the program's own
        // host build script).
        .env_remove("CARGO_ENCODED_RUSTFLAGS")
        .env_remove("RUSTFLAGS")
        // Pin the region base + length (the §2.2 single source of truth).
        .env("RUSTOS_MEM_MAP_ADDR", REGION_VA.to_string())
        .env("RUSTOS_MEM_MAP_LEN", REGION_LEN.to_string())
        .env(
            "CARGO_TARGET_AARCH64_UNKNOWN_NONE_RUSTFLAGS",
            format!("-C relocation-model=pie -C link-arg=-pie -C link-arg=-T{program_ld}"),
        )
        .args([
            "build",
            "-p",
            "rustos-test-mem-map",
            "--target",
            AARCH64_TARGET,
            "-Z",
            "build-std=core,compiler_builtins,alloc",
            "--target-dir",
            &target_dir,
        ])
        .status()
        .expect("spawn cargo to build the mem-map fixture program");
    assert!(
        status.success(),
        "building the mem-map fixture program failed"
    );

    let elf_path = format!("{target_dir}/{AARCH64_TARGET}/debug/rustos-test-mem-map");
    let elf = fs::read(&elf_path).unwrap_or_else(|e| panic!("read {elf_path}: {e}"));

    rustos_itest_harness::elf2rxe::elf_to_rxe(
        &elf,
        &rustos_kernel_syscall::SYSCALL_TABLE_HASH,
        USER_BIAS,
    )
    .expect("convert the mem-map fixture program ELF into an rxe image")
}

/// Emit `PROGRAM_RXE`, `USER_BIAS`, `REGION_VA`, and `REGION_LEN` as a Rust
/// source the test includes.
fn write_program_fixture(path: &std::path::Path, rxe: &[u8]) {
    let mut out = String::new();
    out.push_str("// Auto-generated by build.rs. DO NOT EDIT.\n");
    let _ = writeln!(out, "/// Virtual base the program image is mapped at.");
    let _ = writeln!(out, "pub const USER_BIAS: u64 = {USER_BIAS:#x};");
    let _ = writeln!(
        out,
        "/// Virtual base the program maps its anonymous region at (build.rs)."
    );
    let _ = writeln!(out, "pub const REGION_VA: u64 = {REGION_VA:#x};");
    let _ = writeln!(
        out,
        "/// Length in bytes of the anonymous region (pinned by build.rs)."
    );
    let _ = writeln!(out, "pub const REGION_LEN: u64 = {REGION_LEN};");
    out.push_str("/// The converted `rxe` image of the mem-map fixture program.\n");
    out.push_str("pub const PROGRAM_RXE: &[u8] = &[");
    for (i, b) in rxe.iter().enumerate() {
        if i % 16 == 0 {
            out.push_str("\n    ");
        }
        let _ = write!(out, "0x{b:02x}, ");
    }
    out.push_str("\n];\n");
    fs::write(path, out).expect("write program_rxe.rs");
}

/// Emit the embedded `virt` device tree as a Rust source the test includes.
fn write_dtb_fixture(path: &std::path::Path, dtb: &[u8]) {
    let mut out = String::new();
    out.push_str("// Auto-generated by build.rs. DO NOT EDIT.\n");
    out.push_str("/// Canonical QEMU `virt` flattened device tree, dumped at build\n");
    out.push_str("/// time for the aarch64-none target (empty on host builds).\n");
    out.push_str("pub const DTB_BLOB: &[u8] = &[");
    for (i, b) in dtb.iter().enumerate() {
        if i % 16 == 0 {
            out.push_str("\n    ");
        }
        let _ = write!(out, "0x{b:02x}, ");
    }
    out.push_str("\n];\n");
    fs::write(path, out).expect("write dtb_fixture.rs");
}
