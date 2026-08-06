//! Shared host-side PIE cross-compile recipe for the freestanding program
//! images the image pipeline ships — the user-space driver `Run` binaries
//! (`image_drivers`) and the application-bundle `Run` binaries
//! (`image_apps`) alike, so the link recipe, the linker-script staleness
//! guard, and the artefact resolution live in one definition.
//!
//! The recipe mirrors the kernel `build.rs` embedded-program build: the
//! crate's `Run` binary is compiled position-independent against the one
//! shared `tairix_itest_harness::pie::RUN_LD_WORKSPACE_RELPATH` link script
//! into a target directory shared by every program of the same
//! `(group, triple)` (so it never collides with the outer build), with
//! `core`/`compiler_builtins`/`alloc` built PIC alongside it (`-Z build-std`)
//! and the outer build's `RUSTFLAGS` cleared so the target-scoped PIE link
//! recipe wins. Because every program hands `rustc` the identical link recipe,
//! that build-std sysroot and the shared user-space libraries are compiled
//! once and reused across all of them rather than rebuilt per program.
//!
//! Cargo fingerprints the RUSTFLAGS *string* (which names the linker script
//! by path) but not the script's *content*, so a `Run.ld` edit alone would
//! not trigger a relink and a converter could read a stale ELF. A sidecar
//! copy of the script is kept beside the private target directory; the
//! directory is wiped — forcing a clean rebuild against the current script
//! — only when the content actually changed (or on first build / an
//! unreadable script, which compares as different: fail safe, never
//! silently stale). An unchanged script leaves the directory intact so the
//! nested `cargo` no-ops incrementally instead of rebuilding `build-std`
//! on every invocation.

use std::fmt::Write as _;
use std::path::{Path, PathBuf};
use std::process::Command;

use tairix_itest_harness::pie::PieArch;
use tairix_mkimage::ImageProfile;

use crate::{Context, LONG_BUILD_COMMAND_TIMEOUT};

