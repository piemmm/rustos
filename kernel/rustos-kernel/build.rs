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
/// one bias (`AGENTS.md` §2.2). Mirrors the proven per-arch
/// `spawn_program_qemu_*` fixtures' bias.
const USER_BIAS: u64 = 0x10_0000_0000;

/// One embedded `Run` program the boot path builds into an `rxe` image: the
/// crate package, its `Run` bin, the absolute source dir, the generated
/// fixture file name, and the `const`-name prefix the fixture emits under.
struct Program {
    /// Cargo package name (`-p <pkg>`).
    pkg: &'static str,
    /// `Run` binary name (`--bin <bin>`).
    bin: &'static str,
    /// Path to the program crate dir, relative to this crate's manifest dir.
    rel_dir: &'static str,
    /// Generated fixture file name written under `OUT_DIR`.
    fixture: &'static str,
    /// Prefix for the emitted `const`s (`<PREFIX>_RXE`, `<PREFIX>_USER_BIAS`).
    prefix: &'static str,
    /// Extra source files (relative to the crate dir) to re-run the build on.
    rerun: &'static [&'static str],
}

/// The embedded programs every production boot path spawns: PID 1 `init`, and
/// the `Shell` session program `init` launches (`plans/SPAWN.md` `SP3b`). Both
/// are pure-Rust `Run` bins built the same way for whichever production target
/// is active (`AGENTS.md` §2.2 — one build path), differing only in their
/// package/paths.
const PROGRAMS: &[Program] = &[
    Program {
        pkg: "rustos-init",
        bin: "rustos-init-run",
        rel_dir: "../../userland/system/init",
        fixture: "init_rxe.rs",
        prefix: "INIT",
        rerun: &["src/run.rs", "src/startup.rs", "Run.ld", "Cargo.toml"],
    },
    Program {
        pkg: "rustos-shell",
        bin: "rustos-shell-run",
        rel_dir: "../../userland/shell/shell",
        fixture: "shell_rxe.rs",
        prefix: "SHELL",
        rerun: &["src/run.rs", "Run.ld", "Cargo.toml", "build.rs"],
    },
    Program {
        pkg: "rustos-login",
        bin: "rustos-login-run",
        rel_dir: "../../userland/session/login",
        fixture: "login_rxe.rs",
        prefix: "LOGIN",
        rerun: &["src/run.rs", "Run.ld", "Cargo.toml", "build.rs"],
    },
];

fn main() {
    println!("cargo:rustc-check-cfg=cfg(freestanding)");
    println!("cargo:rustc-check-cfg=cfg(kernel_isa, values(\"x86_64\", \"aarch64\", \"riscv64\"))");

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

    emit_build_id();
    emit_program_rxes(&target);
}

/// Emit a `build_id.rs` fixture (`pub const KERNEL_BUILD_ID: &str`) the
/// boot path logs once at hand-off, so a serial capture proves *which*
/// build is running — the provenance datapoint that settles a "does the
/// running image actually contain this source change?" question without
/// guessing (`AGENTS.md` §15.7).
///
/// The id combines the source identity (`git rev-parse --short HEAD`, plus
/// a `+dirty` marker when the working tree carries uncommitted changes —
/// best-effort, `nogit` when git or the checkout is unavailable) with a
/// build epoch in seconds. The epoch honours `SOURCE_DATE_EPOCH` when set
/// (the standard reproducible-build input, so a pinned build stays
/// bit-reproducible — `AGENTS.md` §19.3), falling back to the current
/// wall-clock second otherwise. `git`'s own metadata is registered as a
/// rerun input so a commit refreshes the id.
fn emit_build_id() {
    let out_dir = std::env::var("OUT_DIR").expect("OUT_DIR");
    println!("cargo:rerun-if-env-changed=SOURCE_DATE_EPOCH");
    println!("cargo:rerun-if-changed=../../.git/HEAD");
    println!("cargo:rerun-if-changed=../../.git/index");

    let source = git_source_id();
    let epoch = build_epoch_secs();
    let build_id = format!("{source} built@{epoch}");

    let fixture = format!(
        "// Auto-generated by build.rs. DO NOT EDIT.\n\
         /// Source + build identity, logged once at boot hand-off so a\n\
         /// serial capture proves which build is running (`AGENTS.md` §15.7).\n\
         pub const KERNEL_BUILD_ID: &str = {build_id:?};\n"
    );
    let path = PathBuf::from(&out_dir).join("build_id.rs");
    fs::write(&path, fixture).expect("write build_id fixture");
}

