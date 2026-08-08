//! Build script for the `tairix-kernel` crate.
//!
//! Two responsibilities, both build glue (confines
//! target-conditional decisions to the architecture ports and the build
//! glue; a build script is build glue):
//!
//! 1. Hand the per-board boot linker script to `rustc` on each
//!    freestanding bare-metal target. The x86_64 image links
//!    `arch/x86_64/linker.ld`; the aarch64 image links the Raspberry
//!    Pi 4 boot script `arch/aarch64/link/aarch64-rpi4.ld` (load address
//!    `0x8_0000`). The QEMU `virt` board's `aarch64-virt.ld` is used only
//!    by the per-test bins, which carry their own build scripts
//!    (no duplication; the one legitimate per-board
//!    artefact is the boot stub + linker script per `plans/PI.md` §0.2).
//!
//! 2. Emit the conditional-compilation names the crate body gates on:
//!    * `freestanding` when the crate is built as a bare-metal production
//!      kernel (a supported instruction set with `target_os = "none"`).
//!    * `kernel_isa = "<isa>"` — the chosen instruction set — for *every*
//!      build, host included. The crate body gates each architecture's
//!      modules (the x86_64 boot pipeline, the aarch64 boot pipeline) on
//!      these names rather than the target instruction set inline, so
//!      the choice lives in this one audited place (
//!      `cargo xtask cfg-check` forbids the target-conditional predicate
//!      in the crate body).
//!
//! The pure selection logic lives in `src/build_support.rs` (also unit
//! tested by the crate's host test build); this script only reads the
//! Cargo-provided target strings and emits the directives.

// The pure, unit-tested selection logic, shared with the crate's host
// test build. Pulled in as a module (not a crate dependency) so the
// build script stays dependency-free. `dead_code` is allowed because the
// shared file also defines single-source values this build script does
// not itself consume (the app-signing seed, signed by the image builds
// and pinned by the crate's host tests); an unused copy here is the cost
// of keeping one definition, not dead surface.
#[allow(dead_code)]
#[path = "src/build_support.rs"]
mod build_support;

use std::fmt::Write as _;
use std::fs;
use std::path::{Path, PathBuf};
use std::process::Command;

use build_support::{
    format_grouped_hex, is_freestanding, kernel_isa, linker_script_for, GROUPED_HEX_LEN,
    KERNEL_DRIVER_SIGNING_SEED, SYSTEM_APP_SIGNING_SEED,
};
use tairix_itest_harness::USER_IMAGE_BIAS;

/// Rust target triple of the freestanding aarch64 (Raspberry Pi 4) build.
const AARCH64_TARGET: &str = "aarch64-unknown-none";

/// Rust target triple of the freestanding x86_64 build.
const X86_64_TARGET: &str = "x86_64-unknown-none";

/// Rust target triple of the freestanding riscv64 (QEMU `virt` / SiFive)
/// build.
const RISCV64_TARGET: &str = "riscv64gc-unknown-none-elf";

/// The `CARGO_TARGET_<TRIPLE>_RUSTFLAGS` environment variable that scopes
/// the PIE link recipe to a given freestanding target (and to it alone, so
/// the embedded program's own host build script is never affected).
///
/// Returns `None` for any target that is not one of the three bare-metal
/// production targets (x86_64, aarch64, riscv64) — host builds, clippy, and
/// fmt then emit inert empty fixtures (the boot-path modules that consume
/// them compile only for a freestanding production target).
fn program_rustflags_var(target: &str) -> Option<&'static str> {
    match target {
        AARCH64_TARGET => Some("CARGO_TARGET_AARCH64_UNKNOWN_NONE_RUSTFLAGS"),
        X86_64_TARGET => Some("CARGO_TARGET_X86_64_UNKNOWN_NONE_RUSTFLAGS"),
        RISCV64_TARGET => Some("CARGO_TARGET_RISCV64GC_UNKNOWN_NONE_ELF_RUSTFLAGS"),
        _ => None,
    }
}

/// Virtual base each spawned program (`Run`) image is mapped at when a
/// production boot path builds it (`plans/PI.md` P6c-3 on aarch64, X3a on
/// x86_64, RV-P3 on riscv64; `plans/SPAWN.md` `SP3b`).
///
/// 64 GiB — far above each boot path's identity map and within the per-arch
/// user VA region (the 39-bit aarch64 TTBR0 / x86_64 / Sv39 windows) — so
/// the program's pages land on freshly walked tables instead of colliding
/// with an identity gigapage block. The spawn seam / producer passes the
/// same bias to the build caller, and `elf_to_rxe` relocates the image for
/// it, so the in-memory pointers match where the image is mapped. Each
/// program lives in its **own** address space, so every program reuses this
/// one bias. Mirrors the proven per-arch
/// `spawn_program_qemu_*` fixtures' bias.
const USER_BIAS: u64 = 0x10_0000_0000;

