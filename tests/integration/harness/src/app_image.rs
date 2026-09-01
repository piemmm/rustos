//! Build-time signed application-bundle composer and `AppInfo.toml`
//! manifest discovery (`plans/APPS.md` deliverable 8).
//!
//! Every runnable program — a command app or a service (a service is an
//! app) — ships as a self-contained signed bundle on the read-only
//! `/System` store. Each program crate authors its manifest **source** as
//! an `AppInfo.toml` beside its `Cargo.toml`; the image builds discover
//! those files by walking the userland crate roots
//! ([`crate::app_image::discover_app_manifests`]) — never a hand-maintained
//! per-bundle list — and compose each bundle's signed wire `AppInfo`
//! ([`crate::app_image::compose_signed_appinfo`]): the
//! [`tairix_abi::AppInfoHeader`] plus its
//! capability body, content-hashed over the canonical
//! [`tairix_abi::digest_bundle_contents`] framing and Ed25519-signed over
//! the header prefix concatenated with the body — exactly the message the
//! bundle verifier reconstructs.
//!
//! The `kind` a manifest declares ([`tairix_abi::ProgramKind`]) is what
//! decides the store: a command app lands in `/System/Commands`, a graphical
//! application in `/System/Applications`, and a service in
//! `/System/Services`. There is no list anywhere of which programs are which
//! — every bundle declares itself, and the walk refuses two bundles that
//! claim the same name, so one store can never shadow a name in another.
//!
//! The manifest-source grammar is a deliberately tiny, line-based TOML
//! subset, parsed fail-closed: `#` comments; the required keys `id`,
//! `name`, `version`, `kind`, and `capabilities` (a single-line array of
//! canonical `CAP_*` names); and the optional keys `associations` (the
//! declared file-type hints), `library` (the program-library folder a
//! graphical application lists itself under — absence means the library
//! never shows the bundle), `library-icon` (an icon asset inside the
//! bundle's `Resources/`), `purpose`, `author`, and `icon-bar` (a bare
//! `true`/`false`; `false` for a bundle the desktop's icon bar gives no slot
//! of its own, because it already reaches it another way). Each key appears
//! at most once. Anything else — an unknown key, a duplicate, a multi-line
//! value, an unknown capability or folder name — is a packaging defect
//! that fails the build, never a guessed default.

use std::fmt;
use std::path::{Path, PathBuf};

use ed25519_dalek::{Signer, SigningKey};
use tairix_abi::{
    digest_bundle_contents, AppInfoHeader, BundleFileDigest, CapabilityId, LibraryCategory,
    ProgramKind, ABI_VERSION_CURRENT, APPINFO_FLAG_NO_ICON_BAR, APPINFO_MAGIC,
    APPINFO_MAX_CAPABILITIES, APPINFO_MAX_MIME, BUNDLE_AUTHOR_MAX, BUNDLE_ID_MAX, BUNDLE_NAME_MAX,
    BUNDLE_PURPOSE_MAX, BUNDLE_SUFFIX, BUNDLE_VERSION_MAX, LIBRARY_ICON_MAX, MIME_ENTRY_LEN,
    MIME_TYPE_MAX,
};
use tairix_crypto::sha256;

/// File name of a program crate's manifest source, beside its `Cargo.toml`.
pub const APP_MANIFEST_SOURCE: &str = "AppInfo.toml";

/// A failed manifest parse, discovery walk, or bundle composition. The
/// message names the offending file and cause; the build fails closed on
/// it rather than planting a guessed bundle.
#[derive(Debug, Eq, PartialEq)]
pub struct AppImageError(String);

impl AppImageError {
    fn new(context: impl fmt::Display, detail: impl fmt::Display) -> Self {
        Self(format!("{context}: {detail}"))
    }
}

impl fmt::Display for AppImageError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.0)
    }
}

impl std::error::Error for AppImageError {}

/// One program's parsed, validated `AppInfo.toml` manifest source.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct AppManifestSource {
    /// Bundle identifier (e.g. `os.tairix.ls`).
    pub id: String,
    /// The program's name — also its bundle directory stem and, for a
    /// command app, the command word the shell resolves.
    pub name: String,
    /// Bundle version string.
    pub version: String,
    /// Which store the bundle is planted in.
    pub kind: ProgramKind,
    /// The bundle's requested capabilities, in manifest order.
    pub capabilities: Vec<CapabilityId>,
    /// The file-type (MIME) associations the bundle declares it can open,
    /// in manifest order. Empty when the bundle declares none (the
    /// `associations` key is optional): a program that opens no operand
    /// file — a pure command — carries no associations, exactly as it
    /// carries no `OpenFile` behaviour. These are a display *hint* the
    /// file manager offers "Open With…" candidates from; the signed load
    /// gate still verifies and capability-checks whichever bundle is
    /// launched (`plans/NEW-FILEMANAGER.md` `FM6b`).
    pub associations: Vec<String>,
    /// The program-library folder the bundle lists itself under, or `None`
    /// for a bundle the desktop's Program Library never shows. Listing is
    /// an explicit opt-in — exactly as a desktop entry is elsewhere — so a
    /// plain command tool stays out of the launcher without a marker, and
    /// a graphical application declares its folder in its own manifest
    /// (`plans/NEW-TASKBAR.md` T3).
    pub library: Option<LibraryCategory>,
    /// The bundle's own icon asset — a plain file name inside its own
    /// `Resources/`. Independent of `library`: it is the bundle's identity
    /// wherever it is drawn (a file-manager tile, a taskbar button, a
    /// launcher row), not a launcher-listing detail. `None` means a draw
    /// site falls back to the bundle's class artwork and then to the
    /// built-in glyph (`plans/ICONS.md`).
    pub library_icon: Option<String>,
    /// The bundle's one-line purpose — what the application is *for* — as
    /// the desktop's application-information panel states it. `None` means
    /// the panel simply omits the line.
    pub purpose: Option<String>,
    /// The bundle's author attribution, shown in the same panel. `None`
    /// means the panel omits it.
    pub author: Option<String>,
    /// Whether the desktop's icon bar gives this bundle a slot of its own.
    /// Absent means it does; `icon-bar = false` is for a bundle the desktop
    /// already reaches another way, whose slot would be a duplicate route
    /// (`plans/NEW-TASKBAR.md`).
    pub icon_bar: bool,
}

