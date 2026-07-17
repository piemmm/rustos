//! Build-time fixture generator for the `plans/PI.md` stage RV-X3 riscv64
//! runtime-`spawn` concurrent-producer vertical (the cross-port sibling of the
//! `aarch64`/`x86_64` `spawn_session` build scripts).
//!
//! Two jobs on the freestanding `riscv64gc-unknown-none-elf` target:
//!
//! 1. Hand the riscv64 `virt` linker script to the test kernel (the single
//!    per-arch script the architecture port owns), exactly
//!    as the sibling riscv64 integration binaries do.
//! 2. Compile the pure-Rust spawn-session fixture program
//!    (`tests/integration/spawn_session_program`) **twice** — once as the
//!    **parent** role and once as the **child** (session) role, selected by the
//!    `TAIRIX_SPAWN_ROLE` environment variable — **position-independent** for the
//!    freestanding riscv64 target (its own `program.ld` roots `tairix-rt`'s
//!    `_start`), each into a private target directory under `OUT_DIR`, pinning
//!    the yield count through `TAIRIX_SPAWN_YIELDS` so this script is the single
//!    source of truth for the count. Each linked PIE ELF is
//!    converted to an `rxe` blob with
//!    [`tairix_itest_harness::elf2rxe::elf_to_rxe`], baking relocations for the
//!    [`USER_BIAS`] the kernel maps each image at and stamping the kernel's
//!    compiled-in syscall CFI tag (`tairix_kernel_syscall::SYSCALL_TABLE_HASH`)
//!    so [`tairix_abi::rxe::LoadImage::parse`] accepts it; emit the
//!    two blobs, the shared bias, and the matching [`YIELDS_PER_TASK`] constant
//!    as a Rust source the test `include!`s. One source serves both roles: the per-process isolation is the separate page-table
//!    hierarchies the kernel builds, not separate sources.
//!
//! On any non-riscv64 target (host `cargo build --workspace`, clippy) it emits
//! inert stubs so the crate still builds; the kernel body that consumes them
//! compiles only for the freestanding riscv64 target.
//!
//! Re-running `build.rs` produces byte-identical output, so the test is
//! deterministic.

use std::env;
use std::fmt::Write as _;
use std::fs;
use std::path::PathBuf;
use std::process::Command;

/// Virtual base each program image is mapped at, in *its own* address space.
/// Chosen at 64 GiB — far above the kernel's identity map — so the image lands
/// on freshly walked Sv39 tables instead of colliding with an identity gigapage
/// leaf (the proven riscv64 spawn layout).
const USER_BIAS: u64 = 0x10_0000_0000;

/// How many times each role yields before exiting. The single source of truth:
/// passed to both program builds via `TAIRIX_SPAWN_YIELDS` *and* emitted as the
/// `YIELDS_PER_TASK` constant the kernel asserts against, so the two halves can
/// never disagree. Large enough that an accidental single
/// run cannot satisfy the PASS check, small enough to drain well within the
/// harness budget.
const YIELDS_PER_TASK: u32 = 8;

/// Rust target triple of the freestanding riscv64 build.
const RISCV64_TARGET: &str = "riscv64gc-unknown-none-elf";

fn main() {
    tairix_itest_harness::emit_target_cfg();
    println!("cargo:rerun-if-changed=build.rs");

    let manifest_dir = env::var("CARGO_MANIFEST_DIR").expect("CARGO_MANIFEST_DIR");
    let out_dir = env::var("OUT_DIR").expect("OUT_DIR");
    let manifest_dir = manifest_dir.trim_end_matches('/');

    let program_dir = format!("{manifest_dir}/../spawn_session_program");
    println!("cargo:rerun-if-changed={program_dir}/src/main.rs");
    println!("cargo:rerun-if-changed={program_dir}/program.ld");
    println!("cargo:rerun-if-changed={program_dir}/Cargo.toml");
    println!("cargo:rerun-if-changed={program_dir}/build.rs");

    let rxe_path = PathBuf::from(&out_dir).join("program_rxe.rs");

    let target = env::var("TARGET").unwrap_or_default();
    if target == RISCV64_TARGET {
        // The test kernel itself links with the riscv64 `virt` script the
        // architecture port owns (the single per-arch script).
        let linker = format!("{manifest_dir}/../../../kernel/arch/riscv64/link/riscv64-virt.ld");
        println!("cargo:rerun-if-changed={linker}");
        println!("cargo:rustc-link-arg=-T{linker}");

        let parent = build_and_convert_program(manifest_dir, &out_dir, &program_dir, "parent");
        let child = build_and_convert_program(manifest_dir, &out_dir, &program_dir, "child");
        write_program_fixture(&rxe_path, &parent, &child);
    } else {
        // Inert stub for host / other targets; the kernel body that uses these
        // consts compiles only for the freestanding riscv64 target.
        write_program_fixture(&rxe_path, &[], &[]);
    }
}