/// Compile `package`'s binary `bin` position-independent for the
/// freestanding target `arch` against the one shared
/// `tairix_itest_harness::pie::RUN_LD_WORKSPACE_RELPATH` link script and
/// return the linked ELF bytes.
///
/// `group` names the target-directory family under the workspace `target/`
/// (e.g. `image-drivers`, `image-apps`), keeping each pipeline's artefacts
/// apart; the arch's triple is a further segment so the same group
/// cross-compiled for two architectures never shares a target directory.
/// Every program in one `(group, triple)` shares that one directory: because
/// they all link the identical script, the `RUSTFLAGS` string `rustc` sees is
/// identical for each, so cargo builds the `-Z build-std` sysroot and the
/// shared user-space libraries once and reuses them for every subsequent
/// program instead of rebuilding them per program (Cargo namespaces the
/// `debug`/`release` profiles under distinct subdirectories, so the two
/// profiles coexist).
///
/// `profile` selects the Cargo build profile the program compiles in — the
/// same image → Cargo-profile mapping the kernel build uses
/// ([`ImageProfile::cargo_build_args`] / [`ImageProfile::cargo_profile_dir`]),
/// so the shippable `installer` image builds its user-space `Run` binaries
/// `--release` while the `debug` and QEMU-test images stay in Cargo's `dev`
/// profile.
///
/// # Errors
///
/// A string describing a failed cross-compile or a missing ELF artefact.
pub fn cross_compile_pie_elf(
    ctx: &Context,
    arch: PieArch,
    group: &str,
    package: &str,
    bin: &str,
    profile: ImageProfile,
) -> Result<Vec<u8>, String> {
    let triple = arch.target_triple();
    let run_ld = ctx
        .workspace_root
        .join(tairix_itest_harness::pie::RUN_LD_WORKSPACE_RELPATH);
    if !run_ld.is_file() {
        return Err(format!(
            "image: the shared PIE link script is missing at {}",
            run_ld.display()
        ));
    }
    let target_dir = ctx.target_dir().join(group).join(triple);
    wipe_target_dir_on_linker_change(&run_ld, &target_dir);

    // Every bundle the pipeline cross-compiles is part of the generic per-arch
    // user-space, so it builds against that image's CPU floor — the identical
    // floor the generic image's kernel uses (resolved from the arch, so the
    // value is not carried two ways). The floor tokens are *prepended* to the
    // PIE link recipe, which stands in for the base flags for user-space; a
    // baseline floor prepends nothing and the build is unchanged.
    let floor = crate::floor::floor_for_image(crate::floor::ImageKind::generic_for_pie_arch(arch));
    let mut rustflags = floor.floor_tokens().join(" ");
    if !rustflags.is_empty() {
        rustflags.push(' ');
    }
    // `write!` into a `String` is infallible; the result is discarded.
    let _ = write!(
        rustflags,
        "-C relocation-model=pie -C link-arg=-pie -C link-arg=-T{}",
        run_ld.display()
    );

    let mut cmd = Command::new(&ctx.cargo);
    cmd.current_dir(&ctx.workspace_root)
        // The outer build exports `CARGO_ENCODED_RUSTFLAGS` / `RUSTFLAGS`
        // into this process's environment; both outrank the target-scoped
        // var below, so a nested cargo would inherit the outer flags and
        // drop the PIE link recipe. Clear them so the target-scoped flags
        // win and apply only to this target's crates.
        .env_remove("CARGO_ENCODED_RUSTFLAGS")
        .env_remove("RUSTFLAGS")
        .env(arch.rustflags_env_var(), rustflags)
        .args([
            "build",
            "--locked",
            "-p",
            package,
            "--bin",
            bin,
            "--target",
            triple,
            "-Z",
            "build-std=core,compiler_builtins,alloc",
        ])
        .args(profile.cargo_build_args())
        .args(["--target-dir"])
        .arg(&target_dir);
    // `-Z build-std` recompiles `core`/`compiler_builtins`/`alloc` from
    // source for the first program in a given (group, triple); that clean
    // rebuild can legitimately outrun an incremental host compile pass.
    ctx.run_with_timeout(
        &format!(
            "image: program build ({package}, {triple}, {})",
            profile.cargo_profile_dir()
        ),
        cmd,
        LONG_BUILD_COMMAND_TIMEOUT,
    )?;

    let elf_path = target_dir
        .join(triple)
        .join(profile.cargo_profile_dir())
        .join(bin);
    std::fs::read(&elf_path)
        .map_err(|e| format!("image: cannot read program ELF {}: {e}", elf_path.display()))
}

/// Wipe `target_dir` — forcing a clean relink — when the `run_ld` content
/// has changed since the last build, and otherwise leave it intact so the
/// nested `cargo` builds incrementally. A sidecar copy of the script is
/// kept beside the target directory; an unreadable script compares as
/// different and forces the (correct, safe) clean rebuild.
fn wipe_target_dir_on_linker_change(run_ld: &Path, target_dir: &Path) {
    let current = std::fs::read(run_ld).ok();
    let sidecar = sidecar_path(target_dir);
    let previous = std::fs::read(&sidecar).ok();
    if current.is_none() || current != previous {
        let _ = std::fs::remove_dir_all(target_dir);
        if let Some(bytes) = &current {
            if let Some(parent) = sidecar.parent() {
                let _ = std::fs::create_dir_all(parent);
            }
            let _ = std::fs::write(&sidecar, bytes);
        }
    }
}

/// The sidecar file recording the `Run.ld` content the private target
/// directory was last linked against.
fn sidecar_path(target_dir: &Path) -> PathBuf {
    let mut name = target_dir
        .file_name()
        .map(std::ffi::OsStr::to_os_string)
        .unwrap_or_default();
    name.push(".run_ld");
    target_dir.with_file_name(name)
}