impl AppManifestSource {
    /// Parse and validate one `AppInfo.toml` text.
    ///
    /// # Errors
    ///
    /// Fails closed on any deviation from the grammar: an unknown or
    /// duplicate key, a missing key, a malformed string or array value, an
    /// over-long or empty identity field, a name that is not a plain
    /// command word, an unknown `kind`, an unknown or duplicate `CAP_*`
    /// name, a capability list exceeding the manifest bound, an unknown
    /// library folder, an over-long `library-icon`, `purpose`, or `author`,
    /// an `icon-bar` that is not a bare `true`/`false`, or a `library` on a
    /// `service`.
    pub fn parse(text: &str) -> Result<Self, AppImageError> {
        let ctx = APP_MANIFEST_SOURCE;
        let mut id = None;
        let mut name = None;
        let mut version = None;
        let mut kind = None;
        let mut capabilities = None;
        let mut associations = None;
        let mut library = None;
        let mut library_icon = None;
        let mut purpose = None;
        let mut author = None;
        let mut icon_bar = None;
        for (index, raw) in text.lines().enumerate() {
            let line = raw.trim();
            if line.is_empty() || line.starts_with('#') {
                continue;
            }
            let at = format!("{ctx} line {}", index + 1);
            let (key, value) = line
                .split_once('=')
                .ok_or_else(|| AppImageError::new(&at, "expected `key = value`"))?;
            let (key, value) = (key.trim(), value.trim());
            match key {
                "id" => set(&at, key, &mut id, parse_string(&at, value)?)?,
                "name" => set(&at, key, &mut name, parse_string(&at, value)?)?,
                "version" => set(&at, key, &mut version, parse_string(&at, value)?)?,
                "kind" => set(&at, key, &mut kind, parse_kind(&at, value)?)?,
                "capabilities" => {
                    set(&at, key, &mut capabilities, parse_capabilities(&at, value)?)?;
                }
                "associations" => {
                    set(&at, key, &mut associations, parse_associations(&at, value)?)?;
                }
                "library" => set(&at, key, &mut library, parse_library(&at, value)?)?,
                "library-icon" => {
                    set(&at, key, &mut library_icon, parse_string(&at, value)?)?;
                }
                "purpose" => set(&at, key, &mut purpose, parse_string(&at, value)?)?,
                "author" => set(&at, key, &mut author, parse_string(&at, value)?)?,
                "icon-bar" => set(&at, key, &mut icon_bar, parse_bool(&at, value)?)?,
                other => {
                    return Err(AppImageError::new(&at, format!("unknown key `{other}`")));
                }
            }
        }
        let manifest = Self {
            id: require(id, "id")?,
            name: require(name, "name")?,
            version: require(version, "version")?,
            kind: require(kind, "kind")?,
            capabilities: require(capabilities, "capabilities")?,
            // `associations` is optional: a bundle that opens no operand
            // file declares none. `library` is optional too — absence means
            // the program library never lists the bundle — as is
            // `library-icon`, whose absence means the bundle is drawn with
            // its class artwork instead of one of its own.
            associations: associations.unwrap_or_default(),
            library,
            library_icon,
            purpose,
            author,
            // A bundle is on the icon bar unless it says otherwise, so the
            // opt-out is a deliberate line in a manifest rather than the
            // default a forgotten key falls into.
            icon_bar: icon_bar.unwrap_or(true),
        };
        manifest.validate()?;
        Ok(manifest)
    }

    /// The bundle directory name: `<name>.app`.
    #[must_use]
    pub fn bundle_dir(&self) -> String {
        format!("{}{BUNDLE_SUFFIX}", self.name)
    }

    /// Cross-field validation applied after a successful parse.
    fn validate(&self) -> Result<(), AppImageError> {
        let ctx = APP_MANIFEST_SOURCE;
        check_len(ctx, "id", &self.id, BUNDLE_ID_MAX)?;
        check_len(ctx, "name", &self.name, BUNDLE_NAME_MAX)?;
        check_len(ctx, "version", &self.version, BUNDLE_VERSION_MAX)?;
        if !self
            .name
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || b == b'-' || b == b'_')
        {
            return Err(AppImageError::new(
                ctx,
                format!("`name` must be a plain command word, got {:?}", self.name),
            ));
        }
        if self.capabilities.len() > usize::from(APPINFO_MAX_CAPABILITIES) {
            return Err(AppImageError::new(ctx, "too many capabilities"));
        }
        if self.associations.len() > usize::from(APPINFO_MAX_MIME) {
            return Err(AppImageError::new(ctx, "too many associations"));
        }
        for mime in &self.associations {
            check_len(ctx, "association", mime, MIME_TYPE_MAX)?;
        }
        if let Some(icon) = &self.library_icon {
            check_len(ctx, "library-icon", icon, LIBRARY_ICON_MAX)?;
        }
        if let Some(purpose) = &self.purpose {
            check_len(ctx, "purpose", purpose, BUNDLE_PURPOSE_MAX)?;
        }
        if let Some(author) = &self.author {
            check_len(ctx, "author", author, BUNDLE_AUTHOR_MAX)?;
        }
        if self.library.is_some() && self.kind == ProgramKind::Service {
            // A service is a daemon, not a user-facing application; listing
            // one in the launcher would offer a user a bundle that opens no
            // surface.
            return Err(AppImageError::new(
                ctx,
                "a `service` cannot declare `library`",
            ));
        }
        Ok(())
    }
}

/// Unwrap a required manifest slot, refusing a missing key.
fn require<T>(slot: Option<T>, key: &str) -> Result<T, AppImageError> {
    slot.ok_or_else(|| AppImageError::new(APP_MANIFEST_SOURCE, format!("missing key `{key}`")))
}

/// Store a parsed value into its manifest slot, refusing a duplicate key.
fn set<T>(at: &str, key: &str, slot: &mut Option<T>, parsed: T) -> Result<(), AppImageError> {
    if slot.is_some() {
        return Err(AppImageError::new(at, format!("duplicate key `{key}`")));
    }
    *slot = Some(parsed);
    Ok(())
}

