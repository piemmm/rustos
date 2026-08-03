//! Cross-compile, compose, and sign the self-contained application bundles
//! the images ship on the read-only `/System` volume (`plans/APPS.md`
//! deliverable 8): every command app under `/System/Apps/<name>.app/` and
//! every service under `/System/Services/<name>.app/`, each a complete
//! on-disk bundle — its signed `AppInfo` and `Run` rxe planted beside its
//! `Help/` tree.
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
use tairix_itest_harness::app_image::{
    compose_signed_appinfo, discover_app_manifests, discover_crate_manifest, DiscoveredApp,
    APP_MANIFEST_SOURCE,
};
use tairix_itest_harness::elf2rxe::elf_to_rxe;
use tairix_itest_harness::pie::PieArch;
use tairix_itest_harness::USER_IMAGE_BIAS;
use tairix_mkimage::ImageProfile;

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
    /// The `/System`-volume-relative store directory (`Apps` or `Services`).
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
            // The four `/System/Fonts` faces the `fontd` service loads at
            // startup ship on every image alongside the bundles.
            files.extend(system_font_files(ctx)?);
            Ok(files)
        })
        .as_ref()
        .map(Vec::as_slice)
        .map_err(Clone::clone)
}

/// The `/System/Fonts` faces the image plants: every committed TrueType face
/// under `lib/font/assets/`, laid down at `Fonts/<basename>` so the sandboxed
/// `fontd` font service can load them at startup (`AGENTS.md` §16.2/§16.4,
/// `plans/FONT-SERVICE.md` FS-6). Discovered from the on-disk assets rather
/// than a hand-maintained list, so a face ships without editing this file
/// (§2.2); `fontd` opens exactly these paths.
///
/// # Errors
///
/// A string describing a failed read of the font-assets directory or a face.
fn system_font_files(ctx: &Context) -> Result<Vec<AppStoreFile>, String> {
    let dir = ctx.workspace_root.join("lib/font/assets");
    let mut faces = Vec::new();
    let entries = std::fs::read_dir(&dir)
        .map_err(|e| format!("image: reading font assets {}: {e}", dir.display()))?;
    for entry in entries {
        let path = entry
            .map_err(|e| format!("image: font asset entry in {}: {e}", dir.display()))?
            .path();
        if path.extension().and_then(|e| e.to_str()) != Some("ttf") {
            continue;
        }
        let name = path
            .file_name()
            .and_then(|n| n.to_str())
            .ok_or_else(|| format!("image: non-UTF-8 font asset name in {}", dir.display()))?
            .to_owned();
        let bytes = std::fs::read(&path)
            .map_err(|e| format!("image: font face {}: {e}", path.display()))?;
        faces.push(AppStoreFile {
            components: vec![b"Fonts".to_vec(), name.into_bytes()],
            bytes,
        });
    }
    // `read_dir` order is unspecified; sort so the planted set is deterministic.
    faces.sort_by(|a, b| a.components.cmp(&b.components));
    Ok(faces)
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
            .map(|res| (res.file, res.bytes.len())),
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
/// `resources` yields `(file_name, byte_len)` for every file in the bundle's
/// `Resources/`. A bundle with no declared icon passes trivially. A declared
/// icon must (a) be present in `Resources/` and (b) be at most
/// [`tairix_icon::MAX_ARTWORK_BYTES`] — the same bound the desktop refuses
/// artwork against *before* it decodes it. Without this check a bundle that
/// ships a missing or over-large icon would render as a fallback glyph
/// forever with nothing telling the author; failing the build closed here,
/// with a message naming the bundle, the file, its size, and the bound, turns
/// that silent failure into an actionable one.
///
/// # Errors
///
/// Returns an actionable build-error message when a declared icon is absent
/// from `Resources/` or exceeds the artwork byte bound.
fn verify_library_icon<'a>(
    bundle_dir: &str,
    library_icon: Option<&str>,
    resources: impl IntoIterator<Item = (&'a str, usize)>,
) -> Result<(), String> {
    let Some(icon) = library_icon else {
        return Ok(());
    };
    let size = resources
        .into_iter()
        .find_map(|(file, len)| (file == icon).then_some(len))
        .ok_or_else(|| {
            format!(
                "image: {bundle_dir} declares library-icon `{icon}`, \
                 but no Resources/{icon} is present in the bundle"
            )
        })?;
    if size > tairix_icon::MAX_ARTWORK_BYTES {
        return Err(format!(
            "image: {bundle_dir} library-icon Resources/{icon} is {size} bytes, \
             exceeding the {}-byte desktop artwork bound; the desktop would \
             refuse it before decoding and draw a fallback glyph",
            tairix_icon::MAX_ARTWORK_BYTES
        ));
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::verify_library_icon;

    /// A bundle that declares no library icon has nothing to verify.
    #[test]
    fn no_declared_icon_passes() {
        assert!(verify_library_icon("terminal.app", None, core::iter::empty()).is_ok());
    }

    /// A present icon within the artwork bound is accepted, and the exact
    /// bound value (not one over it) is accepted.
    #[test]
    fn present_and_within_bound_passes() {
        let resources = [
            ("other.bin", 10),
            ("terminal.png", tairix_icon::MAX_ARTWORK_BYTES),
        ];
        assert!(verify_library_icon("terminal.app", Some("terminal.png"), resources).is_ok());
    }

    /// A declared icon that is not present in `Resources/` is refused, and
    /// the message names the bundle and the missing file.
    #[test]
    fn missing_icon_is_refused() {
        let err = verify_library_icon("terminal.app", Some("terminal.png"), [("other.bin", 10)])
            .expect_err("a declared icon absent from Resources/ must be refused");
        assert!(err.contains("terminal.app"), "{err}");
        assert!(err.contains("terminal.png"), "{err}");
    }

    /// An icon one byte over the artwork bound is refused, and the message
    /// names the file, its size, and the bound.
    #[test]
    fn over_bound_icon_is_refused() {
        let size = tairix_icon::MAX_ARTWORK_BYTES + 1;
        let err = verify_library_icon(
            "terminal.app",
            Some("terminal.png"),
            [("terminal.png", size)],
        )
        .expect_err("an over-large icon must be refused");
        assert!(err.contains("terminal.png"), "{err}");
        assert!(err.contains(&size.to_string()), "{err}");
        assert!(
            err.contains(&tairix_icon::MAX_ARTWORK_BYTES.to_string()),
            "{err}"
        );
    }
}
