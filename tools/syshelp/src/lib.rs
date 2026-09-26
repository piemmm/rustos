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
//! ships the desktop's graphics assets — today the icon class masters and
//! the shipped default wallpaper masters — under `/System/Graphics`. The
//! image builder (`tools/mkimage`) and the QEMU image fixture must plant all
//! of these onto the volume they author.
//!
//! The source of truth for each family is its own on-disk directory. This
//! crate's build script walks every program crate under `userland/`
//! (`bundles::program_crates`; each bundle named by its crate's
//! `AppInfo.toml`, never the crate directory) for `Help/` and `Resources/`,
//! and walks each single-tree graphics asset family — `lib/icon/assets/` for
//! the desktop icon masters, `lib/wallpaper/assets/` for the shipped
//! wallpaper masters — through one shared table and loop, embedding each
//! discovered file as a row in [`HELP_FILES`] / [`RESOURCE_FILES`] /
//! [`GRAPHICS_FILES`]. The planters iterate that discovered data — **never** a
//! hand-maintained list that a new file would force an edit to (the
//! duplication the charter forbids). Adding a bundle's payload is dropping
//! files under `<root>/<name>/Help/<locale>/` or `<root>/<name>/Resources/`,
//! adding an icon is dropping a `<asset-id>.png` or `<asset-id>.svg` under
//! `lib/icon/assets/`, and adding a wallpaper is dropping a
//! `.jpg`/`.jpeg`/`.png` under `lib/wallpaper/assets/`; the next build
//! rediscovers them. Payload is therefore authored in exactly one place and
//! never hardcoded into a binary or copied into the image builder.
//!
//! Each graphics family's files are additionally validated against that
//! family's own contract (`tairix_icon` for icons, `tairix_wallpaper` for
//! wallpapers) as they are discovered, so a name a consumer could never
//! resolve or an over-large file fails the build closed rather than shipping
//! an icon that would silently render as a fallback glyph or a wallpaper
//! that would never be offered.
//!
//! Both authors build their disk through this crate: [`build_system_volume`]
//! counts and plants the `/System` volume's files in one walk and chooses its
//! length, and [`assemble_disk`] lays the partitions behind the MBR. Neither
//! author keeps its own copy of the file set, the sizing, or the layout.
//!
//! The payload is `&'static [u8]` bytes embedded at build time, so this crate
//! is `no_std` and depends on no app crate: both the host image builder and
//! the freestanding QEMU fixture (which also links into the aarch64 guest
//! tail) consume it unchanged.

#![no_std]
#![forbid(unsafe_code)]
#![deny(missing_docs)]

extern crate alloc;
#[cfg(any(feature = "walk", test))]
extern crate std;

#[cfg(any(feature = "walk", test))]
pub mod bundles;

use alloc::vec;
use alloc::vec::Vec;
use core::convert::Infallible;

use tairix_partition::mbr::{self, MbrError};
use tairix_partition::{Partition, PartitionType};

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

/// The program crates the build discovered, workspace-relative.
///
/// The build script's own answer, emitted rather than re-walked, so a
/// consumer of the discovery — this crate's tests, most of all — cannot be
/// reading a different set of programs than the payload was built from.
pub const PROGRAM_CRATES: &[&str] = &include!(concat!(env!("OUT_DIR"), "/program_crates.rs"));

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
/// consumer tells the shipped families apart by matching on this enum,
/// never by comparing a directory name. Adding a family means adding a
/// variant here, which then forces every `match` over this type — in this
/// crate and in every crate that reads [`GRAPHICS_FILES`] — to say
/// explicitly what the new family means to it, rather than silently falling
/// through a default arm.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum GraphicsFamilyKind {
    /// The icon class masters: one `<asset-id>.png` raster *or*
    /// `<asset-id>.svg` vector per icon kind, resolved by the window manager
    /// and file manager by asset id.
    Icon,
    /// The shipped default wallpaper masters: one `.jpg`/`.jpeg`/`.png` per
    /// shipped master, filed one directory level deep in a category
    /// (`Space`, `TAIRiX`, …) whose own name is the label a chooser draws.
    /// A category is listed through `tairix_wallpaper::catalog_categories`
    /// and its masters through `tairix_wallpaper::catalog_entries`.
    Wallpaper,
    /// The shipped cursor sets: one `<asset-id>.svg` per cursor kind, filed
    /// one directory level deep in a set (`High Visibility`, …) whose own
    /// name is the label a chooser draws. A set is listed through
    /// `tairix_cursor::catalog_sets` and its assets resolved by
    /// `tairix_cursor::cursor_asset_kind_for_file`.
    Cursor,
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
            Self::Cursor => "Cursors",
        }
    }
}

