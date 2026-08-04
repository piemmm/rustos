//! Discover the program bundles' `Help/` trees and `Resources/` files
//! from the source tree and emit them as embedded `[HelpFile]` /
//! `[ResourceFile]` tables.
//!
//! The image builder plants each bundle's internationalised help onto the
//! read-only `/System/<store>/<name>.app/Help/<locale>/<file>` store, and
//! each app's bundle resources onto
//! `/System/<store>/<name>.app/Resources/<file>`, where `<store>` is the
//! store the bundle's own declared kind installs it to. The *source of truth* for
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
//! discovers the desktop's graphics assets — today the raster icon masters
//! under `lib/icon/assets/` and the wallpaper masters under
//! `lib/wallpaper/assets/` — and emits one `[GraphicsFile]` table for the
//! image builder to plant under `/System/Graphics`. Both are single,
//! non-per-bundle trees walked by the same `GRAPHICS_FAMILIES` table and
//! loop, each validated against its own family's contract
//! (`tairix_icon`/`tairix_wallpaper`) as it is discovered: a name a consumer
//! could never resolve, an over-large file, or (for icons) two files claiming
//! one asset id fails the build closed rather than shipping artwork that
//! would silently render as a fallback glyph or never be offered.

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

/// One single-tree desktop graphics asset family: a source directory under
/// the workspace root, the `/System/Graphics` subdirectory its files are
/// planted in, the largest byte size a file in it may be, and how to decide
/// whether one file name is legal for it (returning the identifier the
/// "no two files claim the same one" check keys on, or `None` when the name
/// is not legal for this family).
///
/// Unlike the per-bundle help/resource families above, a graphics family is
/// a single non-per-bundle tree, so [`emit_graphics_table`] walks each entry
/// in this table directly rather than under a per-crate `<root>/<crate>/…`
/// layout. Adding a family (a future cursor/chrome set, say) is adding a row
/// here, never a second copy of the walk — that duplication is exactly what
/// this table exists to prevent.
struct GraphicsFamily {
    /// Source directory, relative to the workspace root.
    source_root: &'static str,
    /// The name of the `GraphicsFile::family` variant (`tairix_syshelp::
    /// GraphicsFamilyKind`) this family's rows are tagged with.
    ///
    /// This build script cannot depend on the very crate it generates code
    /// for, so it cannot name that enum as a Rust type; instead this is
    /// spliced verbatim into each emitted row as
    /// `GraphicsFamilyKind::<family_variant>`, which then resolves against
    /// the one enum `src/lib.rs` defines once the generated file is
    /// `include!`d into it. The family's `/System/Graphics` subdirectory
    /// is not written down here at all — `GraphicsFamilyKind::target_dir`
    /// is its one definition.
    family_variant: &'static str,
    /// Largest byte size a file in this family may be.
    max_bytes: usize,
    /// Whether `name` is a legal shipped file name for this family, and if
    /// so the identifier the duplicate-id check keys on.
    identify: fn(&str) -> Option<String>,
}