/// Reject an empty or over-long identity field.
fn check_len(ctx: &str, key: &str, value: &str, max: usize) -> Result<(), AppImageError> {
    if value.is_empty() || value.len() > max {
        return Err(AppImageError::new(
            ctx,
            format!("`{key}` must be 1..={max} bytes, got {}", value.len()),
        ));
    }
    Ok(())
}

/// Parse a double-quoted string with no embedded quote or escape.
fn parse_string(at: &str, value: &str) -> Result<String, AppImageError> {
    let inner = value
        .strip_prefix('"')
        .and_then(|rest| rest.strip_suffix('"'))
        .ok_or_else(|| AppImageError::new(at, "expected a double-quoted string"))?;
    if inner.contains(['"', '\\']) {
        return Err(AppImageError::new(at, "quotes/escapes are not supported"));
    }
    Ok(inner.to_string())
}

/// Parse a bare `true`/`false`. Anything else is a packaging defect rather
/// than a value to coerce.
fn parse_bool(at: &str, value: &str) -> Result<bool, AppImageError> {
    match value {
        "true" => Ok(true),
        "false" => Ok(false),
        other => Err(AppImageError::new(
            at,
            format!("expected `true` or `false`, got `{other}`"),
        )),
    }
}

/// Parse the closed `kind` vocabulary — the shared
/// [`ProgramKind`] definition that also names the store the
/// bundle is planted in, so a manifest's declaration and its destination
/// cannot disagree.
fn parse_kind(at: &str, value: &str) -> Result<ProgramKind, AppImageError> {
    let name = parse_string(at, value)?;
    ProgramKind::from_key(&name)
        .ok_or_else(|| AppImageError::new(at, format!("unknown kind `{name}`")))
}

/// Parse the closed program-library folder vocabulary
/// ([`LibraryCategory`]): the canonical, case-exact folder identifiers.
fn parse_library(at: &str, value: &str) -> Result<LibraryCategory, AppImageError> {
    let name = parse_string(at, value)?;
    LibraryCategory::from_id(&name)
        .ok_or_else(|| AppImageError::new(at, format!("unknown library folder `{name}`")))
}

/// Parse a single-line array of canonical `CAP_*` capability names.
fn parse_capabilities(at: &str, value: &str) -> Result<Vec<CapabilityId>, AppImageError> {
    let inner = value
        .strip_prefix('[')
        .and_then(|rest| rest.strip_suffix(']'))
        .ok_or_else(|| AppImageError::new(at, "expected a single-line `[...]` array"))?
        .trim();
    let mut caps = Vec::new();
    if inner.is_empty() {
        return Ok(caps);
    }
    for item in inner.split(',') {
        let name = parse_string(at, item.trim())?;
        let cap = CapabilityId::from_name(&name)
            .ok_or_else(|| AppImageError::new(at, format!("unknown capability `{name}`")))?;
        if caps.contains(&cap) {
            return Err(AppImageError::new(
                at,
                format!("duplicate capability `{name}`"),
            ));
        }
        caps.push(cap);
    }
    Ok(caps)
}

/// Parse a single-line array of double-quoted MIME-type strings — the
/// bundle's declared file-type associations. Duplicate types are refused
/// (a bundle names each type it handles once).
fn parse_associations(at: &str, value: &str) -> Result<Vec<String>, AppImageError> {
    let inner = value
        .strip_prefix('[')
        .and_then(|rest| rest.strip_suffix(']'))
        .ok_or_else(|| AppImageError::new(at, "expected a single-line `[...]` array"))?
        .trim();
    let mut mimes = Vec::new();
    if inner.is_empty() {
        return Ok(mimes);
    }
    for item in inner.split(',') {
        let mime = parse_string(at, item.trim())?;
        if mimes.contains(&mime) {
            return Err(AppImageError::new(
                at,
                format!("duplicate association `{mime}`"),
            ));
        }
        mimes.push(mime);
    }
    Ok(mimes)
}

/// One program crate the discovery walk found: its cargo package name (the
/// `-p` argument a cross-compile passes), its crate directory, and its
/// parsed manifest source.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct DiscoveredApp {
    /// The cargo package name from the crate's `Cargo.toml`.
    pub package: String,
    /// The crate directory holding `Cargo.toml` and `AppInfo.toml`.
    pub crate_dir: PathBuf,
    /// The parsed, validated manifest source.
    pub manifest: AppManifestSource,
}

/// Walk the userland crate roots (`<userland>/<class>/<crate>/`) and parse
/// every crate's `AppInfo.toml`, returning the discovered programs sorted
/// by bundle name.
///
/// This walk **is** the store's source of truth at build time: adding a
/// program is dropping an `AppInfo.toml` beside its `Cargo.toml`, never
/// editing a central list.
///
/// # Errors
///
/// Fails closed on an unreadable tree, a manifest that does not parse, a
/// crate whose `Cargo.toml` package name cannot be read, or two crates
/// claiming the same bundle name.
pub fn discover_app_manifests(userland_root: &Path) -> Result<Vec<DiscoveredApp>, AppImageError> {
    let mut found: Vec<DiscoveredApp> = Vec::new();
    for class_dir in sorted_dirs(userland_root)? {
        for crate_dir in sorted_dirs(&class_dir)? {
            let Some(discovered) = discover_crate_manifest(&crate_dir)? else {
                continue;
            };
            if let Some(clash) = found
                .iter()
                .find(|d| d.manifest.name == discovered.manifest.name)
            {
                return Err(AppImageError::new(
                    crate_dir.join(APP_MANIFEST_SOURCE).display(),
                    format!(
                        "bundle name `{}` already claimed by package `{}`",
                        discovered.manifest.name, clash.package
                    ),
                ));
            }
            found.push(discovered);
        }
    }
    found.sort_by(|a, b| a.manifest.name.cmp(&b.manifest.name));
    Ok(found)
}