/// One embedded `Run` program the boot path builds into an `rxe` image: the
/// crate package, its `Run` bin, the generated fixture file name, and the
/// `const`-name prefix the fixture emits under.
struct Program {
    /// Cargo package name (`-p <pkg>`).
    pkg: &'static str,
    /// `Run` binary name (`--bin <bin>`).
    bin: &'static str,
    /// Generated fixture file name written under `OUT_DIR`.
    fixture: &'static str,
    /// Prefix for the emitted `const`s (`<PREFIX>_RXE`, `<PREFIX>_USER_BIAS`).
    prefix: &'static str,
}

/// The embedded programs every production boot path spawns: PID 1 `init`, and
/// the `Shell` session program `init` launches (`plans/SPAWN.md` `SP3b`). Both
/// are pure-Rust `Run` bins built the same way for whichever production target
/// is active (one build path), differing only in their
/// package/paths.
const PROGRAMS: &[Program] = &[
    Program {
        pkg: "tairix-init",
        bin: "tairix-init-run",
        fixture: "init_rxe.rs",
        prefix: "INIT",
    },
    Program {
        pkg: "tairix-elsh",
        bin: "tairix-elsh-run",
        fixture: "shell_rxe.rs",
        prefix: "SHELL",
    },
    Program {
        pkg: "tairix-login",
        bin: "tairix-login-run",
        fixture: "login_rxe.rs",
        prefix: "LOGIN",
    },
    Program {
        pkg: "tairix-devmgr",
        bin: "tairix-devmgr-run",
        fixture: "devmgr_rxe.rs",
        prefix: "DEVMGR",
    },
    Program {
        pkg: "tairix-sysinfod",
        bin: "tairix-sysinfod-run",
        fixture: "sysinfod_rxe.rs",
        prefix: "SYSINFOD",
    },
    Program {
        pkg: "tairix-seatmgr",
        bin: "tairix-seatmgr-run",
        fixture: "seatmgr_rxe.rs",
        prefix: "SEATMGR",
    },
    Program {
        pkg: "tairix-netstack",
        bin: "tairix-netstack-run",
        fixture: "netstack_rxe.rs",
        prefix: "NETSTACK",
    },
    Program {
        pkg: "tairix-fontd",
        bin: "tairix-fontd-run",
        fixture: "fontd_rxe.rs",
        prefix: "FONTD",
    },
    Program {
        pkg: "tairix-ps",
        bin: "tairix-ps-run",
        fixture: "ps_rxe.rs",
        prefix: "PS",
    },
    Program {
        pkg: "tairix-sysinfo",
        bin: "tairix-sysinfo-run",
        fixture: "sysinfo_rxe.rs",
        prefix: "SYSINFO",
    },
    Program {
        pkg: "tairix-sysmon",
        bin: "tairix-sysmon-run",
        fixture: "sysmon_rxe.rs",
        prefix: "SYSMON",
    },
    Program {
        pkg: "tairix-stress",
        bin: "tairix-stress-run",
        fixture: "stress_rxe.rs",
        prefix: "STRESS",
    },
    Program {
        pkg: "tairix-top",
        bin: "tairix-top-run",
        fixture: "top_rxe.rs",
        prefix: "TOP",
    },
    Program {
        pkg: "tairix-ls",
        bin: "tairix-ls-run",
        fixture: "ls_rxe.rs",
        prefix: "LS",
    },
    Program {
        pkg: "tairix-cat",
        bin: "tairix-cat-run",
        fixture: "cat_rxe.rs",
        prefix: "CAT",
    },
    Program {
        pkg: "tairix-man",
        bin: "tairix-man-run",
        fixture: "man_rxe.rs",
        prefix: "MAN",
    },
    Program {
        pkg: "tairix-clear",
        bin: "tairix-clear-run",
        fixture: "clear_rxe.rs",
        prefix: "CLEAR",
    },
    Program {
        pkg: "tairix-reset",
        bin: "tairix-reset-run",
        fixture: "reset_rxe.rs",
        prefix: "RESET",
    },
    Program {
        pkg: "tairix-users-cli",
        bin: "tairix-users-cli-run",
        fixture: "users_cli_rxe.rs",
        prefix: "USERS_CLI",
    },
];

