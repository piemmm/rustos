//! Build-time fixture generator for the `PLAN.md` Stage 4.HW aarch64
//! driver-spawn vertical.
//!
//! Three jobs on the freestanding `aarch64-unknown-none` target:
//!
//! 1. Hand the aarch64 `virt` linker script to the test kernel (the single
//!    per-board script the architecture port owns) and dump
//!    the canonical QEMU `virt` flattened device tree, embedding it so the test
//!    discovers the GICv2 base and the generic-timer rate from the firmware
//!    tree (`plans/PI.md` P3/P4). QEMU's `-kernel <ELF>` aarch64 path passes no
//!    DTB pointer (`x0 = 0`), so the board tree is embedded at build time; the
//!    dump helper lives in the shared harness so no aarch64 build script
//!    re-rolls it.
//! 2. Compile the pure-Rust driver-stub fixture program
//!    (`tests/integration/driver_register_program`) **position-independent**
//!    for the freestanding aarch64 target (its own `program.ld` roots
//!    `tairix-rt`'s `_start`), into a private target directory under
//!    `OUT_DIR`.
//! 3. Convert the linked PIE ELF to an `rxe` blob with
//!    [`tairix_itest_harness::elf2rxe::elf_to_rxe`], baking relocations for the
//!    [`USER_BIAS`] the production spawn producer maps every child image at and
//!    stamping the kernel's compiled-in syscall CFI tag
//!    (`tairix_kernel_syscall::SYSCALL_TABLE_HASH`) so
//!    [`tairix_abi::rxe::LoadImage::parse`] accepts it; emit the
//!    bytes and the bias as a Rust source the test `include!`s.
//!
//! On any non-aarch64 target (host `cargo build --workspace`, clippy) it emits
//! inert stubs so the crate still builds; the kernel body that consumes them
//! compiles only for the freestanding aarch64 target.
//!
//! Re-running `build.rs` produces byte-identical output, so the test is
//! deterministic.

use std::env;
use std::fmt::Write as _;
use std::fs;
use std::path::PathBuf;
use std::process::Command;

/// Virtual base the stub program image is mapped at: the production aarch64
/// spawn producer's image bias (the image builder passes it to
/// `build_process_image`, so the `rxe`'s baked relocations must target the
/// same value). It is the shared [`tairix_itest_harness::USER_IMAGE_BIAS`]
/// definition; the test kernel asserts it agrees with the
/// producer's `SHELL_USER_BIAS` at runtime and fails closed on a mismatch.
use tairix_itest_harness::USER_IMAGE_BIAS as USER_BIAS;

/// Rust target triple of the freestanding aarch64 build.
const AARCH64_TARGET: &str = "aarch64-unknown-none";

fn main() {
    tairix_itest_harness::emit_target_cfg();
    println!("cargo:rerun-if-changed=build.rs");

    let manifest_dir = env::var("CARGO_MANIFEST_DIR").expect("CARGO_MANIFEST_DIR");
    let out_dir = env::var("OUT_DIR").expect("OUT_DIR");
    let manifest_dir = manifest_dir.trim_end_matches('/');

    let program_dir = format!("{manifest_dir}/../driver_register_program");
    println!("cargo:rerun-if-changed={program_dir}/src/main.rs");
    println!("cargo:rerun-if-changed={program_dir}/program.ld");
    println!("cargo:rerun-if-changed={program_dir}/Cargo.toml");

    let rxe_path = PathBuf::from(&out_dir).join("program_rxe.rs");
    let dtb_path = PathBuf::from(&out_dir).join("dtb_fixture.rs");
    let image_path = PathBuf::from(&out_dir).join("driver_image.rs");

    let target = env::var("TARGET").unwrap_or_default();
    if target == AARCH64_TARGET {
        // The test kernel itself links with the aarch64 `virt` script the
        // architecture port owns (the single per-board script).
        let linker = format!("{manifest_dir}/../../../kernel/arch/aarch64/link/aarch64-virt.ld");
        println!("cargo:rerun-if-changed={linker}");
        println!("cargo:rustc-link-arg=-T{linker}");

        // One CPU: this is the single-core driver-spawn handshake slice.
        let out_dir_os = std::ffi::OsString::from(&out_dir);
        let dtb = tairix_itest_harness::dump_aarch64_virt_dtb(&out_dir_os, 1);
        write_dtb_fixture(&dtb_path, &dtb);

        let rxe = build_and_convert_program(manifest_dir, &out_dir, &program_dir);
        write_bias_fixture(&rxe_path);
        write_driver_image_fixture(&image_path, &rxe);
    } else {
        // Inert stubs for host / other targets; the kernel body that uses these
        // compiles only for the freestanding aarch64 target.
        write_dtb_fixture(&dtb_path, &[]);
        write_bias_fixture(&rxe_path);
        write_driver_image_fixture(&image_path, &[]);
    }
}

/// Deterministic Ed25519 seed the build signs the driver image with.
const DRIVER_SIGNING_SEED: [u8; 32] = *b"tairix-driver-spawn-vertical/v1!";

