//! Cross-compile, compose, and sign the self-contained application bundles
//! the images ship on the read-only `/System` volume (`plans/APPS.md`
//! deliverable 8): every command app under `/System/Commands/<name>.app/`,
//! every graphical application under `/System/Applications/<name>.app/`, and
//! every service under `/System/Services/<name>.app/`, each a complete
//! on-disk bundle — its signed `AppInfo` and `Run` rxe planted beside its
//! `Help/` tree. Which store a bundle lands in is its manifest's own
//! declared kind, never a list kept here.
//!
//! `tools/mkimage` and the QEMU whole-disk fixture are pure planters: they
//! lay bundle *bytes* onto the volume but never drive `cargo`. This module
//! is the orchestration half — it discovers every program crate's
//! `AppInfo.toml` manifest source by walking the userland crate roots
//! (`tairix_itest_harness::app_image::discover_app_manifests` — never a
//! hand-maintained per-bundle list), compiles each crate's `Run` binary
//! position-independent for the freestanding aarch64 target through the
//! shared `pie_build` recipe, converts the linked PIE ELF to an `rxe`
//! image (relocated for the production user-image bias and stamped with
//! the kernel's compiled-in syscall CFI tag), and composes the bundle's
//! Ed25519-signed wire `AppInfo` over the exact contents the volume ships
//! — the `Run` rxe plus the bundle's discovered `Help/` documents
//! (`tairix_syshelp`), so a tampered file or a swapped capability id fails
//! the verifier closed.
//!
//! Bundles are signed with the dedicated system-app seed
//! ([`build_support::SYSTEM_APP_SIGNING_SEED`]) — a trust domain distinct
//! from the driver-signing seed, so an app-signing authority can never
//! admit a driver. This is host-only build glue; the production image
//! stays Rust-only.

use std::sync::OnceLock;

use tairix_abi::{AppInfoHeader, BundleEntry, BundleFileDigest, APPINFO_MAGIC};
use tairix_fontface::FAMILY_MANIFEST;
use tairix_image::{DecodeLimits, ImageFormat};
use tairix_itest_harness::app_image::{
    compose_signed_appinfo, discover_app_manifests, discover_crate_manifest, DiscoveredApp,
    APP_MANIFEST_SOURCE,
};
use tairix_itest_harness::elf2rxe::elf_to_rxe;
use tairix_itest_harness::pie::PieArch;
use tairix_itest_harness::USER_IMAGE_BIAS;
use tairix_mkimage::ImageProfile;

use super::font_store;
use super::image_drivers::build_support;
use super::pie_build::cross_compile_pie_elf;
use crate::Context;

/// The number of memo slots a `(arch, profile)`-keyed bundle table needs:
/// one per architecture per image profile. The image gate builds the
/// `debug` and `installer` images in one process, so a table keyed by arch
/// alone would hand the second profile the first profile's composed bundles
/// (which are built in a different Cargo profile) — [`memo_slot`] keys by
/// both to keep them apart.
pub(super) const MEMO_SLOTS: usize = PieArch::COUNT * ImageProfile::COUNT;

/// The stable slot a `(arch, profile)` pair occupies in a
/// [`MEMO_SLOTS`]-sized memo table, so each pair memoises its own composed
/// set without a runtime map. The single definition every `(arch, profile)`
/// memo in the image pipeline addresses.
pub(super) const fn memo_slot(arch: PieArch, profile: ImageProfile) -> usize {
    arch.index() * ImageProfile::COUNT + profile.index()
}

/// One composed, signed application bundle ready to plant: the store it
/// lives in, its bundle directory, and its two file payloads. The bundle's
/// `Help/` documents are planted separately from their own discovered
/// source (`tairix_syshelp`); the `AppInfo` content hash covers them.
pub struct BuiltAppBundle {
    /// The `/System`-volume-relative store directory (`Commands`,
    /// `Applications`, or `Services`), from the bundle's declared kind.
    pub store_dir: &'static str,
    /// The bundle directory name (`<name>.app`).
    pub bundle_dir: String,
    /// The signed wire `AppInfo` manifest bytes.
    pub appinfo: Vec<u8>,
    /// The `Run` rxe image bytes.
    pub run: Vec<u8>,
}

/// One `/System`-volume-relative file a built bundle plants, as owned path
/// components plus bytes (e.g. `Apps/ls.app/AppInfo`). `Clone` so a
/// vertical-specific plant list can extend the shared composed set without
/// re-composing it.
#[derive(Clone)]
pub struct AppStoreFile {
    /// The path components relative to the `/System` volume root.
    pub components: Vec<Vec<u8>>,
    /// The file's bytes.
    pub bytes: Vec<u8>,
}

/// Build every discovered application bundle for `arch`'s image: walk
/// the userland crate roots for `AppInfo.toml` manifest sources, compile
/// each crate's `Run` binary for that target, and compose its signed
/// `AppInfo`.
///
/// # Errors
///
/// A string describing a failed discovery walk, cross-compile, ELF→rxe
/// conversion, composition, or a composed manifest that fails the
/// fail-closed sanity check.
pub fn build_app_bundles(
    ctx: &Context,
    arch: PieArch,
    profile: ImageProfile,
) -> Result<Vec<BuiltAppBundle>, String> {
    let userland = ctx.workspace_root.join("userland");
    let discovered = discover_app_manifests(&userland)
        .map_err(|e| format!("image: app-manifest discovery: {e}"))?;
    discovered
        .iter()
        .map(|app| build_bundle(ctx, arch, app, profile))
        .collect()
}

/// Flatten built bundles into the `/System`-volume-relative file list the
/// planters lay down: each bundle's `AppInfo` and `Run` under
/// `<store>/<name>.app/`. The one definition of the planted bundle-file
/// spelling, shared by the Pi image build and the QEMU fixture plant.
#[must_use]
pub fn store_files(bundles: &[BuiltAppBundle]) -> Vec<AppStoreFile> {
    let mut files = Vec::with_capacity(bundles.len() * 2);
    for bundle in bundles {
        for (entry, bytes) in [
            (BundleEntry::AppInfo, &bundle.appinfo),
            (BundleEntry::Run, &bundle.run),
        ] {
            files.push(AppStoreFile {
                components: vec![
                    bundle.store_dir.as_bytes().to_vec(),
                    bundle.bundle_dir.as_bytes().to_vec(),
                    entry.as_str().as_bytes().to_vec(),
                ],
                bytes: bytes.clone(),
            });
        }
    }
    files
}