fn main() {
    // This build script always re-runs. Every output it produces must reflect
    // the *current* build, and none can be captured by a narrow
    // `rerun-if-changed` input: the build provenance id
    // (`KERNEL_BUILD_ID`) carries the build epoch and a `+dirty` working-tree
    // marker, and the embedded program/driver fixtures must never lag their
    // sources. Cargo's `rerun-if-changed` narrowing previously let both go
    // stale — a dirty-tree edit (the day-to-day dev loop) changed neither the
    // tracked git files nor the env, so the script did not re-run, the
    // recompiled kernel embedded a *stale* `build_id.rs`, and a metal reflash
    // reported an old id for new code (the provenance datapoint
    // silently lying). Naming a path that never exists is the documented way
    // to force a re-run on every build; the expensive work behind it is still
    // cheap because it is itself cached (a host build emits inert fixtures,
    // and a freestanding build's nested `cargo` invocations no-op when their
    // sources are unchanged). Cargo only *recompiles* the crate when a
    // generated file's bytes actually change, so a pinned reproducible build
    // (`SOURCE_DATE_EPOCH`) still produces identical output
    // and does not churn.
    println!("cargo:rerun-if-changed=__tairix_always_rerun_build_id__");

    println!("cargo:rustc-check-cfg=cfg(freestanding)");
    println!("cargo:rustc-check-cfg=cfg(kernel_isa, values(\"x86_64\", \"aarch64\", \"riscv64\"))");

    let target = std::env::var("TARGET").unwrap_or_default();
    let target_os = std::env::var("CARGO_CFG_TARGET_OS").unwrap_or_default();
    let target_arch = std::env::var("CARGO_CFG_TARGET_ARCH").unwrap_or_default();

    // `TAIRIX_KERNEL_BOARD` selects the per-board boot linker script for
    // targets with more than one Tier-1 board: the aarch64 image defaults
    // to the Raspberry Pi 4 layout, and `cargo xtask run` builds the QEMU
    // `virt` layout (`aarch64-virt.ld`) it boots interactively. Board
    // selection is build glue, confined to this audited place; an unknown
    // board fails the build loudly rather than defaulting.
    println!("cargo:rerun-if-env-changed=TAIRIX_KERNEL_BOARD");
    let board = std::env::var("TAIRIX_KERNEL_BOARD").ok();
    let linker_script = match linker_script_for(&target, board.as_deref()) {
        Ok(script) => script,
        Err(err) => panic!(
            "TAIRIX_KERNEL_BOARD={:?} is not a known board for target {} \
             (expected `rpi4` or, on aarch64, `virt`)",
            err.board, err.target
        ),
    };
    if let Some(linker_script) = linker_script {
        let manifest_dir = std::env::var("CARGO_MANIFEST_DIR").expect("CARGO_MANIFEST_DIR");
        let linker_script = format!("{}/{linker_script}", manifest_dir.trim_end_matches('/'));
        println!("cargo:rustc-link-arg=-T{linker_script}");
    }

    if let Some(isa) = kernel_isa(&target_arch) {
        println!("cargo:rustc-cfg=kernel_isa=\"{isa}\"");
    }

    if is_freestanding(&target_os, &target_arch) {
        println!("cargo:rustc-cfg=freestanding");
    }

    // The aarch64 production boot spawns every command app and service from
    // its verified on-disk `/System` store bundle (`plans/APPS.md`
    // deliverable 8), so only PID 1 `init` — the compiled-in boot floor — is
    // embedded there. The x86_64/riscv64 ports keep the embedded rows as
    // their explicitly-justified boot floor until their storage floors land.
    let embed_spawn_rows = !(is_freestanding(&target_os, &target_arch) && target_arch == "aarch64");

    emit_build_id();
    emit_program_rxes(&target, embed_spawn_rows);
    emit_user_bias();
    emit_signed_driver_manifests(kernel_isa(&target_arch));
    emit_app_trust_anchor();
}

/// Emit the shared child-image relocation bias (`user_bias.rs`) the
/// `spawn_layout` module re-exports, from the one workspace definition
/// ([`USER_IMAGE_BIAS`]) every rxe converter bakes relocations for — so a
/// port's child mapping and the image pipeline's relocation can never
/// drift, and the bias survives on a target that embeds no program rows.
fn emit_user_bias() {
    let out_dir = std::env::var("OUT_DIR").expect("OUT_DIR");
    let mut out = String::new();
    out.push_str("// Auto-generated by build.rs. DO NOT EDIT.\n");
    out.push_str(
        "/// The user virtual base every spawned child image is mapped at:\n\
         /// the one workspace relocation bias (`tairix_itest_harness::USER_IMAGE_BIAS`)\n\
         /// every embedded and on-disk `Run` rxe is relocated for.\n",
    );
    let mut bias = [0u8; GROUPED_HEX_LEN];
    let _ = writeln!(
        out,
        "pub const CHILD_USER_BIAS: u64 = {};",
        format_grouped_hex(USER_IMAGE_BIAS, &mut bias)
    );
    let path = PathBuf::from(&out_dir).join("user_bias.rs");
    fs::write(&path, out).expect("write user_bias fixture");
}