/// The desktop's single-tree graphics asset families: today the raster icon
/// masters and the shipped wallpaper masters.
const GRAPHICS_FAMILIES: &[GraphicsFamily] = &[
    GraphicsFamily {
        source_root: "lib/icon/assets",
        family_variant: "Icon",
        max_bytes: tairix_icon::MAX_ARTWORK_BYTES,
        identify: |name| {
            tairix_icon::artwork_kind_for_file(name).map(|kind| kind.asset_id().to_string())
        },
    },
    GraphicsFamily {
        source_root: "lib/wallpaper/assets",
        family_variant: "Wallpaper",
        max_bytes: tairix_wallpaper::MAX_WALLPAPER_BYTES,
        identify: |name| tairix_wallpaper::is_wallpaper_file_name(name).then(|| name.to_string()),
    },
];

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
            let (bundle, store) = bundle_identity(&crate_dir);
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
                        "    HelpFile {{ store: {store:?}, bundle: {bundle:?}, locale: {locale:?}, file: {file:?}, bytes: include_bytes!({abs:?}) }},"
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
            let (bundle, store) = bundle_identity(&crate_dir);
            for file in sorted_children(&resources) {
                let path = resources.join(&file);
                if !path.is_file() {
                    continue;
                }
                println!("cargo:rerun-if-changed={}", path.display());
                let abs = path.to_str().expect("resource path is valid UTF-8");
                writeln!(
                    resource_rows,
                    "    ResourceFile {{ store: {store:?}, bundle: {bundle:?}, file: {file:?}, bytes: include_bytes!({abs:?}) }},"
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

/// Discover every [`GraphicsFamily`]'s single-tree assets and write
/// `graphics_files.rs` — one `GraphicsFile` row per asset, planted at
/// `Graphics/<family's GraphicsFamilyKind::target_dir()>/<file>` on the
/// image.
///
/// Each discovered file is validated against its own family's `identify`
/// contract before it is emitted, so the build fails closed on an asset a
/// consumer could never resolve rather than shipping one that would silently
/// render as a fallback glyph (icons) or never be offered (wallpapers):
///
/// * the name must be legal for the family (`family.identify(name).is_some()`),
/// * the file must be at most `family.max_bytes` (the same untrusted-input
///   bound each family's own runtime consumer refuses over-long input
///   against), and
/// * no two files in the same family may claim the same identifier.
///
/// Families are walked in [`GRAPHICS_FAMILIES`] order and each family's
/// files are sorted by name, exactly as the help and resource walks are, so
/// the emitted table — and the planted image — is reproducible.
fn emit_graphics_table(workspace: &Path) {
    let mut rows = String::from("[\n");
    for family in GRAPHICS_FAMILIES {
        let assets = workspace.join(family.source_root);
        println!("cargo:rerun-if-changed={}", assets.display());
        if !assets.is_dir() {
            continue;
        }

        let max_bytes = u64::try_from(family.max_bytes).expect("bound fits u64");
        let mut seen_ids: Vec<String> = Vec::new();
        for file in sorted_children(&assets) {
            let path = assets.join(&file);
            if !path.is_file() {
                continue;
            }
            // A name the family's own runtime consumer would never resolve
            // (an unknown id, a wrong extension, a path separator) can never
            // be offered, so shipping it is a build error, not a silent
            // fallback.
            let id = (family.identify)(&file).unwrap_or_else(|| {
                panic!(
                    "{}: not a legal shipped name for the {} graphics family",
                    path.display(),
                    family.family_variant
                )
            });
            // The byte bound is a fixed validation limit on untrusted input:
            // an over-large asset is refused before it is decoded, so one
            // that exceeds it must never reach the image.
            let len = fs::metadata(&path)
                .unwrap_or_else(|e| panic!("stat {}: {e}", path.display()))
                .len();
            assert!(
                len <= max_bytes,
                "{}: {len} bytes exceeds the {}-byte {} bound",
                path.display(),
                family.max_bytes,
                family.family_variant
            );
            assert!(
                !seen_ids.contains(&id),
                "{}: two files claim the identifier `{id}`",
                path.display()
            );
            seen_ids.push(id);

            println!("cargo:rerun-if-changed={}", path.display());
            let abs = path.to_str().expect("graphics path is valid UTF-8");
            let variant = family.family_variant;
            writeln!(
                rows,
                "    GraphicsFile {{ family: GraphicsFamilyKind::{variant}, file: {file:?}, bytes: include_bytes!({abs:?}) }},"
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
/// `crate_dir` and the `/System` store it installs to, from its
/// `AppInfo.toml` manifest source's `name` and `kind` keys — the same source
/// of truth the app-bundle composer plants under, never the crate
/// directory's own name. The store matters as much as the name: a payload
/// planted under the wrong store would leave the installed bundle missing
/// content its signed digest covers, and the load gate would refuse the
/// bundle — so the store comes from the one shared kind → store mapping the
/// composer itself uses. A crate that ships a `Help/` or `Resources/`
/// payload without a readable manifest name and kind is a broken source
/// tree: fail the build loudly rather than plant the payload under a
/// guessed bundle.
fn bundle_identity(crate_dir: &Path) -> (String, &'static str) {
    let manifest = crate_dir.join("AppInfo.toml");
    let text = fs::read_to_string(&manifest)
        .unwrap_or_else(|e| panic!("read {}: {e}", manifest.display()));
    let value_of = |key: &str| {
        text.lines()
            .map(str::trim)
            .filter(|line| !line.starts_with('#'))
            .find_map(|line| {
                let value = line.strip_prefix(key)?.trim_start().strip_prefix('=')?;
                value.trim().strip_prefix('"')?.strip_suffix('"')
            })
            .unwrap_or_else(|| panic!("{}: no `{key} = \"...\"` key", manifest.display()))
    };
    let name = value_of("name");
    let kind = value_of("kind");
    let store = tairix_abi::ProgramKind::from_key(kind)
        .unwrap_or_else(|| panic!("{}: unknown kind `{kind}`", manifest.display()))
        .store_dir();
    (format!("{name}.app"), store)
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
