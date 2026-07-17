//! Pre-build cleanup of superseded build-script output directories.
//!
//! The `tairix-kernel` build script (`kernel/tairix-kernel/build.rs`)
//! compiles the embedded userland `Run` programs — each a full `-Z
//! build-std` target tree of roughly a gigabyte — into its `OUT_DIR`. Cargo
//! keys a build script's `OUT_DIR` by the *fingerprint* of the build-script
//! build unit, so every time that fingerprint changes (a `build.rs` or
//! `build_support.rs` edit, a dependency bump, a toolchain change) cargo
//! allocates a **fresh** `target/<triple>/<profile>/build/<pkg>-<hash>/`
//! directory and silently orphans the previous one. Cargo never reclaims the
//! orphans, so across a normal development history they accumulate into tens
//! of gigabytes of nested build-std trees that will never be read again.
//!
//! Only the newest `build/<pkg>-<hash>` directory for a given package is
//! live: a successful build refreshes its `invoked.timestamp`, so the live
//! directory always carries the most recent modification time within its
//! package group. Every strictly-older sibling is a superseded fingerprint
//! cargo will never reference; removing it at worst forces the (already
//! always-rerunning) build script to regenerate its output on the next
//! build. The matching `.fingerprint/<pkg>-<hash>` entry is removed with it.
//!
//! This is deliberately conservative (fail safe): a
//! directory whose name is not the cargo `<pkg>-<16-hex>` shape is left
//! untouched, a package with a single build directory is left untouched, and
//! tied-newest directories are all kept. It only ever removes regenerable
//! build-script output, never source, never the live artifacts of the
//! current build.

use std::collections::BTreeMap;
use std::ffi::OsString;
use std::path::{Path, PathBuf};
use std::time::SystemTime;

use crate::Context;

use super::{dir_size, format_bytes};

/// `cargo xtask prune`: remove superseded build-script output directories and
/// report the reclaimed space. Takes no arguments.
pub fn run(ctx: &Context, args: &[OsString]) -> Result<(), String> {
    if let Some(unexpected) = args.first() {
        return Err(format!(
            "prune takes no arguments, got {:?}",
            unexpected.to_string_lossy()
        ));
    }
    let reclaimed = prune(ctx);
    let target_dir = ctx.target_dir();
    eprintln!(
        "xtask: [prune] reclaimed {} of superseded build-script output in {}",
        format_bytes(reclaimed.bytes),
        super::relative(&ctx.workspace_root, &target_dir),
    );
    Ok(())
}

/// What a prune pass reclaimed.
#[derive(Debug, Default, Clone, Copy, PartialEq, Eq)]
pub struct Reclaimed {
    /// Total bytes freed across every removed directory.
    pub bytes: u64,
    /// Number of superseded build directories removed.
    pub dirs: usize,
}

/// Remove every superseded `build/<pkg>-<hash>` directory beneath cargo's
/// target directory and return the space reclaimed.
///
/// Best-effort: an unreadable entry or a removal that races with another
/// process is skipped rather than turned into a hard error, because pruning
/// regenerable cache must never block the build it precedes. The current build's live directories are always retained.
pub fn prune(ctx: &Context) -> Reclaimed {
    let mut reclaimed = Reclaimed::default();
    for build_dir in build_dirs(&ctx.target_dir()) {
        prune_build_dir(&build_dir, &mut reclaimed);
    }
    reclaimed
}