/// The composed application-bundle store files, built **once per xtask
/// process** and shared by every consumer: the Pi image build and every
/// QEMU enrolment/pass plant the identical bundle set, so composing (one
/// cross-compile per bundle plus signing) is paid a single time. The first caller
/// builds; concurrent first callers are serialised by cargo's own build
/// locking and one result wins. A composition failure is memoised too and
/// returned to every caller (fail closed, never a partial store).
///
/// # Errors
///
/// As [`build_app_bundles`].
pub fn app_store_files(
    ctx: &Context,
    arch: PieArch,
    profile: ImageProfile,
) -> Result<&'static [AppStoreFile], String> {
    static FILES: [OnceLock<Result<Vec<AppStoreFile>, String>>; MEMO_SLOTS] =
        [const { OnceLock::new() }; MEMO_SLOTS];
    FILES[memo_slot(arch, profile)]
        .get_or_init(|| {
            let mut files = build_app_bundles(ctx, arch, profile).map(|b| store_files(&b))?;
            // The `/System/Fonts` family store the `fontd` service discovers
            // ships on every image alongside the bundles.
            files.extend(system_font_files(ctx)?);
            Ok(files)
        })
        .as_ref()
        .map(Vec::as_slice)
        .map_err(Clone::clone)
}

/// The `/System/Fonts` store the image plants: every family directory under
/// `lib/font/assets/`, laid down at `Fonts/<family>/<file>` — the family's
/// `FontFamily` manifest and the TrueType faces it lists — so the sandboxed
/// `fontd` font service can discover and load them (`plans/FONT-SERVICE.md`
/// FS-6). Discovered from the on-disk assets rather than a hand-maintained
/// list, so a family ships by dropping its directory into the assets tree;
/// `fontd` opens exactly these paths.
///
/// Licence texts and any other file beside the faces stay on the build host:
/// the service reads only manifests and faces, and an image is not the place
/// to ship bytes nothing reads.
///
/// # Errors
///
/// A string describing a failed read of the font-assets tree, a family that
/// carries no manifest, or a manifest naming a face that is not there.
fn system_font_files(ctx: &Context) -> Result<Vec<AppStoreFile>, String> {
    let families = font_store::read_store(&ctx.workspace_root)?;
    if families.is_empty() {
        return Err(format!(
            "image: no font family under {} — the desktop would have no text",
            font_store::ASSETS_DIR
        ));
    }
    let mut planted = Vec::new();
    for family in families {
        let key = family.key.into_bytes();
        let plant = |name: &str, bytes: Vec<u8>| AppStoreFile {
            components: vec![b"Fonts".to_vec(), key.clone(), name.as_bytes().to_vec()],
            bytes,
        };
        planted.push(plant(FAMILY_MANIFEST, family.manifest_text.into_bytes()));
        for (name, bytes) in family.faces {
            planted.push(plant(&name, bytes));
        }
    }
    Ok(planted)
}

/// The composed store files a single-fixture vertical's disk plants: the
/// shared [`app_store_files`] set **plus** one test-only fixture bundle whose
/// crate lives at `crate_dir` (outside the userland discovery walk, so it is
/// planted only on that vertical's disk, never on a production image),
/// composed through the same discovery/compose/sign path as every store
/// bundle. `label` names the fixture in error messages; `cache` memoises the
/// per-arch result like the shared set. The one definition every
/// single-fixture vertical's store helper below is built on, never copied.
///
/// # Errors
///
/// As [`build_app_bundles`], plus a failed fixture-manifest discovery.
fn fixture_store_files(
    ctx: &Context,
    arch: PieArch,
    profile: ImageProfile,
    crate_dir: &str,
    label: &str,
    cache: &'static [OnceLock<Result<Vec<AppStoreFile>, String>>; MEMO_SLOTS],
) -> Result<&'static [AppStoreFile], String> {
    cache[memo_slot(arch, profile)]
        .get_or_init(|| {
            let base = app_store_files(ctx, arch, profile)?;
            let crate_dir = ctx.workspace_root.join(crate_dir);
            let app = discover_crate_manifest(&crate_dir)
                .map_err(|e| format!("image: {label} manifest discovery: {e}"))?
                .ok_or_else(|| {
                    format!(
                        "image: {} has no {APP_MANIFEST_SOURCE}",
                        crate_dir.display()
                    )
                })?;
            let bundle = build_bundle(ctx, arch, &app, profile)?;
            let mut files = base.to_vec();
            files.extend(store_files(&[bundle]));
            Ok(files)
        })
        .as_ref()
        .map(Vec::as_slice)
        .map_err(Clone::clone)
}

/// The composed store files the memory-stability vertical's disk plants: the
/// shared [`app_store_files`] set plus the test-only `memsoak` fixture bundle
/// (`plans/APPS.md` "Immediate work" I2/I3), memoised per arch.
///
/// # Errors
///
/// As [`fixture_store_files`].
pub fn memsoak_store_files(
    ctx: &Context,
    arch: PieArch,
    profile: ImageProfile,
) -> Result<&'static [AppStoreFile], String> {
    static FILES: [OnceLock<Result<Vec<AppStoreFile>, String>>; MEMO_SLOTS] =
        [const { OnceLock::new() }; MEMO_SLOTS];
    fixture_store_files(
        ctx,
        arch,
        profile,
        "tests/integration/memsoak_program",
        "memsoak",
        &FILES,
    )
}

/// The composed store files the stream vertical's disk plants: the shared
/// [`app_store_files`] set plus the test-only `tcpecho` stream-socket client
/// fixture bundle (`plans/NETWORK.md` N5c), memoised per arch.
///
/// # Errors
///
/// As [`fixture_store_files`].
pub fn tcpecho_store_files(
    ctx: &Context,
    arch: PieArch,
    profile: ImageProfile,
) -> Result<&'static [AppStoreFile], String> {
    static FILES: [OnceLock<Result<Vec<AppStoreFile>, String>>; MEMO_SLOTS] =
        [const { OnceLock::new() }; MEMO_SLOTS];
    fixture_store_files(
        ctx,
        arch,
        profile,
        "tests/integration/tcpecho_program",
        "tcpecho",
        &FILES,
    )
}

