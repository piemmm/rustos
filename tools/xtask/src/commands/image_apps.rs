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

use super::image_drivers::build_support;
use super::pie_build::cross_compile_pie_elf;
use crate::Context;

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
pub fn build_app_bundles(ctx: &Context, arch: PieArch) -> Result<Vec<BuiltAppBundle>, String> {
    let userland = ctx.workspace_root.join("userland");
    let discovered = discover_app_manifests(&userland)
        .map_err(|e| format!("image: app-manifest discovery: {e}"))?;
    discovered
        .iter()
        .map(|app| build_bundle(ctx, arch, app))
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
pub fn app_store_files(ctx: &Context, arch: PieArch) -> Result<&'static [AppStoreFile], String> {
    static FILES: [OnceLock<Result<Vec<AppStoreFile>, String>>; PieArch::COUNT] =
        [const { OnceLock::new() }; PieArch::COUNT];
    FILES[arch.index()]
        .get_or_init(|| build_app_bundles(ctx, arch).map(|bundles| store_files(&bundles)))
        .as_ref()
        .map(Vec::as_slice)
        .map_err(Clone::clone)
}

/// Crate directory of the test-only `memsoak` memory-stability fixture
/// (`plans/APPS.md` "Immediate work" I2/I3), relative to the workspace
/// root. It lives outside the userland discovery walk deliberately: the
/// fixture is planted only on the memory-stability vertical's disk, never
/// on a production image.
const MEMSOAK_CRATE_DIR: &str = "tests/integration/memsoak_program";

/// The composed store files the memory-stability vertical's disk plants:
/// the shared [`app_store_files`] set **plus** the test-only `memsoak`
/// fixture bundle, composed through the same discovery/compose/sign path
/// as every store bundle and memoised like the shared set.
///
/// # Errors
///
/// As [`build_app_bundles`], plus a failed fixture-manifest discovery.
pub fn memsoak_store_files(
    ctx: &Context,
    arch: PieArch,
) -> Result<&'static [AppStoreFile], String> {
    static FILES: [OnceLock<Result<Vec<AppStoreFile>, String>>; PieArch::COUNT] =
        [const { OnceLock::new() }; PieArch::COUNT];
    FILES[arch.index()]
        .get_or_init(|| {
            let base = app_store_files(ctx, arch)?;
            let crate_dir = ctx.workspace_root.join(MEMSOAK_CRATE_DIR);
            let app = discover_crate_manifest(&crate_dir)
                .map_err(|e| format!("image: memsoak manifest discovery: {e}"))?
                .ok_or_else(|| {
                    format!(
                        "image: {} has no {APP_MANIFEST_SOURCE}",
                        crate_dir.display()
                    )
                })?;
            let bundle = build_bundle(ctx, arch, &app)?;
            let mut files = base.to_vec();
            files.extend(store_files(&[bundle]));
            Ok(files)
        })
        .as_ref()
        .map(Vec::as_slice)
        .map_err(Clone::clone)
}

/// Crate directory of the test-only `tcpecho` stream-socket fixture
/// (`plans/NETWORK.md` N5c), relative to the workspace root. Like the
/// `memsoak` fixture it lives outside the userland discovery walk: it is
/// planted only on the stream vertical's disk, never on a production image.
const TCPECHO_CRATE_DIR: &str = "tests/integration/tcpecho_program";

/// The composed store files the stream vertical's disk plants in `/System`:
/// the shared [`app_store_files`] set **plus** the test-only `tcpecho`
/// fixture bundle, composed through the same discovery/compose/sign path as
/// every store bundle and memoised like the shared set.
///
/// # Errors
///
/// As [`build_app_bundles`], plus a failed fixture-manifest discovery.
pub fn tcpecho_store_files(
    ctx: &Context,
    arch: PieArch,
) -> Result<&'static [AppStoreFile], String> {
    static FILES: [OnceLock<Result<Vec<AppStoreFile>, String>>; PieArch::COUNT] =
        [const { OnceLock::new() }; PieArch::COUNT];
    FILES[arch.index()]
        .get_or_init(|| {
            let base = app_store_files(ctx, arch)?;
            let crate_dir = ctx.workspace_root.join(TCPECHO_CRATE_DIR);
            let app = discover_crate_manifest(&crate_dir)
                .map_err(|e| format!("image: tcpecho manifest discovery: {e}"))?
                .ok_or_else(|| {
                    format!(
                        "image: {} has no {APP_MANIFEST_SOURCE}",
                        crate_dir.display()
                    )
                })?;
            let bundle = build_bundle(ctx, arch, &app)?;
            let mut files = base.to_vec();
            files.extend(store_files(&[bundle]));
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
) -> Result<BuiltAppBundle, String> {
    // Every program crate's `Run` binary is named `<package>-run`
    // (the kernel `build.rs` builds the same bins); the artefact is read
    // back fail-closed, so a crate that breaks the convention fails the
    // build loudly.
    let bin = format!("{}-run", app.package);
    let elf = cross_compile_pie_elf(ctx, arch, "image-apps", &app.package, &bin, &app.crate_dir)?;
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