/// Every `target/.../<profile>/build` directory that exists.
///
/// Cargo writes build-script output under `<profile>/build` for the host
/// (`target/debug`, `target/release`) and under `<triple>/<profile>/build`
/// for each explicit `--target`. We enumerate exactly those locations rather
/// than walking the whole tree so the scan stays shallow and cannot wander
/// into the (much larger) `deps`/`incremental` directories.
fn build_dirs(target_dir: &Path) -> Vec<PathBuf> {
    const PROFILES: [&str; 2] = ["debug", "release"];
    let mut dirs = Vec::new();
    for profile in PROFILES {
        let candidate = target_dir.join(profile).join("build");
        if candidate.is_dir() {
            dirs.push(candidate);
        }
    }
    let Ok(entries) = std::fs::read_dir(target_dir) else {
        return dirs;
    };
    for entry in entries.flatten() {
        if !entry.file_type().is_ok_and(|t| t.is_dir()) {
            continue;
        }
        let triple = entry.path();
        for profile in PROFILES {
            let candidate = triple.join(profile).join("build");
            if candidate.is_dir() {
                dirs.push(candidate);
            }
        }
    }
    dirs
}

/// Prune the superseded entries inside one `<profile>/build` directory.
fn prune_build_dir(build_dir: &Path, reclaimed: &mut Reclaimed) {
    let Ok(entries) = std::fs::read_dir(build_dir) else {
        return;
    };

    let mut candidates: Vec<BuildEntry> = Vec::new();
    for entry in entries.flatten() {
        if !entry.file_type().is_ok_and(|t| t.is_dir()) {
            continue;
        }
        let name = entry.file_name();
        let Some(name) = name.to_str() else {
            continue;
        };
        if package_of(name).is_none() {
            continue;
        }
        let modified = entry
            .metadata()
            .and_then(|m| m.modified())
            .unwrap_or(SystemTime::UNIX_EPOCH);
        candidates.push(BuildEntry {
            name: name.to_string(),
            modified,
        });
    }

    let fingerprint_dir = build_dir
        .parent()
        .map(|profile| profile.join(".fingerprint"));
    for name in superseded(&candidates) {
        let dir = build_dir.join(&name);
        let bytes = dir_size(&dir);
        if std::fs::remove_dir_all(&dir).is_ok() {
            reclaimed.bytes = reclaimed.bytes.saturating_add(bytes);
            reclaimed.dirs += 1;
            // The build-script fingerprint shares the directory name; drop it
            // too so cargo does not keep a dangling fingerprint for output we
            // just removed. Absent or unremovable is fine — it is regenerated.
            if let Some(fingerprint_dir) = &fingerprint_dir {
                let _ = std::fs::remove_dir_all(fingerprint_dir.join(&name));
            }
        }
    }
}

/// One `<pkg>-<hash>` build directory and its modification time.
struct BuildEntry {
    name: String,
    modified: SystemTime,
}

/// The names of the superseded directories: every entry whose package group
/// has a strictly newer sibling.
///
/// Pure so it can be unit-tested without touching the filesystem. For each
/// package the newest modification time is kept (every directory sharing that
/// newest time is retained, so a tie never deletes the live one); strictly
/// older siblings are returned for removal.
fn superseded(candidates: &[BuildEntry]) -> Vec<String> {
    let mut newest: BTreeMap<&str, SystemTime> = BTreeMap::new();
    for entry in candidates {
        let package = package_of(&entry.name).unwrap_or(&entry.name);
        newest
            .entry(package)
            .and_modify(|t| {
                if entry.modified > *t {
                    *t = entry.modified;
                }
            })
            .or_insert(entry.modified);
    }

    candidates
        .iter()
        .filter(|entry| {
            let package = package_of(&entry.name).unwrap_or(&entry.name);
            newest
                .get(package)
                .is_some_and(|newest| entry.modified < *newest)
        })
        .map(|entry| entry.name.clone())
        .collect()
}