/// The composed store files the ECN vertical's disk plants: the stream
/// vertical's `tcpecho`-augmented set (so the client fixture and the standard
/// service bundles are on disk exactly as the stream vertical plants them)
/// **plus** a planted `/System/Settings/Configuration/system.conf`
/// ([`tairix_test_netstack_wire::ECN_SYSTEM_CONF`]) that turns `net.tcp.ecn`
/// on stack-wide (`plans/NETWORK.md` N13). `devmgr` reads the planted store
/// pre-unlock over the read-only `/System` endpoint and delivers `tcp_ecn =
/// true` to `netstack`, so the guest's connection negotiates ECN. No new
/// test-only *bundle* is added over the stream set — the config file is the
/// only extra — so this is a thin wrapper over [`tcpecho_store_files`],
/// memoised per arch like the shared set.
///
/// # Errors
///
/// As [`tcpecho_store_files`].
pub fn ecn_net_store_files(
    ctx: &Context,
    arch: PieArch,
    profile: ImageProfile,
) -> Result<&'static [AppStoreFile], String> {
    static FILES: [OnceLock<Result<Vec<AppStoreFile>, String>>; MEMO_SLOTS] =
        [const { OnceLock::new() }; MEMO_SLOTS];
    FILES[memo_slot(arch, profile)]
        .get_or_init(|| {
            let mut files = tcpecho_store_files(ctx, arch, profile)?.to_vec();
            files.push(AppStoreFile {
                components: vec![
                    b"Settings".to_vec(),
                    b"Configuration".to_vec(),
                    b"system.conf".to_vec(),
                ],
                bytes: tairix_test_netstack_wire::ECN_SYSTEM_CONF
                    .as_bytes()
                    .to_vec(),
            });
            Ok(files)
        })
        .as_ref()
        .map(Vec::as_slice)
        .map_err(Clone::clone)
}

/// The composed store files the listener vertical's disk plants: the shared
/// [`app_store_files`] set plus the test-only `tcpserve` TCP-listener server
/// fixture bundle (`plans/NETWORK.md` N6b-2-β-2), memoised per arch.
///
/// # Errors
///
/// As [`fixture_store_files`].
pub fn tcpserve_store_files(
    ctx: &Context,
    arch: PieArch,
    profile: ImageProfile,
) -> Result<&'static [AppStoreFile], String> {
    static FILES: [OnceLock<Result<Vec<AppStoreFile>, String>>; MEMO_SLOTS] =
        [const { OnceLock::new() }; MEMO_SLOTS];
    fixture_store_files(
        ctx,
        arch,
        profile,
        "tests/integration/tcpserve_program",
        "tcpserve",
        &FILES,
    )
}

/// The composed store files the static-addressing vertical's disk plants:
/// the shared [`app_store_files`] set (so the `netstack`/`devmgr` service
/// bundles are on disk exactly as a real image ships them) **plus** the
/// planted `/System/Settings/Network/network.conf`
/// ([`tairix_test_netstack_wire::STATIC_NETWORK_CONF_AARCH64`]) that binds the NIC
/// by `match.node` and assigns it a static IPv6 address (`plans/NETWORK.md`
/// N9b-3-2-β-2-ii-b). No test-only *bundle* is added — the config file is
/// the only extra — so this is not a [`fixture_store_files`] consumer;
/// memoised per arch like the shared set.
///
/// # Errors
///
/// As [`app_store_files`].
pub fn static_net_store_files(
    ctx: &Context,
    arch: PieArch,
    profile: ImageProfile,
) -> Result<&'static [AppStoreFile], String> {
    static FILES: [OnceLock<Result<Vec<AppStoreFile>, String>>; MEMO_SLOTS] =
        [const { OnceLock::new() }; MEMO_SLOTS];
    FILES[memo_slot(arch, profile)]
        .get_or_init(|| {
            let mut files = app_store_files(ctx, arch, profile)?.to_vec();
            // The static vertical binds the NIC by its stable bus location
            // (`<iface>.match.node`), which differs per bus: the aarch64/riscv64
            // virtio-mmio slot base vs. the x86_64 virtio-PCI config-window BAR
            // base. Plant the config whose `match.node` names the NIC location
            // this target's kernel actually resolves, so the same fixture cannot
            // silently mis-bind on the wrong arch.
            let conf = match arch {
                PieArch::X86_64 => tairix_test_netstack_wire::STATIC_NETWORK_CONF_X86_64,
                PieArch::Riscv64 => tairix_test_netstack_wire::STATIC_NETWORK_CONF_RISCV64,
                PieArch::Aarch64 => tairix_test_netstack_wire::STATIC_NETWORK_CONF_AARCH64,
            };
            files.push(AppStoreFile {
                components: vec![
                    b"Settings".to_vec(),
                    b"Network".to_vec(),
                    b"network.conf".to_vec(),
                ],
                bytes: conf.as_bytes().to_vec(),
            });
            Ok(files)
        })
        .as_ref()
        .map(Vec::as_slice)
        .map_err(Clone::clone)
}

/// The composed store files the bond-failover vertical's disk plants: the
/// shared [`app_store_files`] set **plus** the planted
/// `/System/Settings/Network/network.conf`
/// ([`tairix_test_netstack_wire::BOND_NETWORK_CONF`]) that binds two NICs by
/// `match.mac` as the members of one active-backup bond carrying a static
/// IPv6 address (`plans/NETWORK.md` N9b-3-2-β-2-ii-b-bond). The sibling of
/// [`static_net_store_files`] — same shape, a different config — so the two
/// verticals cannot share a config by accident; memoised per arch.
///
/// # Errors
///
/// As [`app_store_files`].
pub fn bond_net_store_files(
    ctx: &Context,
    arch: PieArch,
    profile: ImageProfile,
) -> Result<&'static [AppStoreFile], String> {
    static FILES: [OnceLock<Result<Vec<AppStoreFile>, String>>; MEMO_SLOTS] =
        [const { OnceLock::new() }; MEMO_SLOTS];
    FILES[memo_slot(arch, profile)]
        .get_or_init(|| {
            let mut files = app_store_files(ctx, arch, profile)?.to_vec();
            files.push(AppStoreFile {
                components: vec![
                    b"Settings".to_vec(),
                    b"Network".to_vec(),
                    b"network.conf".to_vec(),
                ],
                bytes: tairix_test_netstack_wire::BOND_NETWORK_CONF
                    .as_bytes()
                    .to_vec(),
            });
            Ok(files)
        })
        .as_ref()
        .map(Vec::as_slice)
        .map_err(Clone::clone)
}

