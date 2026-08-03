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
//!
//! Alongside the per-bundle help and resource families, this script also
//! discovers the desktop's graphics assets — the raster icon masters under
//! `lib/icon/assets/` — and emits a `[GraphicsFile]` table for the image
//! builder to plant under `/System/Graphics`. Those are validated against the
//! desktop's own icon contract (`tairix_icon`) as they are discovered: a name
//! the desktop could never resolve, an over-large file, or two files claiming
//! one asset id fails the build closed rather than shipping artwork that would
//! silently render as a fallback glyph.

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

/// Source directory of the desktop's raster icon masters, relative to the
/// workspace root. Unlike the per-bundle help/resource families this is a
/// single non-per-bundle tree, so it is walked directly rather than under a
/// per-crate `<root>/<crate>/…` layout.
const GRAPHICS_ASSETS_ROOT: &str = "lib/icon/assets";

/// The `/System/Graphics` subdirectory the icon masters are planted in — the
/// `GraphicsFile::dir` every emitted icon row carries. Keeping it as data
/// means a future cursor/chrome family plants through the same table and loop
/// rather than a second one.
const GRAPHICS_ICONS_DIR: &str = "Icons";

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

    emit_graphics_table(&workspace);

    println!("cargo:rerun-if-changed=build.rs");
}

/// Discover the desktop's raster icon masters under [`GRAPHICS_ASSETS_ROOT`]
/// and write `graphics_files.rs` — one `GraphicsFile` row per asset, planted
/// at `Graphics/<dir>/<file>` on the image.
///
/// Each discovered file is validated against the desktop's own icon contract
/// before it is emitted, so the build fails closed on artwork the desktop
/// could never resolve rather than shipping a file that silently renders as a
/// fallback glyph:
///
/// * the name must be a legal `<asset-id>.png`
///   ([`tairix_icon::artwork_kind_for_file`]),
/// * the file must be at most [`tairix_icon::MAX_ARTWORK_BYTES`] (the same
///   untrusted-input bound the runtime resolver refuses over-long input
///   against), and
/// * no two files may claim the same asset id.
///
/// Rows are sorted (by file name) exactly as the help and resource walks are,
/// so the emitted table — and the planted image — is reproducible.
fn emit_graphics_table(workspace: &Path) {
    let assets = workspace.join(GRAPHICS_ASSETS_ROOT);
    println!("cargo:rerun-if-changed={}", assets.display());

    let max_bytes = u64::try_from(tairix_icon::MAX_ARTWORK_BYTES).expect("bound fits u64");
    let mut seen_ids: Vec<&'static str> = Vec::new();
    let mut rows = String::from("[\n");
    if assets.is_dir() {
        for file in sorted_children(&assets) {
            let path = assets.join(&file);
            if !path.is_file() {
                continue;
            }
            // A name the loader would never map to a kind (an unknown id, a
            // wrong extension, a path separator) can never resolve to artwork,
            // so shipping it is a build error, not a silent fallback.
            let kind = tairix_icon::artwork_kind_for_file(&file).unwrap_or_else(|| {
                panic!(
                    "{}: not a legal desktop icon artwork name (expected `<asset-id>.png`)",
                    path.display()
                )
            });
            // The byte bound is a fixed validation limit on untrusted input:
            // an over-large asset is refused before it is decoded, so one that
            // exceeds it must never reach the image.
            let len = fs::metadata(&path)
                .unwrap_or_else(|e| panic!("stat {}: {e}", path.display()))
                .len();
            assert!(
                len <= max_bytes,
                "{}: {len} bytes exceeds the {}-byte icon artwork bound",
                path.display(),
                tairix_icon::MAX_ARTWORK_BYTES
            );
            let id = kind.asset_id();
            assert!(
                !seen_ids.contains(&id),
                "{}: two files claim the asset id `{id}`",
                path.display()
            );
            seen_ids.push(id);

            println!("cargo:rerun-if-changed={}", path.display());
            let abs = path.to_str().expect("graphics path is valid UTF-8");
            writeln!(
                rows,
                "    GraphicsFile {{ dir: {GRAPHICS_ICONS_DIR:?}, file: {file:?}, bytes: include_bytes!({abs:?}) }},"
            )
            .expect("write to String");
        }
    }
    rows.push(']');

    let dest = PathBuf::from(env("OUT_DIR")).join("graphics_files.rs");
    fs::File::create(&dest)
        .and_then(|mut f| f.write_all(rows.as_bytes()))
        .expect("write generated graphics table");
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
