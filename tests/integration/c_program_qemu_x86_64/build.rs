//! Build-time fixture generator for the CCOMPAT stage CC5 x86_64 C-program
//! round-trip.
//!
//! Unlike the CC3 spawn round-trip (whose fixture program is written in Rust),
//! the program here is written in **C** (`../cc5_program/csrc/main.c`). The
//! kernel spawn path consumes an `rxe` load image, so on the freestanding
//! `x86_64-unknown-none` target this script:
//!
//! 1. hands the production x86_64 kernel linker script to `rustc` (the test
//!    boots the real `tairix-kernel` pipeline, so it links exactly like the
//!    other freestanding x86_64 integration binaries);
//! 2. builds the Rust startup/runtime shim (`tairix-test-cc5-program`) as a
//!    position-independent `staticlib` for the freestanding x86_64 target —
//!    this bundles crt0's `_start` and the `tairix_sys_*` syscall stubs into one
//!    `.a` (the curated *System runtime / C ABI* class);
//! 3. compiles `csrc/main.c` to a PIE object with the audited, version-pinned,
//!    checksummed `clang` wrapper (`tairix_cc`) — TAIRiX stays
//!    Rust-only; this only *hosts* a C program;
//! 4. links the object + the shim archive into a PIE ELF with the audited
//!    `ld.lld` wrapper, rooting crt0's `_start` via the shared CC3 link script;
//! 5. converts the linked PIE ELF to an `rxe` blob with
//!    [`tairix_itest_harness::elf2rxe::elf_to_rxe`], baking relocations for the
//!    [`USER_BIAS`] the kernel maps the image at and stamping the kernel's
//!    compiled-in syscall CFI tag so [`tairix_abi::rxe::LoadImage::parse`]
//!    accepts it;
//! 6. emits the bytes and `USER_BIAS` as Rust source the test `include!`s.
//!
//! On any non-x86_64 target (host `cargo build --workspace`, clippy) it emits
//! an inert stub so the crate still builds; the kernel body that consumes the
//! blob compiles only for the freestanding x86_64 target.
//!
//! This mirrors the riscv64 sibling
//! (`tests/integration/c_program_qemu_riscv64/build.rs`); only the target
//! triple, its `RUSTFLAGS` environment-variable name, the `CTarget`, and the
//! per-arch kernel linker script differ.

use std::env;
use std::fmt::Write as _;
use std::fs;
use std::path::{Path, PathBuf};
use std::process::Command;

use tairix_cc::{CTarget, CompileRequest, LinkRequest, Toolchain};

/// Virtual base the program image is mapped at. Identical choice to the CC3
/// x86_64 round-trip: 64 GiB, far above the kernel's low 32 MiB identity window
/// and the higher-half kernel window, below the 512 GiB PML4[0] boundary, so
/// the program's pages land on freshly walked tables.
const USER_BIAS: u64 = 0x10_0000_0000;

/// Rust target triple of the freestanding x86_64 build.
const X86_64_TARGET: &str = "x86_64-unknown-none";

fn main() {
    tairix_itest_harness::emit_target_cfg();
    println!("cargo:rerun-if-changed=build.rs");

    let manifest_dir = env::var("CARGO_MANIFEST_DIR").expect("CARGO_MANIFEST_DIR");
    let out_dir = env::var("OUT_DIR").expect("OUT_DIR");
    let manifest_dir = manifest_dir.trim_end_matches('/');

    let program_dir = format!("{manifest_dir}/../cc5_program");
    let c_source = format!("{program_dir}/csrc/main.c");
    // The PIE link script is the architecture-neutral one the CC3 fixture
    // already owns; reusing it keeps a single definition.
    let link_script = format!("{manifest_dir}/../cc3_program/program.ld");
    let include_dir = format!("{manifest_dir}/../../../include");
    println!("cargo:rerun-if-changed={c_source}");
    println!("cargo:rerun-if-changed={program_dir}/src/lib.rs");
    println!("cargo:rerun-if-changed={program_dir}/Cargo.toml");
    println!("cargo:rerun-if-changed={link_script}");

    let rxe_path = PathBuf::from(&out_dir).join("program_rxe.rs");

    let target = env::var("TARGET").unwrap_or_default();
    if target == X86_64_TARGET {
        // Hand the production x86_64 kernel linker script to the test kernel
        // itself (the single per-arch script the architecture port owns);
        // mirrors `kernel/tairix-kernel/build.rs` and the sibling x86_64
        // integration binaries.
        let kernel_linker = format!("{manifest_dir}/../../../kernel/arch/x86_64/linker.ld");
        println!("cargo:rerun-if-changed={kernel_linker}");
        println!("cargo:rustc-link-arg=-T{kernel_linker}");

        let rxe = build_c_program(
            manifest_dir,
            &out_dir,
            Path::new(&c_source),
            Path::new(&link_script),
            Path::new(&include_dir),
        );
        write_fixture(&rxe_path, &rxe);
    } else {
        // Inert stub for host / other targets; the kernel body that uses these
        // consts compiles only for the freestanding x86_64 target.
        write_fixture(&rxe_path, &[]);
    }
}