/// One shipped desktop graphics asset, ready to plant at
/// `/System/Graphics/<family.target_dir()>[/<category>]/<file>` on the
/// read-only `/System` volume.
///
/// Unlike a [`HelpFile`] or a [`ResourceFile`] a graphics asset is not
/// per-bundle: it is desktop-wide artwork, tagged with the
/// [`GraphicsFamilyKind`] it belongs to so a future family plants through
/// this same table and loop rather than a second one.
#[derive(Clone, Copy, Debug)]
pub struct GraphicsFile {
    /// Which family this asset belongs to.
    pub family: GraphicsFamilyKind,
    /// The category directory this asset is filed under inside its family's
    /// own directory, or `None` for a family whose assets sit directly in
    /// it. Discovery decides this per family, so a flat family can never
    /// gain a category nor a categorised one lose it.
    pub category: Option<&'static str>,
    /// The asset's file name: for an icon, its stable asset id plus
    /// extension; for a wallpaper, the plain file name a consumer lists it
    /// by within its category.
    pub file: &'static str,
    /// The asset's bytes.
    pub bytes: &'static [u8],
}

/// Every desktop graphics asset, discovered from each graphics family's own
/// source tree at build time (`lib/icon/assets/` for icons,
/// `lib/wallpaper/assets/` for wallpapers, `lib/cursor/assets/` for cursor
/// sets) and validated against that
/// family's own contract as it is discovered (a name its consumer could not
/// resolve, an over-large file, or a duplicate identifier fails the build).
///
/// Rows are ordered deterministically (by family, then category, then file
/// name), so the planted store and any reproducible image are stable across
/// builds and hosts.
pub const GRAPHICS_FILES: &[GraphicsFile] =
    &include!(concat!(env!("OUT_DIR"), "/graphics_files.rs"));

