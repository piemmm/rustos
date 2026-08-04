//! Build-discovered system payload (command-app Help documents and
//! `Resources/` files, plus the desktop's graphics assets) for image
//! authoring.
//!
//! TAIRiX ships each command app's internationalised command help as a
//! structured-Markdown `Help/` tree on the read-only `/System` volume, at
//! `/System/<store>/<name>.app/Help/<locale>/<doc>.md` (`plans/APPS.md`), and
//! each app's bundle resources (e.g. `lspci`'s compiled ID-database table,
//! `plans/DEVICES.md`) at `/System/<store>/<name>.app/Resources/<file>`,
//! where `<store>` is the store the bundle's own manifest kind installs it
//! to — `Commands` for a command app, `Applications` for a graphical
//! application, `Services` for a service. It also
//! ships the desktop's graphics assets — today the raster icon masters and
//! the shipped default wallpaper masters — under `/System/Graphics`. The
//! image builder (`tools/mkimage`) and the QEMU image fixture must plant all
//! of these onto the volume they author.
//!
//! The source of truth for each family is its own on-disk directory. This
//! crate's build script walks the command-app source roots (`userland/apps`,
//! `userland/gui`, `userland/shell`; each bundle named by its crate's
//! `AppInfo.toml`, never the crate directory) for `Help/` and `Resources/`,
//! and walks each single-tree graphics asset family — `lib/icon/assets/` for
//! the desktop icon masters, `lib/wallpaper/assets/` for the shipped
//! wallpaper masters — through one shared table and loop, embedding each
//! discovered file as a row in [`HELP_FILES`] / [`RESOURCE_FILES`] /
//! [`GRAPHICS_FILES`]. The planters iterate that discovered data — **never** a
//! hand-maintained list that a new file would force an edit to (the
//! duplication the charter forbids). Adding a bundle's payload is dropping
//! files under `<root>/<name>/Help/<locale>/` or `<root>/<name>/Resources/`,
//! adding an icon is dropping a `<asset-id>.png` under `lib/icon/assets/`,
//! and adding a wallpaper is dropping a `.jpg`/`.jpeg`/`.png` under
//! `lib/wallpaper/assets/`; the next build rediscovers them. Payload is
//! therefore authored in exactly one place and never hardcoded into a binary
//! or copied into the image builder.
//!
//! Each graphics family's files are additionally validated against that
//! family's own contract (`tairix_icon` for icons, `tairix_wallpaper` for
//! wallpapers) as they are discovered, so a name a consumer could never
//! resolve or an over-large file fails the build closed rather than shipping
//! an icon that would silently render as a fallback glyph or a wallpaper
//! that would never be offered.
//!
//! [`plant_system_payload`] is the single walk both planters drive their own
//! `plant_nested_file` from, so they can never lay down a different set of
//! files or spell a path differently.
//!
//! The payload is `&'static [u8]` bytes embedded at build time, so this crate
//! is `no_std` and depends on no app crate: both the host image builder and
//! the freestanding QEMU fixture (which also links into the aarch64 guest
//! tail) consume it unchanged.

#![no_std]
#![forbid(unsafe_code)]
#![deny(missing_docs)]

/// One shipped Help document, ready to plant at
/// `/System/<store>/<bundle>/Help/<locale>/<file>` on the read-only `/System`
/// volume.
///
/// The fields are the volume-relative path components under the bundle's own
/// store plus the document's embedded bytes; the image builder writes `bytes`
/// at `<store>/<bundle>/Help/<locale>/<file>`.
#[derive(Clone, Copy, Debug)]
pub struct HelpFile {
    /// The `/System` subdirectory of the store this bundle installs to —
    /// `Commands`, `Applications`, or `Services`. Carried per row
    /// because the payload must land inside the very bundle directory the
    /// composer signed: a file planted into the other store leaves the
    /// installed bundle missing content its manifest's digest covers, and
    /// the load gate then refuses the bundle outright.
    pub store: &'static str,
    /// The bundle directory name, including the `.app` suffix (e.g. `ls.app`).
    pub bundle: &'static str,
    /// The BCP-47 locale directory (`en-US/` is the mandatory canonical one).
    pub locale: &'static str,
    /// The document file name (e.g. `ls.md`).
    pub file: &'static str,
    /// The document's bytes, embedded from the bundle's source `Help/` tree.
    pub bytes: &'static [u8],
}

