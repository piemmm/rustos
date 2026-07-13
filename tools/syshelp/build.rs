//! Discover the command-app bundles' `Help/` trees and `Resources/` files
//! from the source tree and emit them as embedded `[HelpFile]` /
//! `[ResourceFile]` tables.
//!
//! The image builder plants each command app's internationalised help onto
//! the read-only `/System/Apps/<name>.app/Help/<locale>/<file>` store, and
//! each app's bundle resources onto
//! `/System/Apps/<name>.app/Resources/<file>`. The *source of truth* for
//! both is the bundle's own on-disk directory — never a hand-maintained
//! per-bundle list in the image builder, which would force an edit to a
//! central file every time a bundle is added (the duplication the charter
//! forbids). This script walks the app roots, finds every
//! `Help/<locale>/<doc>.md` and every `Resources/<file>`, and generates an
//! `include_bytes!` row per file, so adding a bundle's payload is dropping
//! files on disk — it is rediscovered on the next build.
//!
//! A build script is host-only build tooling: a genuine build-environment
//! failure (a missing `OUT_DIR`, an unreadable source tree) fails the build
//! loudly rather than emitting a silently-incomplete image.

use std::fmt::Write as _;
use std::fs;
use std::io::Write as _;
use std::path::{Path, PathBuf};

/// Source roots under which command-app bundles live. A bundle's help is
/// discovered from `<root>/<crate>/Help/`; the planted bundle is
/// `<name>.app`, where `<name>` is the crate's `AppInfo.toml` manifest
/// name — the same source of truth the app-bundle composer plants under —
/// so a crate whose directory is not its command word (the desktop
/// session at `userland/gui/session`, bundle `desktop.app`) still lands
/// under its real bundle name. Extending this list is a rare structural
/// change, not a per-bundle edit.
const APP_ROOTS: &[&str] = &["userland/apps", "userland/gui", "userland/shell"];

fn main() {
    let manifest = PathBuf::from(env("CARGO_MANIFEST_DIR"));
    // <workspace>/tools/syshelp -> <workspace>
    let workspace = manifest
        .parent()
        .and_then(Path::parent)
        .expect("crate lives at <workspace>/tools/syshelp")
        .to_path_buf();

    let mut rows = String::from("[\n");
    for root_rel in APP_ROOTS {
        let root = workspace.join(root_rel);
        println!("cargo:rerun-if-changed={}", root.display());
        if !root.is_dir() {
            continue;
        }
        for app in sorted_children(&root) {
            let crate_dir = root.join(&app);
            let help = crate_dir.join("Help");
            if !help.is_dir() {
                continue;
            }
            println!("cargo:rerun-if-changed={}", help.display());
            let bundle = bundle_dir_name(&crate_dir);
            for locale in sorted_children(&help) {
                let locale_dir = help.join(&locale);
                if !locale_dir.is_dir() {
                    continue;
                }
                println!("cargo:rerun-if-changed={}", locale_dir.display());
                for file in sorted_children(&locale_dir) {
                    let path = locale_dir.join(&file);
                    let is_md = path
                        .extension()
                        .is_some_and(|ext| ext.eq_ignore_ascii_case("md"));
                    if !path.is_file() || !is_md {
                        continue;
                    }
                    println!("cargo:rerun-if-changed={}", path.display());
                    let abs = path.to_str().expect("help path is valid UTF-8");
                    writeln!(
                        rows,
                        "    HelpFile {{ bundle: {bundle:?}, locale: {locale:?}, file: {file:?}, bytes: include_bytes!({abs:?}) }},"
                    )
                    .expect("write to String");
                }
            }
        }
    }
    rows.push(']');

    let dest = PathBuf::from(env("OUT_DIR")).join("help_files.rs");
    fs::File::create(&dest)
        .and_then(|mut f| f.write_all(rows.as_bytes()))
        .expect("write generated help table");

    let mut resource_rows = String::from("[\n");
    for root_rel in APP_ROOTS {
        let root = workspace.join(root_rel);
        if !root.is_dir() {
            continue;
        }
        for app in sorted_children(&root) {
            let crate_dir = root.join(&app);
            let resources = crate_dir.join("Resources");
            if !resources.is_dir() {
                continue;
            }
            println!("cargo:rerun-if-changed={}", resources.display());
            let bundle = bundle_dir_name(&crate_dir);
            for file in sorted_children(&resources) {
                let path = resources.join(&file);
                if !path.is_file() {
                    continue;
                }
                println!("cargo:rerun-if-changed={}", path.display());
                let abs = path.to_str().expect("resource path is valid UTF-8");
                writeln!(
                    resource_rows,
                    "    ResourceFile {{ bundle: {bundle:?}, file: {file:?}, bytes: include_bytes!({abs:?}) }},"
                )
                .expect("write to String");
            }
        }
    }
    resource_rows.push(']');

    let dest = PathBuf::from(env("OUT_DIR")).join("resource_files.rs");
    fs::File::create(&dest)
        .and_then(|mut f| f.write_all(resource_rows.as_bytes()))
        .expect("write generated resource table");

    println!("cargo:rerun-if-changed=build.rs");
}

/// The planted bundle directory (`<name>.app`) of the app crate at
/// `crate_dir`, from its `AppInfo.toml` manifest source's `name` key — the
/// same source of truth the app-bundle composer plants under, never the
/// crate directory's own name. A crate that ships a `Help/` or
/// `Resources/` payload without a readable manifest name is a broken
/// source tree: fail the build loudly rather than plant the payload under
/// a guessed bundle.
fn bundle_dir_name(crate_dir: &Path) -> String {
    let manifest = crate_dir.join("AppInfo.toml");
    let text = fs::read_to_string(&manifest)
        .unwrap_or_else(|e| panic!("read {}: {e}", manifest.display()));
    let name = text
        .lines()
        .map(str::trim)
        .filter(|line| !line.starts_with('#'))
        .find_map(|line| {
            let value = line.strip_prefix("name")?.trim_start().strip_prefix('=')?;
            value.trim().strip_prefix('"')?.strip_suffix('"')
        })
        .unwrap_or_else(|| panic!("{}: no `name = \"...\"` key", manifest.display()));
    format!("{name}.app")
}

/// The names of a directory's entries, sorted, so the generated table (and
/// therefore the planted store) is deterministic across builds and hosts.
fn sorted_children(dir: &Path) -> Vec<String> {
    let mut names: Vec<String> = fs::read_dir(dir)
        .unwrap_or_else(|e| panic!("read {}: {e}", dir.display()))
        .filter_map(Result::ok)
        .filter_map(|entry| entry.file_name().into_string().ok())
        .collect();
    names.sort();
    names
}

/// A required build-environment variable Cargo always sets; its absence is a
/// broken build host, not a recoverable condition.
fn env(key: &str) -> String {
    std::env::var(key).unwrap_or_else(|_| panic!("{key} is set by cargo"))
}