/// Compile the spawn-session fixture program PIE for the freestanding riscv64
/// target in the given `role` (`"parent"` or `"child"`) and convert the linked
/// ELF into an `rxe` blob.
fn build_and_convert_program(
    manifest_dir: &str,
    out_dir: &str,
    program_dir: &str,
    role: &str,
) -> Vec<u8> {
    let program_ld = format!("{program_dir}/program.ld");
    let target_dir = format!("{out_dir}/spawn-session-{role}-target");

    // Cargo fingerprints the RUSTFLAGS *string* (which names the linker script
    // by path) but not the script's *content*, so a `program.ld` edit would not
    // by itself trigger a relink and the converter could read a stale ELF.
    // `build.rs` only reruns when its `rerun-if-changed` inputs (including
    // `program.ld`) actually change, so wiping the private target directory
    // here forces a clean rebuild against the current script without churning
    // ordinary incremental builds.
    let _ = fs::remove_dir_all(&target_dir);

    // The program links no architecture crate, so `program.ld`'s
    // `ENTRY(_start)` roots `tairix-rt`'s trampoline; it is built
    // position-independent. Scope the PIE link flags to the
    // riscv64 target so the program's own host build script is unaffected, and
    // build `core` / `alloc` / `compiler_builtins` as PIC alongside it
    // (`-Z build-std`). `alloc` is required because `tairix-rt` registers a
    // `#[global_allocator]`, so the program names `alloc`; omitting it would
    // pull `alloc` from the prebuilt sysroot while `core` is built fresh, a
    // duplicate-lang-item link error.
    let cargo = env::var_os("CARGO").unwrap_or_else(|| "cargo".into());
    let status = Command::new(cargo)
        .current_dir(manifest_dir)
        // The outer build exports `CARGO_ENCODED_RUSTFLAGS` / `RUSTFLAGS` into
        // this build script's environment; both outrank the target-scoped var
        // below, so a nested cargo would inherit the outer kernel's flags and
        // drop the PIE link recipe. Clear them so the target-scoped flags win
        // and apply only to the riscv64 program crates (not the program's own
        // host build script).
        .env_remove("CARGO_ENCODED_RUSTFLAGS")
        .env_remove("RUSTFLAGS")
        // Select the role and pin the yield count (the single source of
        // truth). Both halves of the fixture read these.
        .env("TAIRIX_SPAWN_ROLE", role)
        .env("TAIRIX_SPAWN_YIELDS", YIELDS_PER_TASK.to_string())
        .env(
            "CARGO_TARGET_RISCV64GC_UNKNOWN_NONE_ELF_RUSTFLAGS",
            format!("-C relocation-model=pie -C link-arg=-pie -C link-arg=-T{program_ld}"),
        )
        .args([
            "build",
            "-p",
            "tairix-test-spawn-session-program",
            "--target",
            RISCV64_TARGET,
            "-Z",
            "build-std=core,compiler_builtins,alloc",
            "--target-dir",
            &target_dir,
        ])
        .status()
        .expect("spawn cargo to build the spawn-session fixture program");
    assert!(
        status.success(),
        "building the spawn-session fixture program ({role}) failed"
    );

    let elf_path = format!("{target_dir}/{RISCV64_TARGET}/debug/tairix-test-spawn-session-program");
    let elf = fs::read(&elf_path).unwrap_or_else(|e| panic!("read {elf_path}: {e}"));

    tairix_itest_harness::elf2rxe::elf_to_rxe(
        &elf,
        &tairix_kernel_syscall::SYSCALL_TABLE_HASH,
        USER_BIAS,
    )
    .unwrap_or_else(|e| {
        panic!("convert the spawn-session fixture program ({role}) ELF into an rxe image: {e:?}")
    })
}

/// Emit `PARENT_RXE`, `CHILD_RXE`, `USER_BIAS`, and `YIELDS_PER_TASK` as a Rust
/// source the test includes.
fn write_program_fixture(path: &std::path::Path, parent: &[u8], child: &[u8]) {
    let mut out = String::new();
    out.push_str("// Auto-generated by build.rs. DO NOT EDIT.\n");
    let _ = writeln!(out, "/// Virtual base each program image is mapped at.");
    let _ = writeln!(out, "pub const USER_BIAS: u64 = {USER_BIAS:#x};");
    let _ = writeln!(
        out,
        "/// Times each role yields before exiting (pinned by build.rs)."
    );
    let _ = writeln!(out, "pub const YIELDS_PER_TASK: u64 = {YIELDS_PER_TASK};");
    emit_blob(&mut out, "PARENT_RXE", "the parent role", parent);
    emit_blob(&mut out, "CHILD_RXE", "the child (session) role", child);
    fs::write(path, out).expect("write program_rxe.rs");
}

/// Emit one `pub const <name>: &[u8] = &[…];` byte-blob constant.
fn emit_blob(out: &mut String, name: &str, role_desc: &str, rxe: &[u8]) {
    let _ = writeln!(
        out,
        "/// The converted `rxe` image of the spawn-session fixture in {role_desc}."
    );
    let _ = write!(out, "pub const {name}: &[u8] = &[");
    for (i, b) in rxe.iter().enumerate() {
        if i % 16 == 0 {
            out.push_str("\n    ");
        }
        let _ = write!(out, "0x{b:02x}, ");
    }
    out.push_str("\n];\n");
}