/// Parse one crate directory's `AppInfo.toml` manifest source (beside its
/// `Cargo.toml`), or `None` when the crate authors no manifest. The
/// per-crate half of [`discover_app_manifests`], shared with consumers that
/// compose a single out-of-walk bundle (the memory-stability vertical's
/// test-only fixture), so the parse + package-name resolution has one
/// definition.
///
/// # Errors
///
/// Fails closed on an unreadable manifest, a manifest that does not parse,
/// or a crate whose `Cargo.toml` package name cannot be read.
pub fn discover_crate_manifest(crate_dir: &Path) -> Result<Option<DiscoveredApp>, AppImageError> {
    let manifest_path = crate_dir.join(APP_MANIFEST_SOURCE);
    if !manifest_path.is_file() {
        return Ok(None);
    }
    let text = std::fs::read_to_string(&manifest_path)
        .map_err(|e| AppImageError::new(manifest_path.display(), e))?;
    let manifest = AppManifestSource::parse(&text)
        .map_err(|e| AppImageError::new(manifest_path.display(), e))?;
    let package = read_package_name(crate_dir)?;
    Ok(Some(DiscoveredApp {
        package,
        crate_dir: crate_dir.to_path_buf(),
        manifest,
    }))
}

/// The immediate subdirectories of `root`, sorted by name so the walk is
/// deterministic across filesystems.
fn sorted_dirs(root: &Path) -> Result<Vec<PathBuf>, AppImageError> {
    let mut dirs = Vec::new();
    let entries = std::fs::read_dir(root).map_err(|e| AppImageError::new(root.display(), e))?;
    for entry in entries {
        let entry = entry.map_err(|e| AppImageError::new(root.display(), e))?;
        let path = entry.path();
        if path.is_dir() {
            dirs.push(path);
        }
    }
    dirs.sort();
    Ok(dirs)
}

/// Read the `[package] name` from a crate's `Cargo.toml`.
fn read_package_name(crate_dir: &Path) -> Result<String, AppImageError> {
    let path = crate_dir.join("Cargo.toml");
    let text = std::fs::read_to_string(&path).map_err(|e| AppImageError::new(path.display(), e))?;
    let mut in_package = false;
    for raw in text.lines() {
        let line = raw.trim();
        if line.starts_with('[') {
            in_package = line == "[package]";
            continue;
        }
        if in_package {
            if let Some(value) = line.strip_prefix("name") {
                if let Some(value) = value.trim_start().strip_prefix('=') {
                    return parse_string(&path.display().to_string(), value.trim());
                }
            }
        }
    }
    Err(AppImageError::new(
        path.display(),
        "no `[package] name` found",
    ))
}

/// How a composed manifest names the publisher its per-app state is owned by.
///
/// Naming the form explicitly, rather than inferring it from two seeds that
/// happen to be equal, keeps each call site's intent readable — the two forms
/// produce structurally different manifests and are judged by different
/// checks.
#[derive(Copy, Clone, Debug, Eq, PartialEq)]
pub enum PublisherSource<'a> {
    /// The publisher signs its own builds: the publisher key *is* the signing
    /// key and the manifest carries no delegation certificate.
    SelfPublished,
    /// The publisher key derived from this seed certifies the build signing
    /// key. This is the form the image build uses, so the certificate path is
    /// the one every boot exercises.
    Delegating(&'a [u8; 32]),
    /// A publisher key and certificate supplied verbatim, with the manifest
    /// signature still computed over the result.
    ///
    /// This composes the adversarial bundles the load gate must refuse — a
    /// certificate lifted from another bundle or another signing key — as
    /// *validly signed* manifests, so the refusal has to come from the
    /// publisher check rather than from the signature that would otherwise
    /// catch a crude byte-splice first.
    Certificate {
        /// The publisher key the manifest claims.
        pubkey: [u8; 32],
        /// The delegation certificate offered for that claim.
        cert: [u8; 64],
    },
}

/// A composed, signed wire `AppInfo` plus the public key it was signed
/// with — the trust anchor a verifier must hold to admit the bundle.
pub struct ComposedAppInfo {
    /// The manifest wire bytes: header followed by the capability body.
    pub bytes: Vec<u8>,
    /// The Ed25519 public key derived from the signing seed.
    pub signer_pubkey: [u8; 32],
    /// The Ed25519 public key derived from the publisher seed — the
    /// developer identity the bundle's per-app state is owned by, which the
    /// composed delegation certificate binds the signing key to.
    pub publisher_pubkey: [u8; 32],
}