/// Emit the build's application-signing trust anchor (`app_trust.rs`): the
/// Ed25519 public key derived from the dedicated app-signing seed, a trust
/// domain distinct from the driver-signing anchor. The `spawn` syscall's
/// store-bundle path refuses any `AppInfo` not signed by this key.
fn emit_app_trust_anchor() {
    use ed25519_dalek::SigningKey;

    let out_dir = std::env::var("OUT_DIR").expect("OUT_DIR");
    let signing_key = SigningKey::from_bytes(&SYSTEM_APP_SIGNING_SEED);
    let signer_pubkey: [u8; 32] = signing_key.verifying_key().to_bytes();

    let mut out = String::new();
    out.push_str("// Auto-generated by build.rs. DO NOT EDIT.\n");
    out.push_str(
        "/// Ed25519 public key the image build signs every system app\n\
         /// bundle's `AppInfo` with: the kernel's application trust anchor\n\
         /// (`plans/APPS.md` deliverable 8), a trust domain distinct from the\n\
         /// driver-signing anchor.\n",
    );
    emit_byte_array(
        &mut out,
        "SYSTEM_APP_SIGNER_PUBKEY",
        "[u8; 32]",
        &signer_pubkey,
    );
    let path = PathBuf::from(&out_dir).join("app_trust.rs");
    fs::write(&path, out).expect("write app_trust fixture");
}

/// One chain driver to bake a signed manifest for: the emitted `const`
/// name and the driver crate's own canonical `BIND_KEYS` (so the signed
/// bind table never drifts from the matcher).
struct DriverImage {
    /// `const` name the fixture emits the signed image bytes under.
    const_name: &'static str,
    /// The driver crate's published bind table.
    bind_keys: &'static [tairix_abi::DriverBindKey],
}

/// Bake the signed `.rxe` manifest image of every Pi 4 USB-chain driver
/// the in-kernel `driver_loader` admits through `drvhost::Host::load`
/// (`plans/PI.md` P10 5c-ii), plus the build's driver-signing public key,
/// into a `driver_images.rs` source the loader `include!`s.
///
/// Each image is a `DriverManifest` (kind [`InKernel`], stamped with the
/// kernel's compiled-in `SYSCALL_TABLE_HASH`, requesting `CAP_DRV_LOAD`)
/// followed by its capability body and the driver's own `BIND_KEYS`, all
/// covered by an Ed25519 signature over
/// `header[..signed_end] || cap_body || bind_table || payload` — exactly
/// the message `drvhost::Host::verify_signature` reconstructs. The payload
/// is empty: the drivers are statically linked, so the in-process spawner
/// invokes their `register()` directly rather than loading a program image,
/// and covering an empty payload is a no-op (a user-space driver's non-empty
/// payload, by contrast, is authenticated).
fn emit_signed_driver_manifests(isa: Option<&str>) {
    use ed25519_dalek::SigningKey;

    let out_dir = std::env::var("OUT_DIR").expect("OUT_DIR");
    let signing_key = SigningKey::from_bytes(&KERNEL_DRIVER_SIGNING_SEED);
    let signer_pubkey: [u8; 32] = signing_key.verifying_key().to_bytes();

    // The compiled-in floor is storage-only: the block
    // drivers that read the volume holding the signed driver store. The
    // BCM2711 PCIe / VL805 USB / USB-keyboard drivers are installed as signed
    // `/System/Drivers/` bundles and autoloaded into user space, so their
    // manifests are baked by the image pipeline (`tools/xtask`), not here.
    // The floor is per architecture: virtio-blk is the root block driver on
    // every target, while the BCM2711 EMMC2 SD host is floor **only on
    // aarch64** (the Raspberry Pi 4 SD card). An x86_64 / riscv64 image has
    // no such controller, so its manifest is never baked in — the build
    // emits `EMMC2_IMAGE` for the aarch64 target alone, matching the
    // `#[cfg(kernel_isa = "aarch64")]` `driver_catalog` entry that consumes
    // it. `build.rs` is one host artifact that branches on the resolved
    // target instruction set at run time, so the crate stays a build
    // dependency on every target while contributing image bytes only where
    // the driver belongs.
    let mut images = vec![DriverImage {
        const_name: "VIRTIO_BLK_IMAGE",
        bind_keys: tairix_drv_storage_virtio_blk::BIND_KEYS,
    }];
    if isa == Some("aarch64") {
        images.push(DriverImage {
            const_name: "EMMC2_IMAGE",
            bind_keys: tairix_drv_storage_emmc2::BIND_KEYS,
        });
    }

    let mut out = String::new();
    out.push_str("// Auto-generated by build.rs. DO NOT EDIT.\n");
    out.push_str(
        "/// Ed25519 public key the build signed every embedded driver\n\
         /// manifest with — the kernel's driver-load trust anchor\n\
         /// (`plans/PI.md` P10 5c-ii).\n",
    );
    emit_byte_array(
        &mut out,
        "KERNEL_DRIVER_SIGNER_PUBKEY",
        "[u8; 32]",
        &signer_pubkey,
    );

    for image in &images {
        let bytes = build_signed_driver_image(&signing_key, signer_pubkey, image.bind_keys);
        let _ = writeln!(
            out,
            "/// Signed `.rxe` driver-manifest image admitted through `Host::load`."
        );
        emit_byte_slice(&mut out, image.const_name, &bytes);
    }

    let path = PathBuf::from(&out_dir).join("driver_images.rs");
    fs::write(&path, out).expect("write driver_images fixture");
}