/// Every command app's Help documents, discovered from the source tree at
/// build time.
///
/// Rows are ordered deterministically (by bundle, then locale, then file
/// name), so the planted store and any reproducible image are stable across
/// builds and hosts.
pub const HELP_FILES: &[HelpFile] = &include!(concat!(env!("OUT_DIR"), "/help_files.rs"));

/// One shipped bundle resource, ready to plant at
/// `/System/<store>/<bundle>/Resources/<file>` on the read-only `/System`
/// volume.
///
/// A resource is bundle data the program reads at runtime through the
/// secured VFS (never `include_bytes!` into its binary): e.g. `lspci`'s
/// compiled `pci.ids.bin` lookup table, or the icon the bundle draws itself
/// with. The image builder writes `bytes` at
/// `<store>/<bundle>/Resources/<file>`, and the bundle's signed `AppInfo`
/// content hash covers it, so a tampered resource fails the load gate
/// closed.
#[derive(Clone, Copy, Debug)]
pub struct ResourceFile {
    /// The `/System` subdirectory of the store this bundle installs to —
    /// `Commands`, `Applications`, or `Services`.
    pub store: &'static str,
    /// The bundle directory name, including the `.app` suffix
    /// (e.g. `lspci.app`).
    pub bundle: &'static str,
    /// The resource file name (e.g. `pci.ids.bin`).
    pub file: &'static str,
    /// The file's bytes, embedded from the bundle's source `Resources/`
    /// directory.
    pub bytes: &'static [u8],
}

/// Every command app's `Resources/` files, discovered from the source tree
/// at build time.
///
/// Rows are ordered deterministically (by bundle, then file name), so the
/// planted store and any reproducible image are stable across builds and
/// hosts.
pub const RESOURCE_FILES: &[ResourceFile] =
    &include!(concat!(env!("OUT_DIR"), "/resource_files.rs"));

/// Which family of desktop graphics assets a [`GraphicsFile`] belongs to.
///
/// Closed by design, and deliberately not carried as a free-form string: a
/// consumer tells the two shipped families apart by matching on this enum,
/// never by comparing a directory name. Adding a third family (a future
/// cursor or chrome set, say) means adding a variant here, which then forces
/// every `match` over this type — in this crate and in every crate that
/// reads [`GRAPHICS_FILES`] — to say explicitly what the new family means to
/// it, rather than silently falling through a default arm.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum GraphicsFamilyKind {
    /// The raster icon masters: one `<asset-id>.png` per icon kind,
    /// resolved by the window manager and file manager by asset id.
    Icon,
    /// The shipped default wallpaper masters: one `.jpg`/`.jpeg`/`.png` per
    /// shipped master, listed by name through
    /// `tairix_wallpaper::catalog_entries`.
    Wallpaper,
}

impl GraphicsFamilyKind {
    /// The subdirectory of `/System/Graphics` this family's files are
    /// planted under. The one place either spelling (`Icons`, `Wallpapers`)
    /// is written down.
    #[must_use]
    pub const fn target_dir(self) -> &'static str {
        match self {
            Self::Icon => "Icons",
            Self::Wallpaper => "Wallpapers",
        }
    }
}

/// One shipped desktop graphics asset, ready to plant at
/// `/System/Graphics/<family.target_dir()>/<file>` on the read-only
/// `/System` volume.
///
/// Unlike a [`HelpFile`] or a [`ResourceFile`] a graphics asset is not
/// per-bundle: it is desktop-wide artwork, tagged with the
/// [`GraphicsFamilyKind`] it belongs to so a future family plants through
/// this same table and loop rather than a second one.
#[derive(Clone, Copy, Debug)]
pub struct GraphicsFile {
    /// Which family this asset belongs to.
    pub family: GraphicsFamilyKind,
    /// The asset's file name: for an icon, its stable asset id plus
    /// extension; for a wallpaper, the plain file name a consumer lists it
    /// by.
    pub file: &'static str,
    /// The asset's bytes.
    pub bytes: &'static [u8],
}