/// Visit every discovered payload file — each [`HelpFile`], [`ResourceFile`]
/// and [`GraphicsFile`] — with its `/System`-volume-relative path components
/// and its bytes, stopping at the first error `visit` returns.
fn for_each_payload_file<E>(
    mut visit: impl FnMut(&[&[u8]], &[u8]) -> Result<(), E>,
) -> Result<(), E> {
    for doc in HELP_FILES {
        visit(
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
        visit(
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
        let family = asset.family.target_dir().as_bytes();
        let file = asset.file.as_bytes();
        match asset.category {
            Some(category) => visit(
                &[b"Graphics", family, category.as_bytes(), file],
                asset.bytes,
            ),
            None => visit(&[b"Graphics", family, file], asset.bytes),
        }?;
    }
    Ok(())
}

/// A caller's own files for the `/System` volume: each file's
/// volume-relative path components and its bytes.
pub type PlantedFiles<'a> = &'a [(&'a [&'a [u8]], &'a [u8])];

/// Visit every file a `/System` volume carries: the payload, then `bundles`.
fn for_each_file<E>(
    bundles: &[PlantedFiles<'_>],
    mut visit: impl FnMut(&[&[u8]], &[u8]) -> Result<(), E>,
) -> Result<(), E> {
    for_each_payload_file(&mut visit)?;
    for (components, bytes) in bundles.iter().flat_map(|set| set.iter()) {
        visit(components, bytes)?;
    }
    Ok(())
}

/// Every byte [`for_each_file`] visits.
fn planted_bytes(bundles: &[PlantedFiles<'_>]) -> u64 {
    let mut total = 0u64;
    let counted = for_each_file(bundles, |_, bytes| {
        total = total.saturating_add(u64::try_from(bytes.len()).unwrap_or(u64::MAX));
        Ok::<(), Infallible>(())
    });
    match counted {
        Ok(()) => total,
        Err(never) => match never {},
    }
}

/// A `/System` volume an author is filling at the length
/// [`build_system_volume`] chose.
pub trait SystemVolume: Sized {
    /// Why the author refused.
    type Error;
    /// The finished volume.
    type Image;

    /// Whether `error` says the volume ran out of room: the one refusal a
    /// longer volume can answer.
    fn is_no_space(error: &Self::Error) -> bool;

    /// Lay `bytes` at the volume-relative path `components`.
    ///
    /// # Errors
    ///
    /// The author's refusal, which ends this attempt.
    fn plant(&mut self, components: &[&[u8]], bytes: &[u8]) -> Result<(), Self::Error>;

    /// Complete the volume.
    ///
    /// # Errors
    ///
    /// The author's refusal, which ends this attempt.
    fn finish(self) -> Result<Self::Image, Self::Error>;
}

/// The unit a `/System` volume's length is a whole number of, and so its
/// smallest length.
///
/// A whole number of MiB keeps the partition after the volume 1 MiB-aligned.
/// It is this coarse so one grain covers the filesystem's own metadata and a
/// volume rarely needs a second attempt.
pub const SYSTEM_VOLUME_GRAIN_BYTES: u64 = 32 * 1024 * 1024;

/// Author the `/System` volume: the system payload and `bundles`, planted
/// into what `format` makes of the sector count it is given.
///
/// One walk both counts and plants the files, so the length is chosen for
/// exactly the set planted. The first length is their bytes rounded up to a
/// whole [`SYSTEM_VOLUME_GRAIN_BYTES`]. The filesystem's own metadata is
/// measured rather than estimated: a lack of room adds a grain and the
/// volume is authored again, so it is never more than one grain longer than
/// it has to be.
///
/// # Errors
///
/// The first refusal that is not a lack of room, or a lack of room at twice
/// the first length: metadata as large as the payload is an author defect,
/// not a sizing question.
pub fn build_system_volume<V: SystemVolume>(
    bundles: &[PlantedFiles<'_>],
    mut format: impl FnMut(u64) -> Result<V, V::Error>,
) -> Result<V::Image, V::Error> {
    let first = planted_bytes(bundles)
        .div_ceil(SYSTEM_VOLUME_GRAIN_BYTES)
        .max(1)
        .saturating_mul(SYSTEM_VOLUME_GRAIN_BYTES);
    let last = first.saturating_mul(2);
    let mut len = first;
    loop {
        let authored = format(len / SECTOR_BYTES as u64).and_then(|mut volume| {
            for_each_file(bundles, |components, bytes| volume.plant(components, bytes))?;
            volume.finish()
        });
        match authored {
            Err(refusal) if V::is_no_space(&refusal) && len < last => {
                len = len.saturating_add(SYSTEM_VOLUME_GRAIN_BYTES);
            }
            outcome => return outcome,
        }
    }
}

/// Bytes in one sector of the disks TAIRiX authors.
pub const SECTOR_BYTES: usize = 512;

/// First sector of the boot partition: the 1 MiB offset SD cards align to.
pub const BOOT_PART_LBA: u64 = 2048;

/// Sectors in the FAT32 boot partition: 64 MiB, room for the firmware blobs
/// and the kernel and enough clusters for a valid FAT32 volume.
pub const BOOT_PART_SECTORS: u64 = 131_072;

/// First sector of the `/System` partition, directly after the boot
/// partition.
pub const SYSTEM_PART_LBA: u64 = BOOT_PART_LBA + BOOT_PART_SECTORS;

/// A partition of the boot disk.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum DiskPartition {
    /// The FAT32 boot partition.
    Boot,
    /// The read-only `/System` partition.
    System,
    /// The encrypted data-root partition.
    Root,
}

/// Why [`assemble_disk`] refused a layout.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum DiskLayoutError {
    /// The partition is not a whole number of sectors.
    PartialSector(DiskPartition),
    /// The boot partition is not [`BOOT_PART_SECTORS`] long.
    BootLength,
    /// The partition ends beyond what the disk can address.
    OutOfRange(DiskPartition),
    /// The partition table refused the extents.
    Table(MbrError),
}

/// The boot disk: an MBR naming each partition by its role, then `boot`,
/// `system` and `root` back to back from [`BOOT_PART_LBA`].
///
/// # Errors
///
/// A [`DiskLayoutError`] when a partition is not whole sectors, the boot
/// partition is not [`BOOT_PART_SECTORS`] long, or an extent lies beyond what
/// the disk or its table can address.
pub fn assemble_disk(boot: &[u8], system: &[u8], root: &[u8]) -> Result<Vec<u8>, DiskLayoutError> {
    use DiskPartition::{Boot, Root, System};

    let sectors = |part: DiskPartition, bytes: &[u8]| {
        if !bytes.len().is_multiple_of(SECTOR_BYTES) {
            return Err(DiskLayoutError::PartialSector(part));
        }
        u64::try_from(bytes.len() / SECTOR_BYTES).map_err(|_| DiskLayoutError::OutOfRange(part))
    };
    if sectors(Boot, boot)? != BOOT_PART_SECTORS {
        return Err(DiskLayoutError::BootLength);
    }
    let system_sectors = sectors(System, system)?;
    let root_sectors = sectors(Root, root)?;
    let root_lba = SYSTEM_PART_LBA
        .checked_add(system_sectors)
        .ok_or(DiskLayoutError::OutOfRange(System))?;
    let end = root_lba
        .checked_add(root_sectors)
        .ok_or(DiskLayoutError::OutOfRange(Root))?;
    let table = mbr::encode(&[
        Partition {
            ty: PartitionType::FatBoot,
            start_lba: BOOT_PART_LBA,
            block_count: BOOT_PART_SECTORS,
        },
        Partition {
            ty: PartitionType::ARXFSSystem,
            start_lba: SYSTEM_PART_LBA,
            block_count: system_sectors,
        },
        Partition {
            ty: PartitionType::ARXFSRoot,
            start_lba: root_lba,
            block_count: root_sectors,
        },
    ])
    .map_err(DiskLayoutError::Table)?;

    let offset = |lba: u64, part: DiskPartition| {
        usize::try_from(lba)
            .ok()
            .and_then(|lba| lba.checked_mul(SECTOR_BYTES))
            .ok_or(DiskLayoutError::OutOfRange(part))
    };
    let boot_at = offset(BOOT_PART_LBA, Boot)?;
    let system_at = offset(SYSTEM_PART_LBA, System)?;
    let root_at = offset(root_lba, Root)?;
    let mut disk = vec![0u8; offset(end, Root)?];
    disk[..table.len()].copy_from_slice(&table);
    disk[boot_at..system_at].copy_from_slice(boot);
    disk[system_at..root_at].copy_from_slice(system);
    disk[root_at..].copy_from_slice(root);
    Ok(disk)
}

#[cfg(test)]
mod tests {
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

        use super::bundles::{string_entry, MANIFEST_SOURCE};

        let workspace = Path::new(env!("CARGO_MANIFEST_DIR"))
            .parent()
            .and_then(Path::parent)
            .expect("the crate lives at <workspace>/tools/syshelp");
        let mut store_of: BTreeMap<String, String> = BTreeMap::new();
        for program in super::PROGRAM_CRATES {
            let manifest = workspace.join(program).join(MANIFEST_SOURCE);
            let text = fs::read_to_string(&manifest)
                .unwrap_or_else(|e| panic!("{}: {e}", manifest.display()));
            let (Some(name), Some(kind)) =
                (string_entry(&text, "name"), string_entry(&text, "kind"))
            else {
                panic!("{}: no name or kind", manifest.display());
            };
            let store = tairix_abi::ProgramKind::from_key(kind)
                .unwrap_or_else(|| panic!("{name}: unknown kind `{kind}`"))
                .store_dir();
            store_of.insert(
                format!("{name}{}", tairix_abi::BUNDLE_SUFFIX),
                store.to_string(),
            );
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

    /// The discovered desktop graphics assets are non-empty, and every one
    /// satisfies its own family's contract.
    ///
    /// Partitioning on [`super::GraphicsFamilyKind`] rather than a
    /// directory string is what makes that true of *every* family: a family
    /// added without an arm here fails to compile rather than silently
    /// skipping its own contract. Each partition is then held to the
    /// fail-closed checks `build.rs` applies — the emitted table and each
    /// family's own runtime consumer share one definition, so neither can
    /// drift.
    #[test]
    fn every_discovered_graphics_asset_satisfies_its_family_contract() {
        use super::{GraphicsFamilyKind, GraphicsFile, GRAPHICS_FILES};

        assert!(
            !GRAPHICS_FILES.is_empty(),
            "at least one desktop graphics asset must be discovered"
        );
        let mut icons: Vec<&GraphicsFile> = Vec::new();
        let mut wallpapers: Vec<&GraphicsFile> = Vec::new();
        let mut cursors: Vec<&GraphicsFile> = Vec::new();
        for asset in GRAPHICS_FILES {
            match asset.family {
                GraphicsFamilyKind::Icon => icons.push(asset),
                GraphicsFamilyKind::Wallpaper => wallpapers.push(asset),
                GraphicsFamilyKind::Cursor => cursors.push(asset),
            }
        }
        check_icon_family(&icons);
        check_wallpaper_family(&wallpapers);
        check_cursor_family(&cursors);
    }

    /// The icon family's contract: a flat `<asset-id>.png` or
    /// `<asset-id>.svg` name within the artwork byte bound, with a unique
    /// asset id — so one kind never ships a master in both formats, of
    /// which the resolution order could only ever select the raster one.
    fn check_icon_family(assets: &[&super::GraphicsFile]) {
        assert!(!assets.is_empty(), "at least one icon must be discovered");
        let mut ids: BTreeSet<&str> = BTreeSet::new();
        for asset in assets {
            assert!(
                asset.category.is_none(),
                "the icon family is flat, so `{}` carries no category",
                asset.file
            );
            let kind = tairix_icon::artwork_kind_for_file(asset.file)
                .unwrap_or_else(|| panic!("`{}` is a legal icon artwork name", asset.file));
            assert!(
                asset.bytes.len() <= tairix_icon::MAX_ARTWORK_BYTES,
                "`{}` is within the artwork byte bound",
                asset.file
            );
            assert!(
                ids.insert(kind.asset_id()),
                "asset id `{}` is claimed by more than one file",
                kind.asset_id()
            );
        }
    }

    /// The wallpaper family's contract: a legal shipped file name within
    /// the wallpaper byte bound, filed under a legal category directory and
    /// unique within it, and the default wallpaper's own category present —
    /// without it the desktop's default choice ships nowhere.
    fn check_wallpaper_family(assets: &[&super::GraphicsFile]) {
        assert!(
            !assets.is_empty(),
            "at least one wallpaper must be discovered"
        );
        let mut seen: BTreeSet<(&str, &str)> = BTreeSet::new();
        let mut categories: BTreeSet<&str> = BTreeSet::new();
        for asset in assets {
            let category = asset
                .category
                .unwrap_or_else(|| panic!("wallpaper `{}` is filed under a category", asset.file));
            assert!(
                tairix_wallpaper::is_wallpaper_category_name(category),
                "`{category}` is a legal wallpaper category name"
            );
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
                seen.insert((category, asset.file)),
                "wallpaper `{category}/{}` is claimed by more than one file",
                asset.file
            );
            categories.insert(category);
        }
        assert!(
            categories.contains(tairix_wallpaper::DEFAULT_WALLPAPER_CATEGORY),
            "the default wallpaper's own category must be discovered, or the \
             desktop's default choice ships nowhere"
        );
    }

    /// The cursor family's contract: an asset name some cursor kind asks
    /// for, within the cursor byte bound, filed under a legal set directory
    /// and unique within it — and every shipped set covering every kind,
    /// since a missing one shows that kind's built-in cursor beside the
    /// shipped artwork and the pointer changes look as it changes shape.
    fn check_cursor_family(assets: &[&super::GraphicsFile]) {
        assert!(
            !assets.is_empty(),
            "at least one cursor asset must be discovered"
        );
        let mut kinds: BTreeSet<(&str, &str)> = BTreeSet::new();
        let mut sets: BTreeSet<&str> = BTreeSet::new();
        for asset in assets {
            let set = asset
                .category
                .unwrap_or_else(|| panic!("cursor asset `{}` is filed under a set", asset.file));
            assert!(
                tairix_cursor::is_cursor_set_name(set),
                "`{set}` is a legal cursor-set name"
            );
            let kind = tairix_cursor::cursor_asset_kind_for_file(asset.file).unwrap_or_else(|| {
                panic!("`{set}/{}` is an asset name a kind asks for", asset.file)
            });
            assert!(
                asset.bytes.len() <= tairix_cursor::MAX_CURSOR_ASSET_BYTES,
                "`{set}/{}` is within the cursor asset byte bound",
                asset.file
            );
            assert!(
                kinds.insert((set, kind.asset_id())),
                "cursor kind `{set}/{}` is claimed by more than one file",
                kind.asset_id()
            );
            sets.insert(set);
        }
        assert!(
            sets.contains(tairix_cursor::SHIPPED_CURSOR_SET),
            "the shipped cursor set must be discovered, or `cursor.set` is a \
             choice of one and the row changes nothing"
        );
        for set in &sets {
            for kind in tairix_theme::CURSOR_KINDS {
                assert!(
                    kinds.contains(&(set, kind.asset_id())),
                    "cursor set `{set}` ships no artwork for {kind:?}"
                );
            }
        }
    }

    const GRAIN: u64 = super::SYSTEM_VOLUME_GRAIN_BYTES;

    #[derive(Debug, Eq, PartialEq)]
    enum Refusal {
        NoRoom,
        Fault,
    }

    /// Each planted file's path components and length, in order.
    type Planted = Vec<(Vec<Vec<u8>>, u64)>;

    /// A volume holding `room` planted bytes, recording each file planted.
    #[derive(Debug)]
    struct Volume {
        room: u64,
        planted: Planted,
    }

    impl super::SystemVolume for Volume {
        type Error = Refusal;
        type Image = Planted;

        fn is_no_space(error: &Refusal) -> bool {
            *error == Refusal::NoRoom
        }

        fn plant(&mut self, components: &[&[u8]], bytes: &[u8]) -> Result<(), Refusal> {
            let len = u64::try_from(bytes.len()).expect("a file length fits u64");
            self.room = self.room.checked_sub(len).ok_or(Refusal::NoRoom)?;
            let path = components.iter().map(|c| c.to_vec()).collect();
            self.planted.push((path, len));
            Ok(())
        }

        fn finish(self) -> Result<Self::Image, Refusal> {
            Ok(self.planted)
        }
    }

    /// Every volume length tried, in bytes, when each volume loses
    /// `overhead` bytes to its own metadata, and the outcome.
    fn build(
        bundles: &[super::PlantedFiles<'_>],
        overhead: u64,
    ) -> (Vec<u64>, Result<Planted, Refusal>) {
        let mut tried = Vec::new();
        let outcome = super::build_system_volume(bundles, |sectors| {
            let len = sectors * super::SECTOR_BYTES as u64;
            tried.push(len);
            Ok(Volume {
                room: len.saturating_sub(overhead),
                planted: Vec::new(),
            })
        });
        (tried, outcome)
    }

    /// The shipped payload's bytes, summed independently of the walk under
    /// test.
    fn payload_bytes() -> u64 {
        let docs = HELP_FILES.iter().map(|doc| doc.bytes.len());
        let resources = super::RESOURCE_FILES.iter().map(|res| res.bytes.len());
        let graphics = super::GRAPHICS_FILES.iter().map(|asset| asset.bytes.len());
        docs.chain(resources)
            .chain(graphics)
            .map(|len| u64::try_from(len).expect("a file length fits u64"))
            .sum()
    }

    #[test]
    fn the_first_volume_is_the_planted_bytes_rounded_up_to_a_grain() {
        let (tried, outcome) = build(&[], 0);
        assert!(outcome.is_ok());
        let payload = payload_bytes();
        assert_eq!(tried, [payload.div_ceil(GRAIN).max(1) * GRAIN]);
    }

    #[test]
    fn a_bundle_counts_towards_the_first_volume() {
        let (alone, _) = build(&[], 0);
        let spill = usize::try_from(alone[0] - payload_bytes() + 1).expect("fits usize");
        let run = std::vec![0u8; spill];
        let components: &[&[u8]] = &[b"Commands", b"big.app", b"Run"];
        let bundle = [(components, run.as_slice())];
        let (tried, outcome) = build(&[&[], &bundle], 0);
        assert!(outcome.is_ok());
        assert_eq!(tried, [alone[0] + GRAIN]);
    }

    /// Metadata that overflows the first volume by a byte costs exactly one
    /// more grain.
    #[test]
    fn a_lack_of_room_adds_a_grain_until_the_content_fits() {
        let (alone, _) = build(&[], 0);
        let first = alone[0];
        let overhead = first - payload_bytes() + 1;
        let (tried, outcome) = build(&[], overhead);
        assert!(outcome.is_ok());
        assert_eq!(tried, [first, first + GRAIN]);
    }

    #[test]
    fn a_lack_of_room_at_twice_the_first_length_is_returned() {
        let (tried, outcome) = build(&[], u64::MAX);
        let first = tried[0];
        let expected: Vec<u64> = (0..=first / GRAIN)
            .map(|step| first + step * GRAIN)
            .collect();
        assert_eq!(tried, expected);
        assert_eq!(tried.last(), Some(&(first * 2)));
        assert_eq!(outcome, Err(Refusal::NoRoom));
    }

    #[test]
    fn a_refusal_other_than_a_lack_of_room_is_returned_at_once() {
        let mut tried = 0;
        let outcome = super::build_system_volume(&[], |_| -> Result<Volume, Refusal> {
            tried += 1;
            Err(Refusal::Fault)
        });
        assert_eq!(outcome.unwrap_err(), Refusal::Fault);
        assert_eq!(tried, 1);
    }

    /// What a volume is sized for is exactly what is planted in it: every
    /// payload file and every bundle file, each once.
    #[test]
    fn the_counted_set_and_the_planted_set_are_one() {
        let components: &[&[u8]] = &[b"Drivers", b"input", b"kbd", b"Run"];
        let bundle = [(components, b"a signed driver".as_slice())];
        let (tried, outcome) = build(&[&bundle], 0);
        let planted = outcome.expect("the volume is authored");
        assert_eq!(
            planted.len(),
            HELP_FILES.len() + super::RESOURCE_FILES.len() + super::GRAPHICS_FILES.len() + 1
        );
        let planted_bytes: u64 = planted.iter().map(|(_, len)| len).sum();
        assert_eq!(planted_bytes, payload_bytes() + 15);
        assert_eq!(tried, [planted_bytes.div_ceil(GRAIN) * GRAIN]);
        assert!(planted.iter().any(|(path, _)| path
            .iter()
            .map(Vec::as_slice)
            .eq(components.iter().copied())));
    }

    /// A partition of `sectors` sectors of `fill`, so its placement reads
    /// back.
    fn part(sectors: u64, fill: u8) -> Vec<u8> {
        std::vec![fill; usize::try_from(sectors).expect("fits usize") * super::SECTOR_BYTES]
    }

    #[test]
    fn the_disk_packs_its_partitions_back_to_back_behind_the_table() {
        use super::{BOOT_PART_LBA, BOOT_PART_SECTORS, SECTOR_BYTES, SYSTEM_PART_LBA};
        use tairix_partition::PartitionType;

        let disk = super::assemble_disk(
            &part(BOOT_PART_SECTORS, 0xB0),
            &part(3, 0x5E),
            &part(2, 0x7A),
        )
        .expect("the disk assembles");
        let table = tairix_partition::mbr::parse(&disk[..SECTOR_BYTES]).expect("the MBR parses");
        let extent = |ty| {
            let found = table.first_of_type(ty).expect("the partition is present");
            (found.start_lba, found.block_count)
        };
        assert_eq!(
            extent(PartitionType::FatBoot),
            (BOOT_PART_LBA, BOOT_PART_SECTORS)
        );
        assert_eq!(extent(PartitionType::ARXFSSystem), (SYSTEM_PART_LBA, 3));
        assert_eq!(extent(PartitionType::ARXFSRoot), (SYSTEM_PART_LBA + 3, 2));
        let at = |lba: u64| usize::try_from(lba).expect("fits usize") * SECTOR_BYTES;
        assert_eq!(disk.len(), at(SYSTEM_PART_LBA + 5));
        assert!(disk[at(BOOT_PART_LBA)..at(SYSTEM_PART_LBA)]
            .iter()
            .all(|&b| b == 0xB0));
        assert!(disk[at(SYSTEM_PART_LBA)..at(SYSTEM_PART_LBA + 3)]
            .iter()
            .all(|&b| b == 0x5E));
        assert!(disk[at(SYSTEM_PART_LBA + 3)..].iter().all(|&b| b == 0x7A));
    }

    #[test]
    fn a_partition_that_is_not_whole_sectors_is_refused() {
        use super::{DiskLayoutError::PartialSector, DiskPartition};

        let boot = part(super::BOOT_PART_SECTORS, 0);
        let whole = part(1, 0);
        let ragged = std::vec![0u8; super::SECTOR_BYTES + 1];
        assert_eq!(
            super::assemble_disk(&ragged, &whole, &whole),
            Err(PartialSector(DiskPartition::Boot))
        );
        assert_eq!(
            super::assemble_disk(&boot, &ragged, &whole),
            Err(PartialSector(DiskPartition::System))
        );
        assert_eq!(
            super::assemble_disk(&boot, &whole, &ragged),
            Err(PartialSector(DiskPartition::Root))
        );
    }

    #[test]
    fn a_boot_partition_of_any_other_length_is_refused() {
        let whole = part(1, 0);
        for sectors in [
            1,
            super::BOOT_PART_SECTORS - 1,
            super::BOOT_PART_SECTORS + 1,
        ] {
            assert_eq!(
                super::assemble_disk(&part(sectors, 0), &whole, &whole),
                Err(super::DiskLayoutError::BootLength)
            );
        }
    }

    /// The payload walk yields every discovered file exactly once, at its
    /// `/System`-volume-relative path: a help document under
    /// `Apps/<bundle>/Help/<locale>/`, a resource under
    /// `Apps/<bundle>/Resources/`, an icon under `Graphics/Icons/` by its
    /// discovered file name whatever its format, and a wallpaper under
    /// `Graphics/Wallpapers/<category>/`.
    #[test]
    fn the_payload_walk_visits_every_file_at_its_planted_path() {
        use super::{for_each_payload_file, GRAPHICS_FILES, HELP_FILES, RESOURCE_FILES};

        let mut visited: Vec<Vec<Vec<u8>>> = Vec::new();
        let outcome: Result<(), core::convert::Infallible> =
            for_each_payload_file(|components, _bytes| {
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
        // The folder class icon is the shipped vector master, and the
        // generic file icon the shipped raster one.
        for icon in ["folder.svg", "file.png"] {
            assert!(
                visited.iter().any(|c| c
                    == &[
                        b"Graphics".to_vec(),
                        b"Icons".to_vec(),
                        icon.as_bytes().to_vec()
                    ]),
                "the {icon} icon is planted at Graphics/Icons/{icon}"
            );
        }
        // The shipped default wallpaper lands inside its own category.
        assert!(
            visited.iter().any(|c| c
                == &[
                    b"Graphics".to_vec(),
                    b"Wallpapers".to_vec(),
                    tairix_wallpaper::DEFAULT_WALLPAPER_CATEGORY
                        .as_bytes()
                        .to_vec(),
                    tairix_wallpaper::DEFAULT_WALLPAPER.as_bytes().to_vec(),
                ]),
            "the default wallpaper is planted at Graphics/Wallpapers/{}/{}",
            tairix_wallpaper::DEFAULT_WALLPAPER_CATEGORY,
            tairix_wallpaper::DEFAULT_WALLPAPER
        );
    }
}