/// Compose and Ed25519-sign a bundle's wire `AppInfo` from its manifest
/// source.
///
/// `contents` is every file the bundle ships except `AppInfo` itself
/// (`Run`, `Help/...`), sorted by path; the content hash is SHA-256 over
/// the canonical [`digest_bundle_contents`] framing, and the signature is
/// taken over the header prefix concatenated with the capability body —
/// exactly the message the verifier reconstructs, so a tampered file *or*
/// a swapped capability id breaks the bundle.
///
/// `publisher` is the developer identity the bundle's per-app state is owned
/// by. A [`PublisherSource::Delegating`] publisher signs a certificate over
/// [`AppInfoHeader::publisher_cert_message`], so the build signing key is
/// provably the publisher's choice for *this* bundle and may be rotated on a
/// later release without the app losing its stored state. Both publisher
/// fields sit inside the region the manifest signature covers, so neither can
/// be swapped behind a valid signature.
///
/// # Errors
///
/// Fails closed on an invalid contents list (unsorted, duplicate, or
/// escaping path) or a manifest that violates the wire bounds.
pub fn compose_signed_appinfo(
    seed: &[u8; 32],
    publisher: PublisherSource<'_>,
    manifest: &AppManifestSource,
    syscall_table_hash: [u8; 32],
    contents: &[BundleFileDigest<'_>],
) -> Result<ComposedAppInfo, AppImageError> {
    let ctx = &manifest.name;
    let mut framing = Vec::new();
    digest_bundle_contents(contents, &mut |chunk| framing.extend_from_slice(chunk))
        .map_err(|e| AppImageError::new(ctx, format!("invalid bundle contents: {e:?}")))?;
    let content_hash = sha256(&framing);

    let signing_key = SigningKey::from_bytes(seed);
    let signer_pubkey: [u8; 32] = signing_key.verifying_key().to_bytes();
    let publisher_key = match publisher {
        PublisherSource::SelfPublished | PublisherSource::Certificate { .. } => None,
        PublisherSource::Delegating(publisher_seed) => Some(SigningKey::from_bytes(publisher_seed)),
    };
    let publisher_pubkey: [u8; 32] = match (&publisher_key, publisher) {
        (Some(key), _) => key.verifying_key().to_bytes(),
        (None, PublisherSource::Certificate { pubkey, .. }) => pubkey,
        (None, _) => signer_pubkey,
    };

    let capability_count = u16::try_from(manifest.capabilities.len())
        .map_err(|_| AppImageError::new(ctx, "too many capabilities"))?;
    let mime_count = u16::try_from(manifest.associations.len())
        .map_err(|_| AppImageError::new(ctx, "too many associations"))?;
    let header = AppInfoHeader {
        magic: APPINFO_MAGIC,
        abi_version: ABI_VERSION_CURRENT,
        flags: if manifest.icon_bar {
            0
        } else {
            APPINFO_FLAG_NO_ICON_BAR
        },
        capability_count,
        mime_count,
        id_len: inline_len(ctx, "id", &manifest.id, BUNDLE_ID_MAX)?,
        name_len: inline_len(ctx, "name", &manifest.name, BUNDLE_NAME_MAX)?,
        version_len: inline_len(ctx, "version", &manifest.version, BUNDLE_VERSION_MAX)?,
        library_icon_len: match &manifest.library_icon {
            Some(icon) => inline_len(ctx, "library-icon", icon, LIBRARY_ICON_MAX)?,
            None => 0,
        },
        purpose_len: match &manifest.purpose {
            Some(purpose) => inline_len(ctx, "purpose", purpose, BUNDLE_PURPOSE_MAX)?,
            None => 0,
        },
        author_len: match &manifest.author {
            Some(author) => inline_len(ctx, "author", author, BUNDLE_AUTHOR_MAX)?,
            None => 0,
        },
        library: LibraryCategory::to_wire(manifest.library),
        reserved0: [0; 1],
        id: inline_buf(&manifest.id),
        name: inline_buf(&manifest.name),
        version: inline_buf(&manifest.version),
        library_icon: inline_buf(manifest.library_icon.as_deref().unwrap_or("")),
        purpose: inline_buf(manifest.purpose.as_deref().unwrap_or("")),
        author: inline_buf(manifest.author.as_deref().unwrap_or("")),
        syscall_table_hash,
        content_hash,
        signer_pubkey,
        publisher_pubkey,
        publisher_cert: match publisher {
            PublisherSource::Certificate { cert, .. } => cert,
            _ => [0; 64],
        },
        signature: [0; 64],
    };
    // The certificate signs a message naming both this bundle and the key it
    // delegates to, so it cannot be lifted onto another bundle or key. A
    // self-published bundle has nothing to delegate and leaves it zero.
    let header = match publisher_key {
        None => header,
        Some(key) => AppInfoHeader {
            publisher_cert: key.sign(&header.publisher_cert_message()).to_bytes(),
            ..header
        },
    };

    let mut bytes = Vec::with_capacity(
        AppInfoHeader::WIRE_LEN
            + manifest.capabilities.len() * 2
            + manifest.associations.len() * MIME_ENTRY_LEN,
    );
    bytes.extend_from_slice(&header.to_le_bytes());
    for cap in &manifest.capabilities {
        bytes.extend_from_slice(&cap.as_u16().to_le_bytes());
    }
    // The MIME table follows the capability-id list (the body layout
    // `mime_type_at` reads): one fixed-length entry per association, a
    // length byte then the bytes in a `MIME_TYPE_MAX` buffer. The whole
    // body — capabilities then MIME table — is covered by the signature
    // below, so a tampered association breaks the bundle.
    for mime in &manifest.associations {
        let len = u8::try_from(mime.len())
            .map_err(|_| AppImageError::new(ctx, "association too long"))?;
        let mut entry = [0u8; MIME_ENTRY_LEN];
        entry[0] = len;
        entry[1..=mime.len()].copy_from_slice(mime.as_bytes());
        bytes.extend_from_slice(&entry);
    }

    let mut signed = Vec::with_capacity(bytes.len() - 64);
    signed.extend_from_slice(&bytes[AppInfoHeader::signed_range()]);
    signed.extend_from_slice(&bytes[AppInfoHeader::WIRE_LEN..]);
    let signature = signing_key.sign(&signed).to_bytes();
    bytes[AppInfoHeader::signed_range().end..AppInfoHeader::WIRE_LEN].copy_from_slice(&signature);

    Ok(ComposedAppInfo {
        bytes,
        signer_pubkey,
        publisher_pubkey,
    })
}

/// The validated inline-field length byte.
fn inline_len(ctx: &str, key: &str, value: &str, max: usize) -> Result<u8, AppImageError> {
    check_len(ctx, key, value, max)?;
    u8::try_from(value.len()).map_err(|_| AppImageError::new(ctx, format!("`{key}` too long")))
}

/// The fixed inline buffer holding `value`'s bytes as a prefix.
fn inline_buf<const N: usize>(value: &str) -> [u8; N] {
    let mut buf = [0u8; N];
    buf[..value.len()].copy_from_slice(value.as_bytes());
    buf
}

#[cfg(test)]
mod tests {
    use super::*;
    use tairix_abi::{decode_capability_ids, mime_type_at, PublisherBinding};
    use tairix_crypto::{Ed25519PublicKey, Ed25519Signature};

    const GOOD: &str = "# a comment\n\
        id = \"os.tairix.example\"\n\
        name = \"example\"\n\
        version = \"1.2.3\"\n\
        kind = \"command\"\n\
        capabilities = [\"CAP_CONSOLE_WRITE\", \"CAP_FS_ACCESS\"]\n";

    /// A listed graphical application's manifest: the [`GOOD`] base plus
    /// the explicit library opt-in and its icon.
    fn listed() -> String {
        format!("{GOOD}library = \"Office\"\nlibrary-icon = \"example.svg\"\n")
    }

    #[test]
    fn parses_a_valid_manifest() {
        let manifest = AppManifestSource::parse(GOOD).expect("valid");
        assert_eq!(manifest.id, "os.tairix.example");
        assert_eq!(manifest.name, "example");
        assert_eq!(manifest.version, "1.2.3");
        assert_eq!(manifest.kind, ProgramKind::Command);
        assert_eq!(
            manifest.capabilities,
            [CapabilityId::CONSOLE_WRITE, CapabilityId::FS_ACCESS]
        );
        assert_eq!(manifest.bundle_dir(), "example.app");
        assert_eq!(manifest.library, None, "listing is an explicit opt-in");
        assert_eq!(manifest.library_icon, None);
        assert!(manifest.icon_bar, "the icon bar is the default");
    }

    #[test]
    fn a_bundle_may_declare_that_it_presents_no_icon_bar_slot() {
        let iconless =
            AppManifestSource::parse(&format!("{GOOD}icon-bar = false\n")).expect("valid");
        assert!(!iconless.icon_bar);

        let stated = AppManifestSource::parse(&format!("{GOOD}icon-bar = true\n")).expect("valid");
        assert!(stated.icon_bar, "the default may also be stated");

        for bad in ["\"false\"", "0", "no", "False"] {
            assert!(
                AppManifestSource::parse(&format!("{GOOD}icon-bar = {bad}\n")).is_err(),
                "`{bad}` is a packaging defect, not a value to coerce"
            );
        }
    }

    #[test]
    fn the_icon_bar_declaration_reaches_the_signed_header() {
        let composed = compose_signed_appinfo(
            &[7u8; 32],
            PublisherSource::SelfPublished,
            &AppManifestSource::parse(&format!("{GOOD}icon-bar = false\n")).expect("valid"),
            [0u8; 32],
            &[],
        )
        .expect("composes");
        let header = AppInfoHeader::from_bytes(&composed.bytes).expect("decodes");
        assert!(!header.presents_icon_bar_slot());

        let plain = compose_signed_appinfo(
            &[7u8; 32],
            PublisherSource::SelfPublished,
            &AppManifestSource::parse(GOOD).expect("valid"),
            [0u8; 32],
            &[],
        )
        .expect("composes");
        assert!(AppInfoHeader::from_bytes(&plain.bytes)
            .expect("decodes")
            .presents_icon_bar_slot());
    }

    #[test]
    fn a_listed_application_declares_its_folder_and_icon() {
        let manifest = AppManifestSource::parse(&listed()).expect("valid");
        assert_eq!(manifest.library, Some(LibraryCategory::Office));
        assert_eq!(manifest.library_icon.as_deref(), Some("example.svg"));

        // The icon is optional on a listed bundle; the folder alone lists it.
        let folder_only = format!("{GOOD}library = \"Games\"\n");
        let manifest = AppManifestSource::parse(&folder_only).expect("valid");
        assert_eq!(manifest.library, Some(LibraryCategory::Games));
        assert_eq!(manifest.library_icon, None);
    }

    /// An icon is the bundle's own identity, not a launcher-listing detail:
    /// every command app carries one and none of them is listed, so a
    /// manifest with an icon and no folder must parse.
    #[test]
    fn an_unlisted_bundle_declares_its_own_icon() {
        let text = format!("{GOOD}library-icon = \"example.png\"\n");
        let manifest = AppManifestSource::parse(&text).expect("valid");
        assert_eq!(manifest.library, None);
        assert_eq!(manifest.library_icon.as_deref(), Some("example.png"));
    }

    /// The declared kind, and nothing else, decides the store a bundle is
    /// planted in.
    #[test]
    fn the_declared_kind_selects_the_store() {
        for (key, kind, store) in [
            ("command", ProgramKind::Command, "Commands"),
            ("application", ProgramKind::Application, "Applications"),
            ("service", ProgramKind::Service, "Services"),
        ] {
            let text = GOOD.replace("\"command\"", &format!("\"{key}\""));
            let manifest = AppManifestSource::parse(&text).expect("valid");
            assert_eq!(manifest.kind, kind);
            assert_eq!(manifest.kind.store_dir(), store);
        }
    }

    #[test]
    fn parse_rejects_every_grammar_deviation() {
        // Each mutation of the valid text must fail closed.
        for (broken, why) in [
            (GOOD.replace("id =", "identifier ="), "unknown key"),
            (format!("{GOOD}id = \"twice\"\n"), "duplicate key"),
            (
                GOOD.replace("id = \"os.tairix.example\"\n", ""),
                "missing key",
            ),
            (GOOD.replace("\"command\"", "\"daemon\""), "unknown kind"),
            (
                GOOD.replace("CAP_FS_ACCESS", "CAP_NOT_A_THING"),
                "unknown capability",
            ),
            (
                GOOD.replace("CAP_FS_ACCESS", "CAP_CONSOLE_WRITE"),
                "duplicate capability",
            ),
            (
                GOOD.replace("= \"example\"", "= example"),
                "unquoted string",
            ),
            (
                GOOD.replace("name = \"example\"", "name = \"\""),
                "empty name",
            ),
            (
                GOOD.replace("name = \"example\"", "name = \"a/b\""),
                "name is not a command word",
            ),
            (GOOD.replace(['[', ']'], ""), "not an array"),
            (
                GOOD.replace("version = \"1.2.3\"\n", "version\n"),
                "no equals",
            ),
            (
                format!("{GOOD}library = \"Settings\"\n"),
                "unknown library folder",
            ),
            (
                format!("{GOOD}library = \"office\"\n"),
                "folder identifiers are case-exact",
            ),
            (
                listed().replace("\"command\"", "\"service\""),
                "a service cannot declare library",
            ),
            (
                listed().replace("example.svg", &"x".repeat(LIBRARY_ICON_MAX + 1)),
                "over-long library icon",
            ),
            (
                format!("{GOOD}library = \"Office\"\nlibrary = \"Games\"\n"),
                "duplicate library key",
            ),
        ] {
            assert!(
                AppManifestSource::parse(&broken).is_err(),
                "must reject: {why}"
            );
        }
    }

    #[test]
    fn discovery_finds_every_program_bundle_in_the_real_tree() {
        // The walk over the real source tree is the build-time source of
        // truth for the store; pin the discovered set so dropping or
        // mis-spelling a manifest is a loud test diff.
        let userland = Path::new(env!("CARGO_MANIFEST_DIR")).join("../../../userland");
        let found = discover_app_manifests(&userland).expect("discovery walks");
        let names: Vec<&str> = found.iter().map(|d| d.manifest.name.as_str()).collect();
        assert_eq!(
            names,
            [
                "applib",
                "basename",
                "cat",
                "chmod",
                "clear",
                "confd",
                "configure",
                "cp",
                "datetime",
                "desktop",
                "devmgr",
                "df",
                "dirname",
                "du",
                "edit",
                "elsh",
                "false",
                "files",
                "fontd",
                "fstree",
                "greeter",
                "groupadd",
                "head",
                "host",
                "link",
                "ln",
                "login",
                "ls",
                "lspci",
                "lsusb",
                "man",
                "mdadm",
                "mkdir",
                "mv",
                "netstack",
                "ping",
                "printf",
                "ps",
                "readlink",
                "reset",
                "rm",
                "rmdir",
                "seatmgr",
                "seq",
                "servicectl",
                "sleep",
                "ss",
                "stat",
                "stress",
                "switchboard",
                "sysinfo",
                "sysinfod",
                "sysmon",
                "tail",
                "tee",
                "telnet",
                "terminal",
                "timed",
                "top",
                "true",
                "unlink",
                "unmount",
                "useradd",
                "users",
                "viewer",
                "vim",
                "wallpaper",
                "wc",
                "whoami",
                "widgets",
                "yes"
            ]
        );
        for app in &found {
            assert!(app.crate_dir.join("Cargo.toml").is_file());
            assert!(!app.package.is_empty());
        }
    }

    #[test]
    fn discovery_pins_the_service_subset_of_the_real_tree() {
        // Which bundles install into `/System/Services` is decided by each
        // manifest's own declared kind, so pin the resulting set separately
        // from the whole inventory: a bundle that silently changed kind would
        // move store, and that is a different defect from a missing manifest.
        let userland = Path::new(env!("CARGO_MANIFEST_DIR")).join("../../../userland");
        let found = discover_app_manifests(&userland).expect("discovery walks");
        let services: Vec<&str> = found
            .iter()
            .filter(|d| d.manifest.kind == ProgramKind::Service)
            .map(|d| d.manifest.name.as_str())
            .collect();
        assert_eq!(
            services,
            [
                "confd",
                "devmgr",
                "fontd",
                "greeter",
                "login",
                "netstack",
                "seatmgr",
                "switchboard",
                "sysinfod",
                "timed"
            ]
        );
    }

    #[test]
    fn discovery_rejects_two_crates_claiming_one_bundle_name() {
        let root =
            std::env::temp_dir().join(format!("tairix-app-image-dup-{}", std::process::id()));
        let make = |class: &str, krate: &str| {
            let dir = root.join(class).join(krate);
            std::fs::create_dir_all(&dir).expect("mkdir");
            std::fs::write(
                dir.join("Cargo.toml"),
                format!("[package]\nname = \"{krate}\"\n"),
            )
            .expect("cargo.toml");
            std::fs::write(dir.join(APP_MANIFEST_SOURCE), GOOD).expect("manifest");
        };
        make("apps", "first");
        make("shell", "second");
        let result = discover_app_manifests(&root);
        std::fs::remove_dir_all(&root).expect("cleanup");
        let err = result.expect_err("duplicate bundle name must fail closed");
        assert!(err.to_string().contains("already claimed"));
    }

    #[test]
    fn composed_appinfo_verifies_and_binds_the_contents() {
        let manifest = AppManifestSource::parse(GOOD).expect("valid");
        let seed = [7u8; 32];
        let syscall_hash = [0xAB; 32];
        let run = BundleFileDigest {
            path: "Run",
            bytes: b"program bytes",
        };
        let publisher_seed = [11u8; 32];
        let composed = compose_signed_appinfo(
            &seed,
            PublisherSource::Delegating(&publisher_seed),
            &manifest,
            syscall_hash,
            &[run],
        )
        .expect("composes");

        // The wire manifest decodes and carries the source's identity.
        let header = AppInfoHeader::from_bytes(&composed.bytes).expect("decodes");
        assert_eq!(header.bundle_id(), "os.tairix.example");
        assert_eq!(header.bundle_name(), "example");
        assert_eq!(header.bundle_version(), "1.2.3");
        assert_eq!(header.library_category(), None);
        assert_eq!(header.library_icon(), None);
        assert_eq!(header.syscall_table_hash, syscall_hash);
        assert_eq!(header.signer_pubkey, composed.signer_pubkey);
        assert_eq!(header.publisher_pubkey, composed.publisher_pubkey);
        assert_eq!(header.publisher_binding(), Ok(PublisherBinding::Delegated));

        // The capability body decodes to the requested set.
        let body = &composed.bytes[AppInfoHeader::WIRE_LEN..];
        let count = usize::from(header.capability_count);
        let mut caps = vec![CapabilityId::FS_MOUNT; count];
        assert_eq!(decode_capability_ids(body, count, &mut caps), Ok(count));
        assert_eq!(caps, manifest.capabilities);

        // The signature verifies over header prefix ‖ body — the exact
        // message the bundle verifier reconstructs.
        let mut signed = Vec::new();
        signed.extend_from_slice(&composed.bytes[AppInfoHeader::signed_range()]);
        signed.extend_from_slice(body);
        let key = Ed25519PublicKey::from_bytes(&composed.signer_pubkey).expect("key");
        key.verify(&signed, &Ed25519Signature(header.signature))
            .expect("signature verifies");

        // A flipped capability id breaks the signature (the body is
        // covered), and the content hash binds the Run bytes.
        let mut tampered = signed.clone();
        let body_at = AppInfoHeader::signed_range().end;
        tampered[body_at] ^= 0xFF;
        assert!(key
            .verify(&tampered, &Ed25519Signature(header.signature))
            .is_err());
        let mut framing = Vec::new();
        digest_bundle_contents(&[run], &mut |chunk| framing.extend_from_slice(chunk))
            .expect("frames");
        assert_eq!(header.content_hash, sha256(&framing));
        let other = BundleFileDigest {
            path: "Run",
            bytes: b"different bytes",
        };
        let mut other_framing = Vec::new();
        digest_bundle_contents(&[other], &mut |chunk| {
            other_framing.extend_from_slice(chunk);
        })
        .expect("frames");
        assert_ne!(header.content_hash, sha256(&other_framing));
    }

    /// The composed certificate is the publisher's real signature over the
    /// message the load gate reconstructs — so what the build signs and what
    /// the gate verifies are the same bytes, and a rotated signing key still
    /// carries the same publisher.
    #[test]
    fn the_composed_certificate_delegates_this_bundle_to_this_signing_key() {
        let manifest = AppManifestSource::parse(GOOD).expect("valid");
        let publisher_seed = [11u8; 32];
        let compose = |seed: &[u8; 32]| {
            compose_signed_appinfo(
                seed,
                PublisherSource::Delegating(&publisher_seed),
                &manifest,
                [0xAB; 32],
                &[BundleFileDigest {
                    path: "Run",
                    bytes: b"program bytes",
                }],
            )
            .expect("composes")
        };

        let first = compose(&[7u8; 32]);
        let header = AppInfoHeader::from_bytes(&first.bytes).expect("decodes");
        let key = Ed25519PublicKey::from_bytes(&header.publisher_pubkey).expect("publisher key");
        assert!(key
            .verify(
                &header.publisher_cert_message(),
                &Ed25519Signature(header.publisher_cert)
            )
            .is_ok());

        // A different build key, the same developer: a new certificate, the
        // same publisher identity.
        let next = compose(&[21u8; 32]);
        let next_header = AppInfoHeader::from_bytes(&next.bytes).expect("decodes");
        assert_ne!(next.signer_pubkey, first.signer_pubkey);
        assert_ne!(next_header.publisher_cert, header.publisher_cert);
        assert_eq!(next.publisher_pubkey, first.publisher_pubkey);
    }

    /// A self-published bundle has nothing to delegate: the publisher key is
    /// the signing key and the certificate stays zero, so the gate checks no
    /// second signature.
    #[test]
    fn a_self_published_composition_carries_no_certificate() {
        let manifest = AppManifestSource::parse(GOOD).expect("valid");
        let composed = compose_signed_appinfo(
            &[7u8; 32],
            PublisherSource::SelfPublished,
            &manifest,
            [0xAB; 32],
            &[BundleFileDigest {
                path: "Run",
                bytes: b"program bytes",
            }],
        )
        .expect("composes");
        let header = AppInfoHeader::from_bytes(&composed.bytes).expect("decodes");
        assert_eq!(composed.publisher_pubkey, composed.signer_pubkey);
        assert_eq!(header.publisher_cert, [0u8; 64]);
        assert_eq!(
            header.publisher_binding(),
            Ok(PublisherBinding::SelfPublished)
        );
    }

    #[test]
    fn a_listed_applications_folder_and_icon_survive_composition() {
        let manifest = AppManifestSource::parse(&listed()).expect("valid");
        let composed = compose_signed_appinfo(
            &[3u8; 32],
            PublisherSource::Delegating(&[4u8; 32]),
            &manifest,
            [0xCD; 32],
            &[BundleFileDigest {
                path: "Run",
                bytes: b"program bytes",
            }],
        )
        .expect("composes");
        let header = AppInfoHeader::from_bytes(&composed.bytes).expect("decodes");
        assert_eq!(header.library_category(), Some(LibraryCategory::Office));
        assert_eq!(header.library_icon(), Some("example.svg"));
    }

    #[test]
    fn associations_are_optional_and_default_empty() {
        let manifest = AppManifestSource::parse(GOOD).expect("valid");
        assert!(manifest.associations.is_empty());
    }

    #[test]
    fn associations_parse_in_order_and_reject_duplicates() {
        let text = format!("{GOOD}associations = [\"text/plain\", \"text/markdown\"]\n");
        let manifest = AppManifestSource::parse(&text).expect("valid");
        assert_eq!(manifest.associations, ["text/plain", "text/markdown"]);

        let dup = format!("{GOOD}associations = [\"text/plain\", \"text/plain\"]\n");
        assert!(AppManifestSource::parse(&dup).is_err());
    }

    #[test]
    fn composed_appinfo_carries_the_signed_mime_table() {
        let text = format!("{GOOD}associations = [\"text/plain\", \"text/markdown\"]\n");
        let manifest = AppManifestSource::parse(&text).expect("valid");
        let composed = compose_signed_appinfo(
            &[9u8; 32],
            PublisherSource::Delegating(&[10u8; 32]),
            &manifest,
            [0x11; 32],
            &[BundleFileDigest {
                path: "Run",
                bytes: b"program",
            }],
        )
        .expect("composes");

        let header = AppInfoHeader::from_bytes(&composed.bytes).expect("decodes");
        assert_eq!(header.mime_count, 2);

        // The MIME table follows the capability body and reads back the
        // declared types in order.
        let body = &composed.bytes[AppInfoHeader::WIRE_LEN..];
        let caps = usize::from(header.capability_count);
        assert_eq!(mime_type_at(body, caps, 0), Ok("text/plain"));
        assert_eq!(mime_type_at(body, caps, 1), Ok("text/markdown"));

        // The MIME table is covered by the signature — a flipped byte in it
        // breaks verification.
        let mut signed = Vec::new();
        signed.extend_from_slice(&composed.bytes[AppInfoHeader::signed_range()]);
        signed.extend_from_slice(body);
        let key = Ed25519PublicKey::from_bytes(&composed.signer_pubkey).expect("key");
        key.verify(&signed, &Ed25519Signature(header.signature))
            .expect("verifies");
        let mut tampered = signed.clone();
        let mime_at = AppInfoHeader::signed_range().end + caps * 2 + 1;
        tampered[mime_at] ^= 0xFF;
        assert!(key
            .verify(&tampered, &Ed25519Signature(header.signature))
            .is_err());
    }

    #[test]
    fn compose_refuses_an_invalid_contents_list() {
        let manifest = AppManifestSource::parse(GOOD).expect("valid");
        let unsorted = [
            BundleFileDigest {
                path: "Run",
                bytes: b"",
            },
            BundleFileDigest {
                path: "Help/en-US/example.md",
                bytes: b"",
            },
        ];
        assert!(compose_signed_appinfo(
            &[7u8; 32],
            PublisherSource::Delegating(&[8u8; 32]),
            &manifest,
            [0; 32],
            &unsorted
        )
        .is_err());
    }
}