/// Encode and sign one chain driver's `DriverManifest` image, mirroring
/// the message `drvhost::Host::verify_signature` reconstructs.
fn build_signed_driver_image(
    signing_key: &ed25519_dalek::SigningKey,
    signer_pubkey: [u8; 32],
    bind_keys: &[tairix_abi::DriverBindKey],
) -> Vec<u8> {
    use ed25519_dalek::Signer;
    use tairix_abi::{CapabilityId, DriverKind, DriverManifest, DRIVER_MANIFEST_MAGIC};

    // The drivers are statically linked; their `register()` only needs
    // `CAP_DRV_LOAD` (the admission check). `kind = InKernel` makes the
    // gate additionally require `CAP_DRV_KERNEL` of the caller. No further capabilities are requested: the real MMIO/DMA work
    // runs over the keyboard service's own capability-gated host, not this
    // admission view.
    let caps = [CapabilityId::DRV_LOAD];
    let bind_key_count = u8::try_from(bind_keys.len()).expect("chain bind tables fit in u8");
    let capability_count = u16::try_from(caps.len()).expect("caps fit in u16");

    let mut manifest = DriverManifest {
        magic: DRIVER_MANIFEST_MAGIC,
        abi_version: tairix_abi::ABI_VERSION_CURRENT,
        kind: DriverKind::InKernel,
        bind_key_count,
        capability_count,
        syscall_table_hash: tairix_kernel_syscall::SYSCALL_TABLE_HASH,
        signer_pubkey,
        signature: [0u8; 64],
    };

    let mut cap_body = Vec::with_capacity(caps.len() * 2);
    for c in &caps {
        cap_body.extend_from_slice(&c.as_u16().to_le_bytes());
    }
    let mut bind_body = Vec::new();
    for k in bind_keys {
        bind_body.extend_from_slice(&k.to_le_bytes());
    }

    let header = manifest.to_le_bytes();
    let signed_end = DriverManifest::WIRE_LEN - 64;
    let mut signed_message = Vec::new();
    signed_message.extend_from_slice(&header[..signed_end]);
    signed_message.extend_from_slice(&cap_body);
    signed_message.extend_from_slice(&bind_body);
    manifest.signature = signing_key.sign(&signed_message).to_bytes();

    let mut out = Vec::new();
    out.extend_from_slice(&manifest.to_le_bytes());
    out.extend_from_slice(&cap_body);
    out.extend_from_slice(&bind_body);
    out
}

/// Emit `pub const <name>: <ty> = [ … ];` for a fixed-size byte array.
fn emit_byte_array(out: &mut String, name: &str, ty: &str, bytes: &[u8]) {
    let _ = write!(out, "pub const {name}: {ty} = [");
    for (i, b) in bytes.iter().enumerate() {
        if i % 16 == 0 {
            out.push_str("\n    ");
        }
        let _ = write!(out, "0x{b:02x}, ");
    }
    out.push_str("\n];\n");
}

/// Emit `pub const <name>: &[u8] = &[ … ];` for a variable-length blob.
fn emit_byte_slice(out: &mut String, name: &str, bytes: &[u8]) {
    let _ = write!(out, "pub const {name}: &[u8] = &[");
    for (i, b) in bytes.iter().enumerate() {
        if i % 16 == 0 {
            out.push_str("\n    ");
        }
        let _ = write!(out, "0x{b:02x}, ");
    }
    out.push_str("\n];\n");
}