/// The composed store files the DHCPv4 vertical's disk plants: the shared
/// [`app_store_files`] set **plus** the planted
/// `/System/Settings/Network/network.conf` (the per-arch
/// `tairix_test_netstack_wire::DHCP_NETWORK_CONF_*`) that binds the
/// NIC by `match.node`, selects `ipv4.method dhcp`, and disables IPv6 (DHCP
/// D3). The sibling of [`static_net_store_files`] — same shape, a different
/// config — so the two verticals cannot share a config by accident; memoised
/// per arch. Like [`static_net_store_files`], the `match.node` bus location
/// differs per bus, so the config planted names the NIC location this
/// target's kernel actually resolves (the aarch64/riscv64 virtio-mmio slot
/// base vs. the x86_64 virtio-PCI config-window BAR base).
///
/// # Errors
///
/// As [`app_store_files`].
pub fn dhcp_net_store_files(
    ctx: &Context,
    arch: PieArch,
    profile: ImageProfile,
) -> Result<&'static [AppStoreFile], String> {
    static FILES: [OnceLock<Result<Vec<AppStoreFile>, String>>; MEMO_SLOTS] =
        [const { OnceLock::new() }; MEMO_SLOTS];
    FILES[memo_slot(arch, profile)]
        .get_or_init(|| {
            let mut files = app_store_files(ctx, arch, profile)?.to_vec();
            // The DHCP vertical binds the NIC by its stable bus location
            // (`<iface>.match.node`), which differs per bus: the aarch64/riscv64
            // virtio-mmio slot base vs. the x86_64 virtio-PCI config-window BAR
            // base. Plant the config whose `match.node` names the NIC location
            // this target's kernel actually resolves, so the same fixture cannot
            // silently mis-bind on the wrong arch.
            let conf = match arch {
                PieArch::X86_64 => tairix_test_netstack_wire::DHCP_NETWORK_CONF_X86_64,
                PieArch::Riscv64 => tairix_test_netstack_wire::DHCP_NETWORK_CONF_RISCV64,
                PieArch::Aarch64 => tairix_test_netstack_wire::DHCP_NETWORK_CONF_AARCH64,
            };
            files.push(AppStoreFile {
                components: vec![
                    b"Settings".to_vec(),
                    b"Network".to_vec(),
                    b"network.conf".to_vec(),
                ],
                bytes: conf.as_bytes().to_vec(),
            });
            Ok(files)
        })
        .as_ref()
        .map(Vec::as_slice)
        .map_err(Clone::clone)
}

/// The composed store files the DHCPv6 vertical's disk plants: the shared
/// [`app_store_files`] set **plus** the planted
/// `/System/Settings/Network/network.conf` (the per-arch
/// `tairix_test_netstack_wire::DHCP6_NETWORK_CONF_*`) that binds the NIC by
/// `match.node`, selects `ipv6.method dhcp`, and disables IPv4 (DHCP D4c).
/// The IPv6 sibling of [`dhcp_net_store_files`] — same shape, a different
/// config — so the two verticals cannot share a config by accident; memoised
/// per arch. Like [`dhcp_net_store_files`], the `match.node` bus location
/// differs per bus, so the config planted names the NIC location this
/// target's kernel actually resolves.
///
/// # Errors
///
/// As [`app_store_files`].
pub fn dhcp6_net_store_files(
    ctx: &Context,
    arch: PieArch,
    profile: ImageProfile,
) -> Result<&'static [AppStoreFile], String> {
    static FILES: [OnceLock<Result<Vec<AppStoreFile>, String>>; MEMO_SLOTS] =
        [const { OnceLock::new() }; MEMO_SLOTS];
    FILES[memo_slot(arch, profile)]
        .get_or_init(|| {
            let mut files = app_store_files(ctx, arch, profile)?.to_vec();
            // The DHCPv6 vertical binds the NIC by its stable bus location
            // (`<iface>.match.node`), which differs per bus: the aarch64/riscv64
            // virtio-mmio slot base vs. the x86_64 virtio-PCI config-window BAR
            // base. Plant the config whose `match.node` names the NIC location
            // this target's kernel actually resolves, so the same fixture cannot
            // silently mis-bind on the wrong arch.
            let conf = match arch {
                PieArch::X86_64 => tairix_test_netstack_wire::DHCP6_NETWORK_CONF_X86_64,
                PieArch::Riscv64 => tairix_test_netstack_wire::DHCP6_NETWORK_CONF_RISCV64,
                PieArch::Aarch64 => tairix_test_netstack_wire::DHCP6_NETWORK_CONF_AARCH64,
            };
            files.push(AppStoreFile {
                components: vec![
                    b"Settings".to_vec(),
                    b"Network".to_vec(),
                    b"network.conf".to_vec(),
                ],
                bytes: conf.as_bytes().to_vec(),
            });
            Ok(files)
        })
        .as_ref()
        .map(Vec::as_slice)
        .map_err(Clone::clone)
}

/// Run `body` over the borrowed `(components, bytes)` view of `files` —
/// the planting shape `tools/mkimage` and the QEMU fixture accept. The
/// borrow gymnastics live here once instead of at every call site.
pub fn with_plant_refs<R>(
    files: &[AppStoreFile],
    body: impl FnOnce(&[(&[&[u8]], &[u8])]) -> R,
) -> R {
    let components: Vec<Vec<&[u8]>> = files
        .iter()
        .map(|f| f.components.iter().map(Vec::as_slice).collect())
        .collect();
    let refs: Vec<(&[&[u8]], &[u8])> = files
        .iter()
        .zip(&components)
        .map(|(f, c)| (c.as_slice(), f.bytes.as_slice()))
        .collect();
    body(&refs)
}

