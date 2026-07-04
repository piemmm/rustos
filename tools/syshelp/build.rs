//! Discover the command-app bundles' `Help/` trees from the source tree and
//! emit them as an embedded `[HelpFile]` table.
//!
//! The image builder plants each command app's internationalised help onto
//! the read-only `/System/Apps/<name>.app/Help/<locale>/<file>` store. The
//! *source of truth* for that help is the bundle's own on-disk `Help/`
//! directory — never a hand-maintained per-bundle list in the image builder,
//! which would force an edit to a central file every time a bundle is added
//! (the duplication the charter forbids). This script walks the app roots,
//! finds every `Help/<locale>/<doc>.md`, and generates an `include_bytes!`
//! row per document, so adding a bundle's help is dropping files on disk —
//! the payload is rediscovered on the next build.
//!
//! A build script is host-only build tooling: a genuine build-environment
//! failure (a missing `OUT_DIR`, an unreadable source tree) fails the build
//! loudly rather than emitting a silently-incomplete image.

use std::fmt::Write as _;
use std::fs;
use std::io::Write as _;
use std::path::{Path, PathBuf};

/// Source roots under which command-app bundles live. A bundle's help is
/// discovered from `<root>/<name>/Help/`; the planted bundle is
/// `<name>.app`. Extending this list is a rare structural change, not a
/// per-bundle edit.
const APP_ROOTS: &[&str] = &["userland/apps"];

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
            let help = root.join(&app).join("Help");
            if !help.is_dir() {
                continue;
            }
            println!("cargo:rerun-if-changed={}", help.display());
            let bundle = format!("{app}.app");
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

    println!("cargo:rerun-if-changed=build.rs");
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