/// Emit a `build_id.rs` fixture (`pub const KERNEL_BUILD_ID: &str`) the
/// boot path logs once at hand-off, so a serial capture proves *which*
/// build is running — the provenance datapoint that settles a "does the
/// running image actually contain this source change?" question without
/// guessing.
///
/// The id combines the source identity (`git rev-parse --short HEAD`, plus
/// a `+dirty.<fp>` marker when the working tree carries uncommitted changes,
/// where `<fp>` fingerprints that uncommitted content so two different dirty
/// trees never collide — best-effort, `nogit` when git or the checkout is
/// unavailable) with a
/// build epoch in seconds. The epoch honours `SOURCE_DATE_EPOCH` when set
/// (the standard reproducible-build input, so a pinned build stays
/// bit-reproducible), falling back to the current
/// wall-clock second otherwise.
///
/// This is regenerated on *every* build (`main` declares no narrow
/// `rerun-if-changed` input — see its rationale), so the `+dirty` hash and
/// the epoch always track the image actually produced. Tracking only git's
/// metadata as a rerun input — the previous design — left the id stale
/// through a dirty-tree edit, so a metal reflash reported an old id for new
/// code. The `SOURCE_DATE_EPOCH` parsing this relies on
/// is the host-unit-tested [`build_support::parse_source_date_epoch`].
fn emit_build_id() {
    let out_dir = std::env::var("OUT_DIR").expect("OUT_DIR");

    let source = git_source_id();
    let epoch = build_epoch_secs();
    let build_id = format!("{source} built@{epoch}");

    let fixture = format!(
        "// Auto-generated by build.rs. DO NOT EDIT.\n\
         /// Source + build identity, logged once at boot hand-off so a\n\
         /// serial capture proves which build is running.\n\
         pub const KERNEL_BUILD_ID: &str = {build_id:?};\n"
    );
    let path = PathBuf::from(&out_dir).join("build_id.rs");
    fs::write(&path, fixture).expect("write build_id fixture");
}

/// `git rev-parse --short HEAD` with a `+dirty.<fp>` suffix when the working
/// tree is not clean; `nogit` when git or the checkout is unavailable
/// (the build must never fail for a missing VCS).
///
/// The `<fp>` is a short content fingerprint of the uncommitted changes
/// ([`dirty_content_fingerprint`]), so two *different* dirty trees no longer
/// report the same `<hash>+dirty` id. That collision was the concrete failure
/// this closes: a metal reflash could not be told apart from the previous one
/// on the id alone — the operator had no way to confirm the image on the SD
/// card actually carried the source change under test. A distinct `<fp>`
/// proves a distinct tree; the same `<fp>` proves the same content.
fn git_source_id() -> String {
    let manifest_dir = std::env::var("CARGO_MANIFEST_DIR").expect("CARGO_MANIFEST_DIR");
    let head = Command::new("git")
        .current_dir(&manifest_dir)
        .args(["rev-parse", "--short=12", "HEAD"])
        .output();
    let Ok(head) = head else {
        return "nogit".to_string();
    };
    if !head.status.success() {
        return "nogit".to_string();
    }
    let hash = String::from_utf8_lossy(&head.stdout).trim().to_string();
    match dirty_content_fingerprint(&manifest_dir) {
        Some(fp) => format!("{hash}+dirty.{fp}"),
        None => hash,
    }
}

/// A short content fingerprint of the working tree's uncommitted changes, or
/// `None` when the tree is clean (a committed build needs no marker).
///
/// The fingerprint covers both channels of "not committed": every tracked
/// modification since `HEAD` (`git diff HEAD`, which folds in staged and
/// unstaged edits alike) and the full contents of every untracked,
/// non-ignored file (`git ls-files --others --exclude-standard`, so a brand
/// new source file that no diff would show still moves the fingerprint).
/// `.gitignore` is honoured, so build outputs under `images/` and `target/`
/// never enter it. The bytes are hashed with the host-tested, fast
/// non-cryptographic [`build_support::short_content_hash`] — this only has to
/// tell developer working trees apart, never resist an adversary. Any git
/// hiccup collapses to "no fingerprint" (the bare `<hash>` id) rather than
/// failing the build, matching the surrounding fail-safe VCS handling.
fn dirty_content_fingerprint(manifest_dir: &str) -> Option<String> {
    let diff = Command::new("git")
        .current_dir(manifest_dir)
        .args(["diff", "HEAD"])
        .output()
        .ok()?;
    if !diff.status.success() {
        return None;
    }
    let untracked = Command::new("git")
        .current_dir(manifest_dir)
        .args(["ls-files", "--others", "--exclude-standard", "-z"])
        .output()
        .ok()?;
    if !untracked.status.success() {
        return None;
    }

    let mut material = diff.stdout;
    // `git ls-files` prints paths relative to its working directory (here
    // `manifest_dir`), so resolve each against that base — an untracked file
    // elsewhere in the repo comes through as a `../…`-prefixed relative path
    // that still resolves correctly.
    let base = PathBuf::from(manifest_dir);
    for name in untracked.stdout.split(|&b| b == 0) {
        if name.is_empty() {
            continue;
        }
        let path = base.join(String::from_utf8_lossy(name).as_ref());
        // The path came straight from git and names an existing untracked
        // file; a read that nonetheless fails (a race with a concurrent
        // delete) folds the path name in instead, so the fingerprint still
        // moves rather than silently dropping the change.
        match fs::read(&path) {
            Ok(bytes) => material.extend_from_slice(&bytes),
            Err(_) => material.extend_from_slice(name),
        }
    }

    // A dirty `git status` with an empty diff and no untracked files cannot
    // occur, but fold the emptiness to `None` so the id degrades to the bare
    // committed hash rather than fingerprinting nothing.
    if material.is_empty() {
        None
    } else {
        let mut buf = [0u8; 12];
        Some(build_support::short_content_hash(&material, &mut buf).to_string())
    }
}