/// Build the x86_64 PIE C image and convert it to an `rxe` blob.
fn build_c_program(
    manifest_dir: &str,
    out_dir: &str,
    c_source: &Path,
    link_script: &Path,
    include_dir: &Path,
) -> Vec<u8> {
    let archive = build_runtime_shim(manifest_dir, out_dir);

    // Discover and validate the C toolchain (version-pinned + checksummed); record the audited binaries for the build transcript.
    let toolchain =
        Toolchain::discover().unwrap_or_else(|e| panic!("C toolchain unavailable: {e}"));
    for line in toolchain.audit_lines() {
        println!("cargo:warning=tairix-cc: {line}");
    }

    let object = PathBuf::from(out_dir).join("cc5_main.o");
    toolchain
        .compile(&CompileRequest {
            target: CTarget::X86_64,
            source: c_source,
            object: &object,
            include_dirs: &[include_dir],
        })
        .unwrap_or_else(|e| panic!("compiling the CC5 C program failed: {e}"));

    let elf_path = PathBuf::from(out_dir).join("cc5_program.elf");
    toolchain
        .link(&LinkRequest {
            target: CTarget::X86_64,
            objects: &[&object],
            archives: &[&archive],
            linker_script: link_script,
            output: &elf_path,
        })
        .unwrap_or_else(|e| panic!("linking the CC5 C program failed: {e}"));

    let elf = fs::read(&elf_path).unwrap_or_else(|e| panic!("read {}: {e}", elf_path.display()));
    tairix_itest_harness::elf2rxe::elf_to_rxe(
        &elf,
        &tairix_kernel_syscall::SYSCALL_TABLE_HASH,
        USER_BIAS,
    )
    .expect("convert the CC5 C program ELF into an rxe image")
}

/// Build the Rust crt0 + `tairix_sys_*` runtime shim as a position-independent
/// `staticlib` for the freestanding x86_64 target, returning its `.a` path.
fn build_runtime_shim(manifest_dir: &str, out_dir: &str) -> PathBuf {
    let target_dir = format!("{out_dir}/cc5-shim-target");
    // The shim links no architecture crate; built PIC alongside `core` /
    // `compiler_builtins` (`-Z build-std`) with the same relocation model the
    // C object uses, so the final image carries only `R_*_RELATIVE`
    // relocations and `elf_to_rxe` accepts it.
    let cargo = env::var_os("CARGO").unwrap_or_else(|| "cargo".into());
    let status = Command::new(cargo)
        .current_dir(manifest_dir)
        // The outer build exports `CARGO_ENCODED_RUSTFLAGS` / `RUSTFLAGS` into
        // this build script's environment; both outrank the target-scoped var
        // below. Clear them so the PIC flag applies to the shim crates only.
        .env_remove("CARGO_ENCODED_RUSTFLAGS")
        .env_remove("RUSTFLAGS")
        .env(
            "CARGO_TARGET_X86_64_UNKNOWN_NONE_RUSTFLAGS",
            "-C relocation-model=pie",
        )
        .args([
            "build",
            "--release",
            "-p",
            "tairix-test-cc5-program",
            "--target",
            X86_64_TARGET,
            "-Z",
            "build-std=core,compiler_builtins",
            "-Z",
            "build-std-features=compiler-builtins-mem",
            "--target-dir",
            &target_dir,
        ])
        .status()
        .expect("spawn cargo to build the CC5 runtime shim");
    assert!(status.success(), "building the CC5 runtime shim failed");

    PathBuf::from(format!(
        "{target_dir}/{X86_64_TARGET}/release/libtairix_test_cc5_program.a"
    ))
}

/// Emit `PROGRAM_RXE` and `USER_BIAS` as a Rust source file the test includes.
fn write_fixture(path: &Path, rxe: &[u8]) {
    let mut out = String::new();
    out.push_str("// Auto-generated by build.rs. DO NOT EDIT.\n");
    let _ = writeln!(out, "/// Virtual base the program image is mapped at.");
    let _ = writeln!(out, "pub const USER_BIAS: u64 = {USER_BIAS:#x};");
    out.push_str("/// The converted `rxe` image of the CC5 C program.\n");
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