/// Every desktop graphics asset, discovered from each graphics family's own
/// source tree at build time (`lib/icon/assets/` for icons,
/// `lib/wallpaper/assets/` for wallpapers) and validated against that
/// family's own contract as it is discovered (a name its consumer could not
/// resolve, an over-large file, or a duplicate identifier fails the build).
///
/// Rows are ordered deterministically (by family, then file name), so the
/// planted store and any reproducible image are stable across builds and
/// hosts.
pub const GRAPHICS_FILES: &[GraphicsFile] =
    &include!(concat!(env!("OUT_DIR"), "/graphics_files.rs"));

/// Invoke `plant` once for every discovered system payload file — each
/// command app's [`HelpFile`] and [`ResourceFile`], and every desktop
/// [`GraphicsFile`] — passing the file's `/System`-volume-relative path
/// components and its bytes.
///
/// The image builder (`tools/mkimage`) and the QEMU whole-disk image fixture
/// (`tests/integration/encrypted_root_image`) both lay these files onto the
/// read-only `/System` volume, each with its own `plant_nested_file` and its
/// own error type. Driving both from this one walk is the single definition
/// of *which* files ship and *where* they land, so the two planters can never
/// list a different payload set (the duplication the charter forbids). It
/// takes a closure rather than returning owned paths so it needs no
/// allocation and stays `no_std`; `plant` returns the caller's own error on
/// failure, which stops the walk.
///
/// # Errors
///
/// Returns the first error `plant` reports, failing the whole planting closed
/// rather than shipping a partial payload.
pub fn plant_system_payload<E>(
    mut plant: impl FnMut(&[&[u8]], &[u8]) -> Result<(), E>,
) -> Result<(), E> {
    for doc in HELP_FILES {
        plant(
            &[
                doc.store.as_bytes(),
                doc.bundle.as_bytes(),
                b"Help",
                doc.locale.as_bytes(),
                doc.file.as_bytes(),
            ],
            doc.bytes,
        )?;
    }
    for res in RESOURCE_FILES {
        plant(
            &[
                res.store.as_bytes(),
                res.bundle.as_bytes(),
                b"Resources",
                res.file.as_bytes(),
            ],
            res.bytes,
        )?;
    }
    for asset in GRAPHICS_FILES {
        plant(
            &[
                b"Graphics",
                asset.family.target_dir().as_bytes(),
                asset.file.as_bytes(),
            ],
            asset.bytes,
        )?;
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    extern crate std;

    use std::collections::BTreeSet;
    use std::vec::Vec;

    use tairix_help::{lint_help_trees, LintDoc};

    use super::HELP_FILES;

    /// Discovery finds the command apps that ship help. This anchors the
    /// scan: if the roots or the tree layout regress, at least the known
    /// command apps must still be found.
    #[test]
    fn discovers_the_shipped_command_apps() {
        let bundles: BTreeSet<&str> = HELP_FILES.iter().map(|doc| doc.bundle).collect();
        assert!(bundles.contains("ls.app"), "ls.app help must be discovered");
        assert!(
            bundles.contains("man.app"),
            "man.app help must be discovered"
        );
    }

    /// Discovery finds the bundle resources the shipped command apps carry.
    /// This anchors the resource scan exactly as the help scan above: if
    /// the roots or the `Resources/` layout regress, at least the known
    /// resource-carrying apps must still be found, with non-empty bytes.
    #[test]
    fn discovers_the_shipped_bundle_resources() {
        let lspci_table = super::RESOURCE_FILES
            .iter()
            .find(|r| r.bundle == "lspci.app" && r.file == "pci.ids.bin")
            .expect("lspci.app's pci.ids.bin resource must be discovered");
        assert!(
            !lspci_table.bytes.is_empty(),
            "a discovered resource carries its file bytes"
        );
    }

    /// Every discovered payload row is planted inside the store its own
    /// bundle installs to.
    ///
    /// A bundle's signed `AppInfo` digest covers its `Help/` and
    /// `Resources/` files, so a row planted into the *other* store leaves
    /// the installed bundle missing content its digest claims and the load
    /// gate refuses the bundle outright — the whole bundle, not just the
    /// stray file. So this re-reads each bundle's declared kind from its own
    /// manifest, independently of the discovery that produced the rows, maps
    /// it through the one shared kind -> store definition, and holds every
    /// row to it.
    #[test]
    fn a_bundles_payload_is_planted_in_the_store_it_installs_to() {
        use std::collections::BTreeMap;
        use std::path::Path;
        use std::string::{String, ToString};
        use std::{format, fs};

        let workspace = Path::new(env!("CARGO_MANIFEST_DIR"))
            .parent()
            .and_then(Path::parent)
            .expect("the crate lives at <workspace>/tools/syshelp");
        let mut store_of: BTreeMap<String, String> = BTreeMap::new();
        for root in ["userland/apps", "userland/gui", "userland/shell"] {
            let Ok(entries) = fs::read_dir(workspace.join(root)) else {
                continue;
            };
            for entry in entries.filter_map(Result::ok) {
                let Ok(text) = fs::read_to_string(entry.path().join("AppInfo.toml")) else {
                    continue;
                };
                let value_of = |key: &str| {
                    text.lines()
                        .map(str::trim)
                        .filter(|line| !line.starts_with('#'))
                        .find_map(|line| {
                            let value = line.strip_prefix(key)?.trim_start().strip_prefix('=')?;
                            value.trim().strip_prefix('"')?.strip_suffix('"')
                        })
                        .map(ToString::to_string)
                };
                let (Some(name), Some(kind)) = (value_of("name"), value_of("kind")) else {
                    continue;
                };
                let store = tairix_abi::ProgramKind::from_key(&kind)
                    .unwrap_or_else(|| panic!("{name}: unknown kind `{kind}`"))
                    .store_dir();
                store_of.insert(format!("{name}.app"), store.to_string());
            }
        }
        assert!(
            store_of.values().any(|store| store == "Services"),
            "a service bundle must be among the discovered roots, or this proves nothing"
        );

        let rows = HELP_FILES.iter().map(|doc| (doc.store, doc.bundle)).chain(
            super::RESOURCE_FILES
                .iter()
                .map(|res| (res.store, res.bundle)),
        );
        for (store, bundle) in rows {
            let expected = store_of
                .get(bundle)
                .unwrap_or_else(|| panic!("{bundle} has no manifest among the app roots"));
            assert_eq!(
                store, expected,
                "{bundle}'s payload must be planted under {expected}, not {store}"
            );
        }
    }

    /// Every discovered tree passes the one shared help-tree lint
    /// (`plans/APPS.md` §8.1) — the same judgement `cargo xtask help-lint`
    /// gates on: spellings and fail-closed parse bounds, canonical `en-US/`
    /// presence, required-locale completeness, no translation-only
    /// documents, cross-locale `OPTIONS` switch-key drift, and the content
    /// policy. A tree this rejects can never reach an image.
    #[test]
    fn every_discovered_tree_passes_the_shared_lint() {
        assert!(!HELP_FILES.is_empty(), "at least one help tree must exist");
        let docs: Vec<LintDoc<'_>> = HELP_FILES
            .iter()
            .map(|doc| LintDoc {
                bundle: doc.bundle,
                locale: doc.locale,
                file: doc.file,
                bytes: doc.bytes,
            })
            .collect();
        let violations = lint_help_trees(&docs);
        assert!(violations.is_empty(), "{}", violations.join("\n"));
    }

    /// The discovered desktop graphics assets are non-empty and every one
    /// satisfies its own family's contract: an icon is a legal
    /// `<asset-id>.png` name within the artwork byte bound with a unique
    /// asset id, and a wallpaper is a legal shipped file name within the
    /// wallpaper byte bound with a unique name. Dispatching on
    /// [`super::GraphicsFamilyKind`] rather than a directory string means a
    /// third family added here without a matching arm fails to compile,
    /// never silently skips its own contract. This mirrors the fail-closed
    /// checks `build.rs` applies — the emitted table and each family's own
    /// runtime consumer share one definition, so neither can drift.
    #[test]
    fn every_discovered_graphics_asset_satisfies_its_family_contract() {
        use super::{GraphicsFamilyKind, GRAPHICS_FILES};

        assert!(
            !GRAPHICS_FILES.is_empty(),
            "at least one desktop graphics asset must be discovered"
        );
        let mut icon_ids: BTreeSet<&str> = BTreeSet::new();
        let mut wallpaper_names: BTreeSet<&str> = BTreeSet::new();
        for asset in GRAPHICS_FILES {
            match asset.family {
                GraphicsFamilyKind::Icon => {
                    let kind = tairix_icon::artwork_kind_for_file(asset.file)
                        .unwrap_or_else(|| panic!("`{}` is a legal icon artwork name", asset.file));
                    assert!(
                        asset.bytes.len() <= tairix_icon::MAX_ARTWORK_BYTES,
                        "`{}` is within the artwork byte bound",
                        asset.file
                    );
                    assert!(
                        icon_ids.insert(kind.asset_id()),
                        "asset id `{}` is claimed by more than one file",
                        kind.asset_id()
                    );
                }
                GraphicsFamilyKind::Wallpaper => {
                    assert!(
                        tairix_wallpaper::is_wallpaper_file_name(asset.file),
                        "`{}` is a legal wallpaper file name",
                        asset.file
                    );
                    assert!(
                        asset.bytes.len() <= tairix_wallpaper::MAX_WALLPAPER_BYTES,
                        "`{}` is within the wallpaper byte bound",
                        asset.file
                    );
                    assert!(
                        wallpaper_names.insert(asset.file),
                        "wallpaper name `{}` is claimed by more than one file",
                        asset.file
                    );
                }
            }
        }
        assert!(!icon_ids.is_empty(), "at least one icon must be discovered");
        assert!(
            !wallpaper_names.is_empty(),
            "at least one wallpaper must be discovered"
        );
    }

    /// The shared payload walk yields every discovered file exactly once,
    /// at its `/System`-volume-relative path: a help document under
    /// `Apps/<bundle>/Help/<locale>/`, a resource under
    /// `Apps/<bundle>/Resources/`, an icon under `Graphics/Icons/`, and a
    /// wallpaper under `Graphics/Wallpapers/`. All planters drive their own
    /// `plant_nested_file` from this one walk, so this pins the count and
    /// the path spelling they share.
    #[test]
    fn the_shared_walk_visits_every_payload_file_at_its_planted_path() {
        use super::{plant_system_payload, GRAPHICS_FILES, HELP_FILES, RESOURCE_FILES};

        let mut visited: Vec<Vec<Vec<u8>>> = Vec::new();
        let outcome: Result<(), core::convert::Infallible> =
            plant_system_payload(|components, _bytes| {
                visited.push(components.iter().map(|c| c.to_vec()).collect());
                Ok(())
            });
        assert!(
            outcome.is_ok(),
            "the walk never errors when planting cannot"
        );

        assert_eq!(
            visited.len(),
            HELP_FILES.len() + RESOURCE_FILES.len() + GRAPHICS_FILES.len(),
            "every discovered file is visited exactly once"
        );
        // The known folder-icon lands at Graphics/Icons/folder.png.
        assert!(
            visited.iter().any(|c| c
                == &[
                    b"Graphics".to_vec(),
                    b"Icons".to_vec(),
                    b"folder.png".to_vec()
                ]),
            "the folder icon is planted at Graphics/Icons/folder.png"
        );
        // The shipped default wallpaper lands at Graphics/Wallpapers/tairix-dark.jpg.
        assert!(
            visited.iter().any(|c| c
                == &[
                    b"Graphics".to_vec(),
                    b"Wallpapers".to_vec(),
                    tairix_wallpaper::DEFAULT_WALLPAPER.as_bytes().to_vec(),
                ]),
            "the default wallpaper is planted at Graphics/Wallpapers/{}",
            tairix_wallpaper::DEFAULT_WALLPAPER
        );
    }
}