/// The build epoch in whole seconds: `SOURCE_DATE_EPOCH` when set (so a
/// pinned, reproducible build is stable), else the
/// current wall-clock second.
fn build_epoch_secs() -> u64 {
    if let Ok(epoch) = std::env::var("SOURCE_DATE_EPOCH") {
        if let Some(secs) = build_support::parse_source_date_epoch(&epoch) {
            return secs;
        }
    }
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map_or(0, |d| d.as_secs())
}

/// Build every embedded [`PROGRAMS`] `Run` PIE and embed its `rxe` image so a
/// boot path can spawn PID 1 `init` into user mode (`plans/PI.md` P6c-3 on
/// aarch64, X3a on x86_64, RV-P3 on riscv64) and `init` can launch the session
/// program (`plans/SPAWN.md` `SP3b`).
///
/// On a freestanding production target ([`program_rustflags_var`] returns the
/// target-scoped link var) each program is compiled position-independent
/// against its own `Run.ld` into a private target directory under `OUT_DIR`
/// (so it never collides with the outer kernel build, one
/// program source built for each target), then the linked PIE ELF is
/// converted into an `rxe` blob with
/// [`tairix_itest_harness::elf2rxe::elf_to_rxe`], baking relocations for
/// [`USER_IMAGE_BIAS`] and stamping the kernel's compiled-in syscall CFI tag
/// (`tairix_kernel_syscall::SYSCALL_TABLE_HASH`) so
/// [`tairix_abi::rxe::LoadImage::parse`] accepts it.
///
/// On every other target (host `cargo build --workspace`, clippy) each
/// fixture is an inert empty blob: the boot-path modules that consume them
/// compile only for a freestanding production target.
fn emit_program_rxes(target: &str, embed_spawn_rows: bool) {
    let manifest_dir = std::env::var("CARGO_MANIFEST_DIR").expect("CARGO_MANIFEST_DIR");
    let out_dir = std::env::var("OUT_DIR").expect("OUT_DIR");
    let manifest_dir = manifest_dir.trim_end_matches('/');

    // Every embedded program links the one shared PIE script (this crate lives
    // two levels below the workspace root), and they all build into one shared
    // target directory. Because the link recipe `rustc` sees is then identical
    // for every program, cargo builds the `-Z build-std` sysroot and the
    // shared user-space libraries once and reuses them for each subsequent
    // program instead of rebuilding them per program. The single staleness
    // guard is therefore run once, before the loop, against that shared
    // directory.
    let run_ld = format!(
        "{manifest_dir}/../../{}",
        tairix_itest_harness::pie::RUN_LD_WORKSPACE_RELPATH
    );
    let target_dir = format!("{out_dir}/programs-target");
    wipe_target_dir_on_linker_change(&run_ld, &target_dir, &out_dir);

    for program in PROGRAMS {
        // Only PID 1 `init` is embedded on a target whose runtime `spawn`
        // resolves the on-disk store bundles instead of embedded rows.
        if !embed_spawn_rows && program.prefix != "INIT" {
            continue;
        }
        emit_program_rxe(
            target,
            manifest_dir,
            &out_dir,
            &run_ld,
            &target_dir,
            program,
        );
    }
}

/// Build one [`Program`] and write its generated fixture under `OUT_DIR`.
fn emit_program_rxe(
    target: &str,
    manifest_dir: &str,
    out_dir: &str,
    run_ld: &str,
    target_dir: &str,
    program: &Program,
) {
    let rxe = match program_rustflags_var(target) {
        Some(rustflags_var) => build_and_convert(
            manifest_dir,
            run_ld,
            target_dir,
            program,
            target,
            rustflags_var,
        ),
        None => Vec::new(),
    };
    let fixture_path = PathBuf::from(out_dir).join(program.fixture);
    write_fixture(&fixture_path, program, &rxe);
}

