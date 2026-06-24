//! Build a bootable BIOS ISO containing a multiboot2 kernel.
//!
//! `grub-mkrescue` is the canonical tool. Stage 2's QEMU integration
//! tests need a deterministic, reproducible disk image; this module
//! wraps the external invocation so the build rule lives in exactly
//! one place and so `cargo xtask test --qemu` can
//! report a clean diagnostic when the host is missing `grub-mkrescue`
//! / `xorriso` rather than emitting an opaque process error.
//!
//! # Reproducibility
//!
//! `grub-mkrescue` is *not* byte-for-byte reproducible (it embeds
//! timestamps in the ISO9660 metadata). Stage 9 of `PLAN.md` covers
//! reproducible builds; the Stage-2 tests only require *functional*
//! reproducibility (the kernel boots and reports a pass/fail). This
//! module therefore does not attempt to pin the ISO contents.

use std::fs;
use std::io;
use std::path::{Path, PathBuf};
use std::process::Command;

/// Build a bootable ISO at `out_iso` containing `kernel_elf` as the
/// multiboot2 payload, wrapped in a GRUB BIOS boot image.
///
/// `staging_dir` is a scratch directory the builder is free to populate
/// (`{staging_dir}/boot/kernel.elf` and `{staging_dir}/boot/grub/grub.cfg`
/// are created). The directory is **not** cleaned up — the caller (the
/// xtask driver) owns the temp directory lifecycle so a developer can
/// inspect a failed build.
///
/// # Errors
///
/// * `NotFound` — `grub-mkrescue` or `xorriso` are not on `$PATH`.
/// * `Other` — `grub-mkrescue` exited non-zero; stderr is included in
///   the error message.
pub fn build_grub_iso(
    kernel_elf: &Path,
    staging_dir: &Path,
    out_iso: &Path,
) -> io::Result<PathBuf> {
    if which("grub-mkrescue").is_none() {
        return Err(io::Error::new(
            io::ErrorKind::NotFound,
            "grub-mkrescue is not on PATH; install `grub-common` + \
             `grub-pc-bin` (or the equivalent on your distro)",
        ));
    }
    if which("xorriso").is_none() {
        return Err(io::Error::new(
            io::ErrorKind::NotFound,
            "xorriso is not on PATH; install the `xorriso` package",
        ));
    }
    if !kernel_elf.is_file() {
        return Err(io::Error::new(
            io::ErrorKind::NotFound,
            format!("kernel ELF not found: {}", kernel_elf.display()),
        ));
    }

    let boot_dir = staging_dir.join("boot");
    let grub_dir = boot_dir.join("grub");
    fs::create_dir_all(&grub_dir)?;

    let kernel_dst = boot_dir.join("kernel.elf");
    fs::copy(kernel_elf, &kernel_dst)?;

    fs::write(
        grub_dir.join("grub.cfg"),
        "set timeout=0\n\
         set default=0\n\
         \n\
         menuentry \"rustos\" {\n\
         \x20   multiboot2 /boot/kernel.elf\n\
         \x20   boot\n\
         }\n",
    )?;

    if let Some(parent) = out_iso.parent() {
        fs::create_dir_all(parent)?;
    }

    let output = Command::new("grub-mkrescue")
        // `--locales=` skips embedding language packs (smaller ISO, faster
        // build, fewer host dependencies).
        .arg("--locales=")
        .arg("--themes=")
        .arg("--fonts=")
        .arg("-o")
        .arg(out_iso)
        .arg(staging_dir)
        .output()?;

    if !output.status.success() {
        return Err(io::Error::other(format!(
            "grub-mkrescue failed ({}): {}",
            output.status,
            String::from_utf8_lossy(&output.stderr).trim()
        )));
    }

    Ok(out_iso.to_path_buf())
}

/// Located OVMF firmware: the read-only CODE image and a writable copy of
/// the VARS image (the runner copies the distro's stock VARS image into
/// the kernel build directory because QEMU writes to it during boot).
#[derive(Clone, Debug)]
pub struct OvmfPaths {
    /// Read-only OVMF code image.
    pub code: PathBuf,
    /// Writable copy of the OVMF variables image. Created by
    /// [`find_ovmf`] in the system tempdir under a path unique to this
    /// call (process id + a monotonic counter), so concurrent
    /// [`crate::Runner::run`] invocations in one process never share — or
    /// overwrite mid-boot — the same NVRAM store. The caller owns the
    /// file's lifetime and removes it once the guest has exited.
    pub vars_copy: PathBuf,
}

/// Build a path for a writable OVMF VARS copy that is unique to this
/// call.
///
/// QEMU's `find_ovmf` is invoked once per [`crate::Runner::run`], and the
/// guests run concurrently inside one host process (the `cargo xtask`
/// QEMU driver). Keying the copy only by process id would hand every
/// concurrent x86_64 guest the *same* writable pflash file: a later run's
/// `fs::copy` truncates and rewrites the store while an earlier run's QEMU
/// still has it live, and the victim QEMU aborts with `pflash … has
/// invalid size 0` (a status-1 exit whose diagnostic lands on stderr, not
/// the serial console). The monotonic counter — mirroring
/// [`crate::MonitorSocket`] — gives each run its own file. The id keeps
/// runs in different host processes distinct.
fn unique_vars_copy_path() -> PathBuf {
    use std::sync::atomic::{AtomicU64, Ordering};
    static COUNTER: AtomicU64 = AtomicU64::new(0);
    let n = COUNTER.fetch_add(1, Ordering::Relaxed);
    std::env::temp_dir().join(format!(
        "rustos-qemu-ovmf-vars-{}-{}.fd",
        std::process::id(),
        n
    ))
}