/// The package name of a cargo build directory `<pkg>-<hash>`, or `None` if
/// the name does not have that shape.
///
/// Cargo suffixes every build directory with a 16-character lowercase-hex
/// metadata hash. Requiring exactly that shape means a directory that is not
/// a cargo build artifact (or whose layout we do not recognise) is left
/// untouched rather than guessed at. The package portion
/// itself may contain hyphens (`tairix-kernel-ipc`), so we split on the last
/// hyphen only.
fn package_of(name: &str) -> Option<&str> {
    let (package, hash) = name.rsplit_once('-')?;
    if package.is_empty() {
        return None;
    }
    if hash.len() == 16 && hash.bytes().all(|b| b.is_ascii_hexdigit()) {
        Some(package)
    } else {
        None
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::Duration;

    fn entry(name: &str, secs: u64) -> BuildEntry {
        BuildEntry {
            name: name.to_string(),
            modified: SystemTime::UNIX_EPOCH + Duration::from_secs(secs),
        }
    }

    #[test]
    fn package_of_strips_the_sixteen_hex_hash() {
        assert_eq!(
            package_of("tairix-kernel-d1e365cab8d956e9"),
            Some("tairix-kernel")
        );
        assert_eq!(
            package_of("tairix-kernel-ipc-2416769c78dec007"),
            Some("tairix-kernel-ipc")
        );
    }

    #[test]
    fn package_of_rejects_non_artifact_names() {
        // No hash suffix.
        assert_eq!(package_of("CACHEDIR.TAG"), None);
        assert_eq!(package_of("tairix-kernel"), None);
        // Wrong hash length.
        assert_eq!(package_of("foo-deadbeef"), None);
        // Non-hex suffix of the right length.
        assert_eq!(package_of("foo-zzzzzzzzzzzzzzzz"), None);
        // Empty package portion.
        assert_eq!(package_of("-d1e365cab8d956e9"), None);
    }

    #[test]
    fn superseded_keeps_only_the_newest_per_package() {
        let candidates = [
            entry("tairix-kernel-1111111111111111", 100),
            entry("tairix-kernel-2222222222222222", 300),
            entry("tairix-kernel-3333333333333333", 200),
        ];
        let mut pruned = superseded(&candidates);
        pruned.sort();
        assert_eq!(
            pruned,
            vec![
                "tairix-kernel-1111111111111111".to_string(),
                "tairix-kernel-3333333333333333".to_string(),
            ]
        );
    }

    #[test]
    fn superseded_does_not_cross_package_groups() {
        let candidates = [
            entry("tairix-kernel-1111111111111111", 100),
            entry("tairix-kernel-2222222222222222", 300),
            entry("tairix-kernel-ipc-3333333333333333", 50),
            entry("tairix-kernel-ipc-4444444444444444", 60),
        ];
        let mut pruned = superseded(&candidates);
        pruned.sort();
        assert_eq!(
            pruned,
            vec![
                "tairix-kernel-1111111111111111".to_string(),
                "tairix-kernel-ipc-3333333333333333".to_string(),
            ]
        );
    }

    #[test]
    fn superseded_keeps_a_lone_directory() {
        let candidates = [entry("tairix-kernel-1111111111111111", 100)];
        assert!(superseded(&candidates).is_empty());
    }

    #[test]
    fn superseded_keeps_every_tied_newest_directory() {
        // Two directories share the newest mtime: neither may be removed, so
        // the live one is never deleted on a timestamp tie.
        let candidates = [
            entry("tairix-kernel-1111111111111111", 300),
            entry("tairix-kernel-2222222222222222", 300),
            entry("tairix-kernel-3333333333333333", 100),
        ];
        let pruned = superseded(&candidates);
        assert_eq!(pruned, vec!["tairix-kernel-3333333333333333".to_string()]);
    }

    #[test]
    fn superseded_ignores_unrecognised_names() {
        // A directory that is not a cargo build artifact is its own group and
        // is never pruned.
        let candidates = [
            entry("CACHEDIR.TAG", 100),
            entry("tairix-kernel-1111111111111111", 100),
            entry("tairix-kernel-2222222222222222", 300),
        ];
        let pruned = superseded(&candidates);
        assert_eq!(pruned, vec!["tairix-kernel-1111111111111111".to_string()]);
    }
}