/// Wipe the shared programs `target_dir` — forcing a clean relink — when the
/// shared `Run.ld` content has changed since the last build, and otherwise
/// leave it intact so the nested `cargo` builds incrementally.
///
/// Cargo fingerprints the linker script by *path* (through the RUSTFLAGS
/// string), not content, so a `Run.ld` edit would otherwise leave a stale
/// linked ELF in place. The script content is kept as a single sidecar copy
/// under `OUT_DIR`; the wipe fires only on a real content change (or first
/// build / missing sidecar), so this build script re-running on every build
/// does not rebuild `build-std` each time. A read failure compares as
/// different and simply forces the (correct, safe) clean rebuild — fail safe,
/// never silently stale.
fn wipe_target_dir_on_linker_change(run_ld: &str, target_dir: &str, out_dir: &str) {
    let current = fs::read(run_ld).ok();
    let sidecar = PathBuf::from(out_dir).join("programs-target.run_ld");
    let previous = fs::read(&sidecar).ok();
    if current.is_none() || current != previous {
        let _ = fs::remove_dir_all(target_dir);
        if let Some(bytes) = &current {
            let _ = fs::write(&sidecar, bytes);
        }
    }
}

/// Compile a program's `Run` bin PIE for the given freestanding `target` and
/// convert the linked ELF into an `rxe` blob. `rustflags_var` is the
/// target-scoped `CARGO_TARGET_<TRIPLE>_RUSTFLAGS` variable that carries the
/// PIE link recipe (one build path for every production target).
fn build_and_convert(
    manifest_dir: &str,
    run_ld: &str,
    target_dir: &str,
    program: &Program,
    target: &str,
    rustflags_var: &str,
) -> Vec<u8> {
    // The program links no architecture crate, so `Run.ld`'s `ENTRY(_start)`
    // roots the `tairix-rt` runtime trampoline; it is built
    // position-independent, with `core` /
    // `compiler_builtins` / `alloc` built PIC alongside it (`-Z
    // build-std`). `alloc` is required because the program packages name it
    // transitively, even though the banner-printing `Run` binaries never
    // allocate (the unreachable allocating paths are dead-stripped, so no
    // global allocator is needed). Scope the PIE link flags to the chosen
    // production target so the program's own host build script is unaffected.
    let cargo = std::env::var_os("CARGO").unwrap_or_else(|| "cargo".into());
    let status = Command::new(cargo)
        .current_dir(manifest_dir)
        // The outer build exports `CARGO_ENCODED_RUSTFLAGS` / `RUSTFLAGS`
        // into this build script's environment; both outrank the
        // target-scoped var below, so a nested cargo would inherit the
        // outer kernel's flags and drop the PIE link recipe. Clear them so
        // the target-scoped flags win and apply only to the program crates
        // for this target (not the program's own host build script).
        .env_remove("CARGO_ENCODED_RUSTFLAGS")
        .env_remove("RUSTFLAGS")
        .env(
            rustflags_var,
            format!("-C relocation-model=pie -C link-arg=-pie -C link-arg=-T{run_ld}"),
        )
        .args([
            "build",
            "-p",
            program.pkg,
            "--bin",
            program.bin,
            "--target",
            target,
            "-Z",
            "build-std=core,compiler_builtins,alloc",
            "--target-dir",
            target_dir,
        ])
        .status()
        .unwrap_or_else(|e| panic!("spawn cargo to build the {} Run program: {e}", program.pkg));
    assert!(
        status.success(),
        "building the {} Run program failed",
        program.pkg
    );

    let elf_path = format!("{target_dir}/{target}/debug/{}", program.bin);
    let elf = fs::read(&elf_path).unwrap_or_else(|e| panic!("read {elf_path}: {e}"));

    tairix_itest_harness::elf2rxe::elf_to_rxe(
        &elf,
        &tairix_kernel_syscall::SYSCALL_TABLE_HASH,
        USER_BIAS,
    )
    .unwrap_or_else(|e| {
        panic!(
            "convert the {} Run program ELF into an rxe image: {e:?}",
            program.pkg
        )
    })
}

/// Emit `<PREFIX>_RXE` and `<PREFIX>_USER_BIAS` as a Rust source the boot
/// path `include!`s.
fn write_fixture(path: &Path, program: &Program, rxe: &[u8]) {
    let prefix = program.prefix;
    let mut out = String::new();
    out.push_str("// Auto-generated by build.rs. DO NOT EDIT.\n");
    let _ = writeln!(
        out,
        "/// Virtual base the `{}` image is mapped at.",
        program.pkg
    );
    let mut bias = [0u8; GROUPED_HEX_LEN];
    let _ = writeln!(
        out,
        "pub const {prefix}_USER_BIAS: u64 = {};",
        format_grouped_hex(USER_BIAS, &mut bias)
    );
    let _ = writeln!(
        out,
        "/// The converted `rxe` image of the `{}` `Run` program.",
        program.pkg
    );
    let _ = writeln!(out, "pub const {prefix}_RXE: &[u8] = &[");
    for (i, b) in rxe.iter().enumerate() {
        if i % 16 == 0 {
            out.push_str("\n    ");
        }
        let _ = write!(out, "0x{b:02x}, ");
    }
    out.push_str("\n];\n");
    fs::write(path, out).expect("write program rxe fixture");
}
