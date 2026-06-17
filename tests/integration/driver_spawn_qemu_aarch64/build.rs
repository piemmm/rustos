//! Build-time fixture generator for the `PLAN.md` Stage 4.HW aarch64
//! driver-spawn vertical.
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
//! 2. Compile the pure-Rust driver-stub fixture program
//!    (`tests/integration/driver_register_program`) **position-independent**
//!    for the freestanding aarch64 target (its own `program.ld` roots
//!    `rustos-rt`'s `_start`), into a private target directory under
//!    `OUT_DIR`.
//! 3. Convert the linked PIE ELF to an `rxe` blob with
//!    [`rustos_itest_harness::elf2rxe::elf_to_rxe`], baking relocations for the
//!    [`USER_BIAS`] the production spawn producer maps every child image at and
//!    stamping the kernel's compiled-in syscall CFI tag
//!    (`rustos_kernel_syscall::SYSCALL_TABLE_HASH`) so
//!    [`rustos_abi::rxe::LoadImage::parse`] accepts it (§9 / §19.2); emit the
//!    bytes and the bias as a Rust source the test `include!`s.
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

/// Virtual base the stub program image is mapped at. This is the production
/// aarch64 spawn producer's image bias
/// (`rustos_kernel::aarch64::spawn_producer::USER_IMAGE_BIAS`, 64 GiB): `spawn_with`
/// passes that bias to `build_process_image`, so the `rxe`'s baked
/// relocations must target the same value. The test kernel asserts the two
/// constants agree at runtime and fails closed on a mismatch
/// (`AGENTS.md` §2.9).
const USER_BIAS: u64 = 0x10_0000_0000;

/// Rust target triple of the freestanding aarch64 build.
const AARCH64_TARGET: &str = "aarch64-unknown-none";

fn main() {
    rustos_itest_harness::emit_target_cfg();
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
        // architecture port owns (the single per-board script, §2.2).
        let linker = format!("{manifest_dir}/../../../kernel/arch/aarch64/link/aarch64-virt.ld");
        println!("cargo:rerun-if-changed={linker}");
        println!("cargo:rustc-link-arg=-T{linker}");

        // One CPU: this is the single-core driver-spawn handshake slice.
        let out_dir_os = std::ffi::OsString::from(&out_dir);
        let dtb = rustos_itest_harness::dump_aarch64_virt_dtb(&out_dir_os, 1);
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
const DRIVER_SIGNING_SEED: [u8; 32] = *b"rustos-driver-spawn-vertical/v1!";

/// Wrap `rxe` as the payload of a signed `DriverManifest` image the
/// `SpawnDriverLoader` gate admits, and emit it plus the signer public key.
///
/// The manifest is `kind = UserSpace` (the gate then spawns the payload as a
/// process), stamps the kernel's compiled-in `SYSCALL_TABLE_HASH`, and
/// requests the driver-class capabilities the spawned stub needs to send its
/// reply (`MEM_DMA`, the reply port's send gate, plus `IRQ_BIND`). The
/// signature covers `header[..signed_end] || cap_body || bind_table ||
/// payload`, exactly what `drvhost::Host::verify_signature` reconstructs
/// (`AGENTS.md` §8 / §2.17 — the payload program is authenticated). The bind
/// table is empty: the device manager matches against the candidate bind
/// keys the kernel constructs, and a malformed/forged image still fails the
/// gate (the bind-table decode path is covered by `drvhost`'s
/// `devmgr_autoload` test).
fn write_driver_image_fixture(path: &std::path::Path, rxe: &[u8]) {
    use ed25519_dalek::{Signer, SigningKey};
    use rustos_abi::{CapabilityId, DriverKind, DriverManifest, DRIVER_MANIFEST_MAGIC};

    let (image, pubkey): (Vec<u8>, [u8; 32]) = if rxe.is_empty() {
        // Host / non-aarch64 build: inert stub (the kernel body is
        // aarch64-only and never reads these on host).
        (Vec::new(), [0u8; 32])
    } else {
        let signing_key = SigningKey::from_bytes(&DRIVER_SIGNING_SEED);
        let signer_pubkey: [u8; 32] = signing_key.verifying_key().to_bytes();
        let caps = [CapabilityId::MEM_DMA, CapabilityId::IRQ_BIND];
        let capability_count = u16::try_from(caps.len()).expect("caps fit in u16");
        let mut manifest = DriverManifest {
            magic: DRIVER_MANIFEST_MAGIC,
            abi_version: rustos_abi::ABI_VERSION_CURRENT,
            kind: DriverKind::UserSpace,
            bind_key_count: 0,
            capability_count,
            syscall_table_hash: rustos_kernel_syscall::SYSCALL_TABLE_HASH,
            signer_pubkey,
            signature: [0u8; 64],
        };
        let mut cap_body = Vec::with_capacity(caps.len() * 2);
        for c in &caps {
            cap_body.extend_from_slice(&c.as_u16().to_le_bytes());
        }
        let header = manifest.to_le_bytes();
        let signed_end = DriverManifest::WIRE_LEN - 64;
        let mut signed_message = Vec::new();
        signed_message.extend_from_slice(&header[..signed_end]);
        signed_message.extend_from_slice(&cap_body);
        // (empty bind table)
        signed_message.extend_from_slice(rxe);
        manifest.signature = signing_key.sign(&signed_message).to_bytes();
        let mut out = Vec::new();
        out.extend_from_slice(&manifest.to_le_bytes());
        out.extend_from_slice(&cap_body);
        out.extend_from_slice(rxe);
        (out, signer_pubkey)
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
        .env(
            "CARGO_TARGET_AARCH64_UNKNOWN_NONE_RUSTFLAGS",
            format!("-C relocation-model=pie -C link-arg=-pie -C link-arg=-T{program_ld}"),
        )
        .args([
            "build",
            "-p",
            "rustos-test-driver-register-program",
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
        format!("{target_dir}/{AARCH64_TARGET}/debug/rustos-test-driver-register-program");
    let elf = fs::read(&elf_path).unwrap_or_else(|e| panic!("read {elf_path}: {e}"));

    rustos_itest_harness::elf2rxe::elf_to_rxe(
        &elf,
        &rustos_kernel_syscall::SYSCALL_TABLE_HASH,
        USER_BIAS,
    )
    .expect("convert the driver-register fixture program ELF into an rxe image")
}

/// Emit `USER_BIAS` as a Rust source the test includes.
///
/// The converted stub `rxe` bytes are not emitted on their own: they are
/// embedded as the *payload* of the signed `DRIVER_IMAGE`
/// ([`write_driver_image_fixture`]), which is what the `SpawnDriverLoader`
/// gate admits and spawns. Emitting a second loose copy would be unused
/// (`AGENTS.md` §2.14).
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