/// Locate OVMF on common Linux distros and prepare a writable VARS copy.
///
/// Searches a fixed list of standard paths (Debian/Ubuntu, Fedora, Arch).
/// If none of the candidates are present, returns `NotFound` with a
/// clear diagnostic telling the developer which package to install.
///
/// # Errors
///
/// * `NotFound` — no OVMF code image was found on the system.
/// * Other `io::Error`s — failed to copy the VARS image.
pub fn find_ovmf() -> io::Result<OvmfPaths> {
    // (code, vars) candidate pairs. First match wins. The 4 MiB
    // variants are preferred — modern QEMU recommends them.
    const CANDIDATES: &[(&str, &str)] = &[
        // Debian / Ubuntu (`ovmf` package).
        (
            "/usr/share/OVMF/OVMF_CODE_4M.fd",
            "/usr/share/OVMF/OVMF_VARS_4M.fd",
        ),
        (
            "/usr/share/OVMF/OVMF_CODE.fd",
            "/usr/share/OVMF/OVMF_VARS.fd",
        ),
        // Fedora / RHEL (`edk2-ovmf` package).
        (
            "/usr/share/edk2/ovmf/OVMF_CODE.fd",
            "/usr/share/edk2/ovmf/OVMF_VARS.fd",
        ),
        // Arch / Manjaro (`edk2-ovmf` package).
        (
            "/usr/share/edk2-ovmf/x64/OVMF_CODE.fd",
            "/usr/share/edk2-ovmf/x64/OVMF_VARS.fd",
        ),
        // macOS Homebrew, Apple-silicon prefix (`qemu` formula ships
        // edk2 firmware; x86_64 boots against the shared i386 VARS
        // template).
        (
            "/opt/homebrew/share/qemu/edk2-x86_64-code.fd",
            "/opt/homebrew/share/qemu/edk2-i386-vars.fd",
        ),
        // macOS Homebrew, Intel prefix.
        (
            "/usr/local/share/qemu/edk2-x86_64-code.fd",
            "/usr/local/share/qemu/edk2-i386-vars.fd",
        ),
    ];

    for (code, vars) in CANDIDATES {
        let code_path = PathBuf::from(code);
        let vars_path = PathBuf::from(vars);
        if code_path.is_file() && vars_path.is_file() {
            let dst = unique_vars_copy_path();
            fs::copy(&vars_path, &dst)?;
            return Ok(OvmfPaths {
                code: code_path,
                vars_copy: dst,
            });
        }
    }

    Err(io::Error::new(
        io::ErrorKind::NotFound,
        "OVMF firmware not found in any standard path. Install the \
         `ovmf` package on Debian/Ubuntu, `edk2-ovmf` on Fedora/Arch, \
         or `qemu` (which bundles the edk2 firmware) via Homebrew on macOS.",
    ))
}

fn which(binary: &str) -> Option<PathBuf> {
    let path = std::env::var_os("PATH")?;
    for dir in std::env::split_paths(&path) {
        let candidate = dir.join(binary);
        if candidate.is_file() {
            return Some(candidate);
        }
    }
    None
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn missing_kernel_elf_is_notfound() {
        let staging = std::env::temp_dir().join("rustos-qemu-iso-test");
        let out = staging.join("out.iso");
        let err = build_grub_iso(Path::new("/definitely/not/here"), &staging, &out)
            .expect_err("expected NotFound");
        assert_eq!(err.kind(), io::ErrorKind::NotFound);
    }

    #[test]
    fn each_vars_copy_path_is_unique_within_a_process() {
        // Regression: keying the writable OVMF VARS copy only by process
        // id handed every concurrent x86_64 guest in one host process
        // (the `cargo xtask` QEMU driver) the same pflash file, so one
        // run's `fs::copy` truncated another's live NVRAM store mid-boot
        // and the victim QEMU aborted with `pflash … invalid size 0`
        // (a status-1 exit). The monotonic counter must make successive
        // paths distinct so each run owns its own copy.
        let mut seen = std::collections::HashSet::new();
        for _ in 0..1000 {
            assert!(
                seen.insert(unique_vars_copy_path()),
                "unique_vars_copy_path handed out a duplicate path"
            );
        }
    }

    #[test]
    fn vars_copy_paths_share_the_runner_prefix() {
        // The cleanup guard and any stray-file sweep key off this prefix;
        // pin it so a rename cannot silently orphan the temp files.
        let path = unique_vars_copy_path();
        let name = path
            .file_name()
            .and_then(|n| n.to_str())
            .expect("temp path has a UTF-8 file name");
        assert!(
            name.starts_with("rustos-qemu-ovmf-vars-"),
            "unexpected VARS copy file name: {name}"
        );
        assert_eq!(
            path.extension().and_then(|e| e.to_str()),
            Some("fd"),
            "unexpected VARS copy suffix: {name}"
        );
    }
}