/// Build one discovered program's bundle: compile its `Run` binary, convert
/// it to the production-biased rxe, and compose the signed `AppInfo` over
/// the bundle's shipped contents (`Run` plus its `Help/` documents).
fn build_bundle(
    ctx: &Context,
    arch: PieArch,
    app: &DiscoveredApp,
    profile: ImageProfile,
) -> Result<BuiltAppBundle, String> {
    // Every program crate's `Run` binary is named `<package>-run`
    // (the kernel `build.rs` builds the same bins); the artefact is read
    // back fail-closed, so a crate that breaks the convention fails the
    // build loudly.
    let bin = format!("{}-run", app.package);
    let elf = cross_compile_pie_elf(ctx, arch, "image-apps", &app.package, &bin, profile)?;
    let run = elf_to_rxe(
        &elf,
        &tairix_kernel_syscall::SYSCALL_TABLE_HASH,
        USER_IMAGE_BIAS,
    )
    .map_err(|e| format!("image: convert {} ELF to rxe: {e}", app.package))?;

    let bundle_dir = app.manifest.bundle_dir();

    // The signed content hash covers every file the bundle ships except
    // `AppInfo` itself: the `Run` rxe plus the bundle's `Help/` documents
    // and `Resources/` files, exactly as the planters lay them down (the
    // help and resource bytes come from the same discovered source the
    // planters plant, so image and signature cannot drift). Paths are
    // bundle-relative and byte-sorted, the order the canonical digest
    // framing requires.
    let mut contents: Vec<(String, &[u8])> = tairix_syshelp::HELP_FILES
        .iter()
        .filter(|doc| doc.bundle == bundle_dir)
        .map(|doc| {
            let path = format!("{}/{}/{}", BundleEntry::Help.as_str(), doc.locale, doc.file);
            (path, doc.bytes)
        })
        .collect();
    contents.extend(
        tairix_syshelp::RESOURCE_FILES
            .iter()
            .filter(|res| res.bundle == bundle_dir)
            .map(|res| {
                let path = format!("{}/{}", BundleEntry::Resources.as_str(), res.file);
                (path, res.bytes)
            }),
    );
    contents.push((BundleEntry::Run.as_str().to_string(), run.as_slice()));
    contents.sort_by(|a, b| a.0.cmp(&b.0));

    // A declared `library-icon` is a file the desktop rasterises from the
    // bundle's own `Resources/`; the discovered resource bytes are the exact
    // bytes the image ships and the desktop would decode, so verifying them
    // here — where the bundle is composed — is verifying what the image
    // actually carries.
    verify_library_icon(
        &bundle_dir,
        app.manifest.library_icon.as_deref(),
        tairix_syshelp::RESOURCE_FILES
            .iter()
            .filter(|res| res.bundle == bundle_dir)
            .map(|res| (res.file, res.bytes)),
    )?;

    let digests: Vec<BundleFileDigest<'_>> = contents
        .iter()
        .map(|(path, bytes)| BundleFileDigest { path, bytes })
        .collect();

    let composed = compose_signed_appinfo(
        &build_support::SYSTEM_APP_SIGNING_SEED,
        &app.manifest,
        tairix_kernel_syscall::SYSCALL_TABLE_HASH,
        &digests,
    )
    .map_err(|e| format!("image: compose {} AppInfo: {e}", app.package))?;
    verify_composed_appinfo(&composed.bytes, &app.manifest.name)?;

    Ok(BuiltAppBundle {
        store_dir: app.manifest.kind.store_dir(),
        bundle_dir,
        appinfo: composed.bytes,
        run,
    })
}

/// Fail-closed sanity check on a freshly composed `AppInfo` before it is
/// planted (never ship a malformed store entry): it must re-decode through
/// the same `tairix_abi` definition the bundle verifier uses, carry the
/// manifest magic, name the bundle it was composed for, and be signed. The
/// signature *verifies* against the app trust anchor by construction (it is
/// signed with the same seed the anchor derives from); the composer's own
/// unit tests prove the full verification contract.
fn verify_composed_appinfo(bytes: &[u8], name: &str) -> Result<(), String> {
    let header = AppInfoHeader::from_bytes(bytes)
        .map_err(|e| format!("image: composed {name} AppInfo does not decode: {e:?}"))?;
    if header.magic != APPINFO_MAGIC {
        return Err(format!(
            "image: composed {name} AppInfo has the wrong magic"
        ));
    }
    if header.bundle_name() != name {
        return Err(format!(
            "image: composed AppInfo names `{}`, expected `{name}`",
            header.bundle_name()
        ));
    }
    if header.signature == [0u8; 64] {
        return Err(format!("image: composed {name} AppInfo is unsigned"));
    }
    Ok(())
}

/// Verify a bundle's declared `library-icon` is one the desktop could
/// actually draw, from the bundle's own `Resources/`.
///
/// `resources` yields `(file_name, bytes)` for every file in the bundle's
/// `Resources/`. A bundle with no declared icon passes trivially. A declared
/// icon must be present in `Resources/` and then satisfy the shared master
/// contract ([`verify_icon_master`]). Without this check a bundle that ships
/// a missing, over-large, or undecodable icon would render as a fallback
/// glyph forever with nothing telling the author; failing the build closed
/// here turns that silent failure into an actionable one.
///
/// # Errors
///
/// Returns an actionable build-error message when a declared icon is absent
/// from `Resources/` or is not artwork the desktop would draw.
fn verify_library_icon<'a>(
    bundle_dir: &str,
    library_icon: Option<&str>,
    resources: impl IntoIterator<Item = (&'a str, &'a [u8])>,
) -> Result<(), String> {
    let Some(icon) = library_icon else {
        return Ok(());
    };
    let bytes = resources
        .into_iter()
        .find_map(|(file, bytes)| (file == icon).then_some(bytes))
        .ok_or_else(|| {
            format!(
                "image: {bundle_dir} declares library-icon `{icon}`, \
                 but no Resources/{icon} is present in the bundle"
            )
        })?;
    verify_icon_master(
        &format!("{bundle_dir} library-icon Resources/{icon}"),
        bytes,
    )
}

/// Verify one icon master is artwork the desktop will actually draw.
///
/// `label` names the artefact in the build error. An icon master is either
/// **vector** artwork (SVG — the preferred form, since it is resolution
/// independent) or a high-resolution **raster** master (PNG). Which one it is
/// is decided from the bytes exactly as the sandboxed rasteriser decides it at
/// runtime — a PNG signature, else the supported SVG subset — never from the
/// file name, so the build accepts precisely what the desktop would draw.
/// Either form must be at most [`tairix_icon::MAX_ARTWORK_BYTES`], the same
/// bound the desktop refuses artwork against *before* it decodes it.
///
/// Checking the decode here rather than trusting the file extension is what
/// makes "the icon is broken" a build failure instead of a silent glyph on
/// someone's desktop.
///
/// # Errors
///
/// Returns an actionable build-error message naming `label` and what is wrong
/// with the artwork.
fn verify_icon_master(label: &str, bytes: &[u8]) -> Result<(), String> {
    if bytes.len() > tairix_icon::MAX_ARTWORK_BYTES {
        return Err(format!(
            "image: {label} is {} bytes, exceeding the {}-byte desktop artwork \
             bound; the desktop would refuse it before decoding and draw a \
             fallback glyph",
            bytes.len(),
            tairix_icon::MAX_ARTWORK_BYTES
        ));
    }
    if tairix_image::sniff(bytes) == Some(ImageFormat::Png) {
        verify_raster_master(label, bytes)
    } else {
        verify_vector_master(label, bytes)
    }
}