/// Wrap `rxe` as the payload of a signed `DriverManifest` image the
/// `SpawnDriverLoader` gate admits, and emit it plus the signer public key.
///
/// The manifest is `kind = UserSpace` (the gate then spawns the payload as a
/// process), stamps the kernel's compiled-in `SYSCALL_TABLE_HASH`, and
/// requests the driver-class capabilities the spawned stub needs to send its
/// reply (`MEM_DMA`, the reply port's send gate, plus `IRQ_BIND`). The
/// signature covers `header[..signed_end] || cap_body || bind_table ||
/// payload`, exactly what `drvhost::Host::verify_signature` reconstructs
/// (the payload program is authenticated). The bind
/// table is empty: the device manager matches against the candidate bind
/// keys the kernel constructs, and a malformed/forged image still fails the
/// gate (the bind-table decode path is covered by `drvhost`'s
/// `devmgr_autoload` test).
fn write_driver_image_fixture(path: &std::path::Path, rxe: &[u8]) {
    use tairix_abi::{CapabilityId, DriverKind};
    use tairix_itest_harness::driver_image::build_signed_driver_image;

    let (image, pubkey): (Vec<u8>, [u8; 32]) = if rxe.is_empty() {
        // Host / non-aarch64 build: inert stub (the kernel body is
        // aarch64-only and never reads these on host).
        (Vec::new(), [0u8; 32])
    } else {
        // The shared composer assembles + signs the bundle from the one
        // manifest wire definition: a `kind = UserSpace`
        // image requesting the driver-class caps the spawned stub needs to
        // reply, with an empty bind table (the device manager matches against
        // the candidate bind keys the kernel constructs), signed so the
        // signature covers the payload.
        let signed = build_signed_driver_image(
            &DRIVER_SIGNING_SEED,
            DriverKind::UserSpace,
            &[CapabilityId::MEM_DMA, CapabilityId::IRQ_BIND],
            &[],
            tairix_kernel_syscall::SYSCALL_TABLE_HASH,
            rxe,
        );
        (signed.image, signed.signer_pubkey)
    };

    let mut out = String::new();
    out.push_str("// Auto-generated by build.rs. DO NOT EDIT.\n");
    out.push_str("/// Signed `kind = UserSpace` driver-manifest image whose payload is the\n");
    out.push_str("/// stub program `rxe`; admitted through the `SpawnDriverLoader` gate.\n");
    out.push_str("pub const DRIVER_IMAGE: &[u8] = &[");
    for (i, b) in image.iter().enumerate() {
        if i % 16 == 0 {
            out.push_str("\n    ");
        }
        let _ = write!(out, "0x{b:02x}, ");
    }
    out.push_str("\n];\n");
    out.push_str("/// Public key the build signed `DRIVER_IMAGE` with — the test's sole\n");
    out.push_str("/// `SpawnDriverLoader` trust anchor.\n");
    out.push_str("pub const DRIVER_SIGNER_PUBKEY: [u8; 32] = [");
    for (i, b) in pubkey.iter().enumerate() {
        if i % 8 == 0 {
            out.push_str("\n    ");
        }
        let _ = write!(out, "0x{b:02x}, ");
    }
    out.push_str("\n];\n");
    fs::write(path, out).expect("write driver_image.rs");
}

/// Compile the driver-stub fixture program PIE for the freestanding aarch64
/// target and convert the linked ELF into an `rxe` blob.
fn build_and_convert_program(manifest_dir: &str, out_dir: &str, program_dir: &str) -> Vec<u8> {
    let program_ld = format!("{program_dir}/program.ld");
    let target_dir = format!("{out_dir}/driver-register-target");

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
    // aarch64 target so the program's own host build script is unaffected, and
    // build `core` / `alloc` / `compiler_builtins` as PIC alongside it
    // (`-Z build-std`). `alloc` is required because `tairix-rt` registers a
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
        .env(
            "CARGO_TARGET_AARCH64_UNKNOWN_NONE_RUSTFLAGS",
            format!("-C relocation-model=pie -C link-arg=-pie -C link-arg=-T{program_ld}"),
        )
        .args([
            "build",
            "-p",
            "tairix-test-driver-register-program",
            "--target",
            AARCH64_TARGET,
            "-Z",
            "build-std=core,compiler_builtins,alloc",
            "--target-dir",
            &target_dir,
        ])
        .status()
        .expect("spawn cargo to build the driver-register fixture program");
    assert!(
        status.success(),
        "building the driver-register fixture program failed"
    );

    let elf_path =
        format!("{target_dir}/{AARCH64_TARGET}/debug/tairix-test-driver-register-program");
    let elf = fs::read(&elf_path).unwrap_or_else(|e| panic!("read {elf_path}: {e}"));

    tairix_itest_harness::elf2rxe::elf_to_rxe(
        &elf,
        &tairix_kernel_syscall::SYSCALL_TABLE_HASH,
        USER_BIAS,
    )
    .expect("convert the driver-register fixture program ELF into an rxe image")
}

/// Emit `USER_BIAS` as a Rust source the test includes.
///
/// The converted stub `rxe` bytes are not emitted on their own: they are
/// embedded as the *payload* of the signed `DRIVER_IMAGE`
/// ([`write_driver_image_fixture`]), which is what the `SpawnDriverLoader`
/// gate admits and spawns. Emitting a second loose copy would be unused.
fn write_bias_fixture(path: &std::path::Path) {
    let mut out = String::new();
    out.push_str("// Auto-generated by build.rs. DO NOT EDIT.\n");
    let _ = writeln!(
        out,
        "/// Virtual base the stub program image is relocated for."
    );
    let _ = writeln!(out, "pub const USER_BIAS: u64 = {USER_BIAS:#x};");
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