/// `git rev-parse --short HEAD` with a `+dirty` suffix when the working
/// tree is not clean; `nogit` when git or the checkout is unavailable
/// (the build must never fail for a missing VCS — `AGENTS.md` §2.9).
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
    let dirty = Command::new("git")
        .current_dir(&manifest_dir)
        .args(["status", "--porcelain"])
        .output()
        .is_ok_and(|o| o.status.success() && !o.stdout.is_empty());
    if dirty {
        format!("{hash}+dirty")
    } else {
        hash
    }
}

/// The build epoch in whole seconds: `SOURCE_DATE_EPOCH` when set (so a
/// pinned, reproducible build is stable — `AGENTS.md` §19.3), else the
/// current wall-clock second.
fn build_epoch_secs() -> u64 {
    if let Ok(epoch) = std::env::var("SOURCE_DATE_EPOCH") {
        if let Ok(secs) = epoch.trim().parse::<u64>() {
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
/// (so it never collides with the outer kernel build — `AGENTS.md` §2.2, one
/// program source built for each target), then the linked PIE ELF is
/// converted into an `rxe` blob with
/// [`rustos_itest_harness::elf2rxe::elf_to_rxe`], baking relocations for
/// [`USER_BIAS`] and stamping the kernel's compiled-in syscall CFI tag
/// (`rustos_kernel_syscall::SYSCALL_TABLE_HASH`) so
/// [`rustos_abi::rxe::LoadImage::parse`] accepts it (§9 / §19.2).
///
/// On every other target (host `cargo build --workspace`, clippy) each
/// fixture is an inert empty blob: the boot-path modules that consume them
/// compile only for a freestanding production target.
fn emit_program_rxes(target: &str) {
    let manifest_dir = std::env::var("CARGO_MANIFEST_DIR").expect("CARGO_MANIFEST_DIR");
    let out_dir = std::env::var("OUT_DIR").expect("OUT_DIR");
    let manifest_dir = manifest_dir.trim_end_matches('/');

    for program in PROGRAMS {
        emit_program_rxe(target, manifest_dir, &out_dir, program);
    }
}

/// Build one [`Program`] and write its generated fixture under `OUT_DIR`.
fn emit_program_rxe(target: &str, manifest_dir: &str, out_dir: &str, program: &Program) {
    let prog_dir = format!("{manifest_dir}/{}", program.rel_dir);
    for rel in program.rerun {
        println!("cargo:rerun-if-changed={prog_dir}/{rel}");
    }

    let rxe = match program_rustflags_var(target) {
        Some(rustflags_var) => build_and_convert(
            manifest_dir,
            out_dir,
            &prog_dir,
            program,
            target,
            rustflags_var,
        ),
        None => Vec::new(),
    };
    let fixture_path = PathBuf::from(out_dir).join(program.fixture);
    write_fixture(&fixture_path, program, &rxe);
}

/// Compile a program's `Run` bin PIE for the given freestanding `target` and
/// convert the linked ELF into an `rxe` blob. `rustflags_var` is the
/// target-scoped `CARGO_TARGET_<TRIPLE>_RUSTFLAGS` variable that carries the
/// PIE link recipe (one build path for every production target, `AGENTS.md`
/// §2.2).
fn build_and_convert(
    manifest_dir: &str,
    out_dir: &str,
    prog_dir: &str,
    program: &Program,
    target: &str,
    rustflags_var: &str,
) -> Vec<u8> {
    let run_ld = format!("{prog_dir}/Run.ld");
    let target_dir = format!("{out_dir}/{}-target", program.pkg);

    // Cargo fingerprints the RUSTFLAGS *string* (which names the linker
    // script by path) but not the script's *content*, so a `Run.ld` edit
    // would not by itself trigger a relink and the converter could read a
    // stale ELF. `build.rs` only reruns when its `rerun-if-changed` inputs
    // (including `Run.ld`) actually change, so wiping the private target
    // directory here forces a clean rebuild against the current script
    // without churning ordinary incremental builds.
    let _ = fs::remove_dir_all(&target_dir);

    // The program links no architecture crate, so `Run.ld`'s `ENTRY(_start)`
    // roots the `rustos-rt` runtime trampoline; it is built
    // position-independent (`AGENTS.md` §19.2), with `core` /
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
            &target_dir,
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

    rustos_itest_harness::elf2rxe::elf_to_rxe(
        &elf,
        &rustos_kernel_syscall::SYSCALL_TABLE_HASH,
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
    let _ = writeln!(out, "pub const {prefix}_USER_BIAS: u64 = {USER_BIAS:#x};");
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