/// Verify a raster icon master: it must decode through [`tairix_image`] under
/// the same limits the sandboxed rasteriser applies, be square (every slot
/// draws icons in a square), be at least [`tairix_icon::MIN_ARTWORK_SIDE`] on
/// a side so a slot only ever downscales a master rather than blurring it up,
/// and carry at least one pixel that is not fully transparent.
///
/// # Errors
///
/// Returns an actionable build-error message naming `label` and the geometry
/// or decode problem.
fn verify_raster_master(label: &str, bytes: &[u8]) -> Result<(), String> {
    // A raster icon master is always PNG (`plans/ICONS.md`), never
    // progressive JPEG, so the progressive-coefficient-store bound is
    // never consulted here.
    let limits = DecodeLimits::new(
        tairix_icon::MAX_ARTWORK_SIDE,
        tairix_icon::MAX_ARTWORK_SIDE,
        u64::from(tairix_icon::MAX_ARTWORK_SIDE) * u64::from(tairix_icon::MAX_ARTWORK_SIDE),
        0,
    );
    let image = tairix_image::decode(bytes, &limits)
        .map_err(|e| format!("image: {label} is not artwork the desktop can decode: {e:?}"))?;
    let (width, height) = (image.width(), image.height());
    if width != height {
        return Err(format!(
            "image: {label} is {width}x{height}; an icon master must be square"
        ));
    }
    if width < tairix_icon::MIN_ARTWORK_SIDE {
        return Err(format!(
            "image: {label} is {width}x{height}, smaller than the {}-pixel icon \
             master side; a slot would have to blur it up",
            tairix_icon::MIN_ARTWORK_SIDE
        ));
    }
    if image
        .pixels()
        .as_chunks::<4>()
        .0
        .iter()
        .all(|px| px[3] == 0)
    {
        return Err(format!(
            "image: {label} decodes but every pixel is fully transparent; an \
             icon master must draw something"
        ));
    }
    Ok(())
}

/// Verify a vector icon master: it must decode through the desktop's own SVG
/// subset ([`tairix_icon::decode_svg`]) and actually draw something.
///
/// A vector master carries no pixel geometry to check: it is resolution
/// independent, so the raster rules above do not apply to it. What is left to
/// prove is that the document decodes, that it was authored on a square
/// design box, and that it paints visible area. The decoder letter-boxes a
/// rectangular drawing into the square slot rather than squashing it, which
/// is right for artwork in general but wrong for an icon master: it would
/// ship a picture with bars down two sides. An empty or wholly transparent
/// document likewise decodes happily and would ship as an invisible icon.
///
/// # Errors
///
/// Returns an actionable build-error message naming `label` and why the bytes
/// are not artwork the desktop can draw.
fn verify_vector_master(label: &str, bytes: &[u8]) -> Result<(), String> {
    let image = tairix_svg::decode(bytes).map_err(|e| {
        format!("image: {label} is neither a PNG nor an SVG the desktop can decode: {e:?}")
    })?;
    let (width, height) = image.source_extent();
    if (width - height).abs() > 1e-9 {
        return Err(format!(
            "image: {label} is authored on a {width}x{height} design box; an icon \
             master must be square"
        ));
    }
    let icon = tairix_icon::VectorIcon::from_svg(&image);
    let side = tairix_icon::MIN_ARTWORK_SIDE;
    let surface = icon.rasterise(side).ok_or_else(|| {
        format!("image: {label} decodes but cannot be rasterised at {side} pixels")
    })?;
    if surface.pixels().iter().all(|pixel| pixel.a == 0) {
        return Err(format!(
            "image: {label} decodes but draws nothing; an icon master must draw \
             something"
        ));
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::{verify_icon_master, verify_library_icon};

    /// A minimal but wholly valid `width`×`height` 8-bit greyscale PNG,
    /// opaque white, or grey+alpha at `alpha` when one is given.
    ///
    /// Built here rather than committed as a fixture so a test can ask for
    /// exactly the geometry — or the transparency — it wants to be refused.
    /// The `IDAT` zlib stream is a run of stored (uncompressed) DEFLATE
    /// blocks, which is legal and keeps this to arithmetic the reader can
    /// check by eye; greyscale keeps a master-sized image comfortably inside
    /// the artwork byte bound without needing a compressor here.
    fn png(width: u32, height: u32, alpha: Option<u8>) -> Vec<u8> {
        fn crc32(bytes: &[u8]) -> u32 {
            let mut crc = 0xFFFF_FFFFu32;
            for byte in bytes {
                crc ^= u32::from(*byte);
                for _ in 0..8 {
                    let mask = 0u32.wrapping_sub(crc & 1);
                    crc = (crc >> 1) ^ (0xEDB8_8320 & mask);
                }
            }
            !crc
        }
        fn chunk(out: &mut Vec<u8>, tag: [u8; 4], body: &[u8]) {
            out.extend_from_slice(&u32::try_from(body.len()).expect("chunk fits").to_be_bytes());
            let mut crc_over = tag.to_vec();
            crc_over.extend_from_slice(body);
            out.extend_from_slice(&tag);
            out.extend_from_slice(body);
            out.extend_from_slice(&crc32(&crc_over).to_be_bytes());
        }

        // One white sample per pixel, followed by its alpha sample when the
        // caller asked for an alpha channel; a row is the filter byte plus
        // `width` of those samples, and the raw image is `height` such rows.
        let sample: &[u8] = match alpha {
            Some(alpha) => &[0xFF, alpha],
            None => &[0xFF],
        };
        let mut row = vec![0u8];
        row.extend_from_slice(&sample.repeat(width as usize));
        let raw = row.repeat(height as usize);

        // RFC 1950 envelope over RFC 1951 stored blocks (each at most the
        // 16-bit block length the format allows).
        let mut zlib = vec![0x78, 0x01];
        let mut rest = raw.as_slice();
        loop {
            let take = rest.len().min(usize::from(u16::MAX));
            let (block, tail) = rest.split_at(take);
            let len = u16::try_from(block.len()).expect("bounded by u16::MAX");
            zlib.push(u8::from(tail.is_empty()));
            zlib.extend_from_slice(&len.to_le_bytes());
            zlib.extend_from_slice(&(!len).to_le_bytes());
            zlib.extend_from_slice(block);
            rest = tail;
            if rest.is_empty() {
                break;
            }
        }
        let (mut a, mut b) = (1u32, 0u32);
        for byte in &raw {
            a = (a + u32::from(*byte)) % 65521;
            b = (b + a) % 65521;
        }
        zlib.extend_from_slice(&((b << 16) | a).to_be_bytes());

        // Colour type 4 (grey+alpha) when an alpha channel was asked for,
        // else 0 (grey).
        let colour_type = if alpha.is_some() { 4 } else { 0 };
        let mut ihdr = Vec::new();
        ihdr.extend_from_slice(&width.to_be_bytes());
        ihdr.extend_from_slice(&height.to_be_bytes());
        ihdr.extend_from_slice(&[8, colour_type, 0, 0, 0]);

        let mut out = vec![0x89, b'P', b'N', b'G', 0x0D, 0x0A, 0x1A, 0x0A];
        chunk(&mut out, *b"IHDR", &ihdr);
        chunk(&mut out, *b"IDAT", &zlib);
        chunk(&mut out, *b"IEND", &[]);
        out
    }

    /// A bundle that declares no library icon has nothing to verify.
    #[test]
    fn no_declared_icon_passes() {
        assert!(verify_library_icon("terminal.app", None, core::iter::empty()).is_ok());
    }

    /// A present icon that decodes as a square master at the shipped side is
    /// accepted.
    #[test]
    fn a_square_master_at_the_shipped_side_passes() {
        let master = png(
            tairix_icon::MIN_ARTWORK_SIDE,
            tairix_icon::MIN_ARTWORK_SIDE,
            None,
        );
        let resources: [(&str, &[u8]); 2] = [("other.bin", &[0u8; 4]), ("terminal.png", &master)];
        assert!(verify_library_icon("terminal.app", Some("terminal.png"), resources).is_ok());
    }

    /// A vector master is accepted, and needs no pixel side: SVG is
    /// resolution independent, so the raster master's geometry rules do not
    /// apply to it. This is the preferred form for a new app's icon.
    #[test]
    fn a_square_vector_master_passes() {
        let master: &[u8] =
            br##"<svg viewBox="0 0 32 32"><polygon points="2,2 30,2 30,30 2,30" fill="#3070f0"/></svg>"##;
        let resources: [(&str, &[u8]); 1] = [("files.svg", master)];
        assert!(verify_library_icon("files.app", Some("files.svg"), resources).is_ok());
    }

    /// A vector master whose design box is not square is refused: the decoder
    /// would letter-box it into every square slot, shipping a picture with
    /// bars down two sides rather than an icon.
    #[test]
    fn a_non_square_vector_master_is_refused() {
        let master: &[u8] =
            br##"<svg viewBox="0 0 32 16"><polygon points="0,0 32,0 32,16 0,16" fill="#3070f0"/></svg>"##;
        let err = verify_icon_master("x.app icon", master)
            .expect_err("a rectangular vector master must be refused");
        assert!(err.contains("must be square"), "{err}");
    }

    /// A vector master that decodes but paints nothing is refused: it would
    /// ship as an invisible icon rather than a picture, which is exactly the
    /// silent failure this check exists to turn into a build error.
    #[test]
    fn a_vector_master_that_draws_nothing_is_refused() {
        let err = verify_icon_master("x.app icon", br#"<svg viewBox="0 0 32 32"></svg>"#)
            .expect_err("a vector master that draws nothing must be refused");
        assert!(err.contains("draws nothing"), "{err}");
    }

    /// A raster master that decodes but is wholly transparent is refused for
    /// the same reason: the slot would draw an empty square.
    #[test]
    fn a_fully_transparent_raster_master_is_refused() {
        let side = tairix_icon::MIN_ARTWORK_SIDE;
        let err = verify_icon_master("x.app icon", &png(side, side, Some(0)))
            .expect_err("a fully transparent master must be refused");
        assert!(err.contains("transparent"), "{err}");
    }

    /// A declared icon that is not present in `Resources/` is refused, and
    /// the message names the bundle and the missing file.
    #[test]
    fn missing_icon_is_refused() {
        let resources: [(&str, &[u8]); 1] = [("other.bin", &[0u8; 4])];
        let err = verify_library_icon("terminal.app", Some("terminal.png"), resources)
            .expect_err("a declared icon absent from Resources/ must be refused");
        assert!(err.contains("terminal.app"), "{err}");
        assert!(err.contains("terminal.png"), "{err}");
    }

    /// An icon one byte over the artwork bound is refused *before* it is
    /// decoded, and the message names the file, its size, and the bound.
    #[test]
    fn over_bound_icon_is_refused() {
        let size = tairix_icon::MAX_ARTWORK_BYTES + 1;
        let over = vec![0u8; size];
        let resources: [(&str, &[u8]); 1] = [("terminal.png", &over)];
        let err = verify_library_icon("terminal.app", Some("terminal.png"), resources)
            .expect_err("an over-large icon must be refused");
        assert!(err.contains("terminal.png"), "{err}");
        assert!(err.contains(&size.to_string()), "{err}");
        assert!(
            err.contains(&tairix_icon::MAX_ARTWORK_BYTES.to_string()),
            "{err}"
        );
    }

    /// A file that is not decodable artwork at all is refused: the check is
    /// the desktop's own decoder, not the file's name.
    #[test]
    fn a_file_that_is_not_an_image_is_refused() {
        let err = verify_icon_master("x.app icon", b"PNG? no.")
            .expect_err("undecodable bytes must be refused");
        assert!(err.contains("decode"), "{err}");
    }

    /// A non-square master is refused: every slot draws an icon in a square,
    /// so a rectangular master would be letterboxed forever.
    #[test]
    fn a_non_square_master_is_refused() {
        let side = tairix_icon::MIN_ARTWORK_SIDE;
        let err = verify_icon_master("x.app icon", &png(side, side / 2, None))
            .expect_err("a rectangular master must be refused");
        assert!(err.contains("square"), "{err}");
    }

    /// A master below the shipped side is refused: a slot may only ever
    /// downscale artwork, never blur it up.
    #[test]
    fn an_undersized_master_is_refused() {
        let small = tairix_icon::MIN_ARTWORK_SIDE / 2;
        let err = verify_icon_master("x.app icon", &png(small, small, None))
            .expect_err("an undersized master must be refused");
        assert!(
            err.contains(&tairix_icon::MIN_ARTWORK_SIDE.to_string()),
            "{err}"
        );
    }

    /// Every icon master the image ships — the desktop's class artwork and
    /// every bundle's own icon alike — is artwork the desktop will really
    /// draw. Discovered from disk, so a new asset is judged the moment it is
    /// dropped in.
    ///
    /// Only the icon-family rows of [`tairix_syshelp::GRAPHICS_FILES`] are
    /// judged here: the icon and wallpaper families are different pictures
    /// under different contracts (see
    /// [`every_shipped_wallpaper_master_is_artwork_the_desktop_will_draw`]),
    /// and dispatching on [`tairix_syshelp::GraphicsFamilyKind`] rather than
    /// a directory string is what keeps the two from being conflated.
    #[test]
    fn every_shipped_icon_master_is_artwork_the_desktop_will_draw() {
        let icon_masters: Vec<_> = tairix_syshelp::GRAPHICS_FILES
            .iter()
            .filter(|asset| asset.family == tairix_syshelp::GraphicsFamilyKind::Icon)
            .collect();
        assert!(!icon_masters.is_empty(), "at least one icon master ships");
        // Decided from the bytes, exactly as the verification decides it.
        assert!(
            icon_masters.iter().any(|asset| {
                tairix_image::sniff(asset.bytes) != Some(tairix_image::ImageFormat::Png)
            }),
            "a vector class master ships, so the vector arm is exercised"
        );
        for asset in icon_masters {
            verify_icon_master(
                &format!("Graphics/{}/{}", asset.family.target_dir(), asset.file),
                asset.bytes,
            )
            .expect("a shipped icon master");
        }

        let userland = std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
            .parent()
            .and_then(std::path::Path::parent)
            .expect("the crate lives at <workspace>/tools/xtask")
            .join("userland");
        let discovered = super::discover_app_manifests(&userland).expect("discovery");
        let mut bundle_icons = 0usize;
        for app in &discovered {
            let bundle_dir = app.manifest.bundle_dir();
            if app.manifest.library_icon.is_some() {
                bundle_icons += 1;
            }
            verify_library_icon(
                &bundle_dir,
                app.manifest.library_icon.as_deref(),
                tairix_syshelp::RESOURCE_FILES
                    .iter()
                    .filter(|res| res.bundle == bundle_dir)
                    .map(|res| (res.file, res.bytes)),
            )
            .expect("a bundle's own icon");
        }
        assert!(
            bundle_icons > 0,
            "the shipped bundles declare their own icons"
        );
    }

    /// Every wallpaper master the image ships is a photograph the desktop's
    /// own decoder can actually turn into a picture. Discovered from disk
    /// exactly as the icon masters above are (only the wallpaper-family rows
    /// of [`tairix_syshelp::GRAPHICS_FILES`]), so a new wallpaper is judged
    /// the moment it is dropped into `lib/wallpaper/assets/` — never a
    /// hand-maintained list.
    ///
    /// A wallpaper master is not held to the icon contract above: it need
    /// not be square, has no minimum side, and a fully opaque photograph is
    /// exactly what is expected rather than something the icon check would
    /// refuse for having no transparency. [`verify_wallpaper_master`] is its
    /// own named check for exactly that reason.
    #[test]
    fn every_shipped_wallpaper_master_is_artwork_the_desktop_will_draw() {
        let wallpaper_masters: Vec<_> = tairix_syshelp::GRAPHICS_FILES
            .iter()
            .filter(|asset| asset.family == tairix_syshelp::GraphicsFamilyKind::Wallpaper)
            .collect();
        assert!(
            !wallpaper_masters.is_empty(),
            "at least one wallpaper master ships"
        );
        for asset in wallpaper_masters {
            verify_wallpaper_master(
                &format!("Graphics/{}/{}", asset.family.target_dir(), asset.file),
                asset.bytes,
            )
            .expect("a shipped wallpaper master");
        }
    }

    /// Verify a wallpaper master is a photograph the desktop will actually
    /// draw: within [`tairix_wallpaper::MAX_WALLPAPER_BYTES`], a format
    /// [`tairix_image::sniff`] recognises, and one that format's decoder can
    /// actually turn into a picture with real dimensions.
    ///
    /// This is the wallpaper family's own contract, separate from
    /// [`verify_icon_master`] rather than a mode flag on it: a wallpaper is
    /// a full-bleed photograph, so it carries none of an icon master's shape
    /// rules (square, a minimum side, at least one opaque pixel).
    ///
    /// The shipped masters run to several megapixels each (six thousand
    /// pixels or more on a side), so this decodes through
    /// [`tairix_image::decode_fitted`] at a tiny destination box rather than
    /// at natural size. For the shipped JPEG masters that makes the decoder
    /// pick its coarsest reduced scale — an eighth of natural size — which
    /// keeps this check fast and light over every shipped master without
    /// ever exercising the full-resolution decode a real screen would need.
    /// PNG has no reduced-scale decode process, so a PNG wallpaper would
    /// still decode at natural size here; every master shipped today is
    /// JPEG.
    ///
    /// # Errors
    ///
    /// Returns an actionable build-error message naming `label` and what is
    /// wrong with the artwork.
    fn verify_wallpaper_master(label: &str, bytes: &[u8]) -> Result<(), String> {
        // The JPEG format's own absolute frame-dimension ceiling (ITU-T
        // T.81 B.2.2), not a guessed "reasonable wallpaper size" — so this
        // check can never refuse a master the desktop's own decoder would
        // happily draw. The progressive-coefficient budget is generous for
        // the same reason: unlike the output size, a progressive JPEG's
        // coefficient store scales with its *natural* size regardless of
        // the small fit box requested below.
        const JPEG_FRAME_DIMENSION_LIMIT: u32 = 0xFFFF;
        const COEFFICIENT_BUDGET: u64 = 256 * 1024 * 1024;
        // A tiny fit box: for a reduced-scale format this forces the
        // coarsest available scale, which is what keeps this check fast
        // over a multi-megapixel master.
        const FIT_SIDE: u32 = 64;

        if bytes.len() > tairix_wallpaper::MAX_WALLPAPER_BYTES {
            return Err(format!(
                "image: {label} is {} bytes, exceeding the {}-byte desktop wallpaper \
                 bound; the desktop would refuse it before decoding",
                bytes.len(),
                tairix_wallpaper::MAX_WALLPAPER_BYTES
            ));
        }
        if tairix_image::sniff(bytes).is_none() {
            return Err(format!(
                "image: {label} is not a format the desktop's image decoder recognises"
            ));
        }
        let limits = tairix_image::DecodeLimits::new(
            JPEG_FRAME_DIMENSION_LIMIT,
            JPEG_FRAME_DIMENSION_LIMIT,
            u64::from(JPEG_FRAME_DIMENSION_LIMIT) * u64::from(JPEG_FRAME_DIMENSION_LIMIT),
            COEFFICIENT_BUDGET,
        );
        let fit = tairix_image::FitBox::new(FIT_SIDE, FIT_SIDE);
        let image = tairix_image::decode_fitted(bytes, &limits, fit)
            .map_err(|e| format!("image: {label} is not artwork the desktop can decode: {e:?}"))?;
        if image.width() == 0 || image.height() == 0 {
            return Err(format!(
                "image: {label} decodes to a degenerate {}x{} image",
                image.width(),
                image.height()
            ));
        }
        Ok(())
    }
}
