//! Build-time signed application-bundle composer and `AppInfo.toml`
//! manifest discovery (`plans/APPS.md` deliverable 8).
//!
//! Every runnable program — a command app or a service (a service is an
//! app) — ships as a self-contained signed bundle on the read-only
//! `/System` store. Each program crate authors its manifest **source** as
//! an `AppInfo.toml` beside its `Cargo.toml`; the image builds discover
//! those files by walking the userland crate roots
//! ([`discover_app_manifests`]) — never a hand-maintained per-bundle list —
//! and compose each bundle's signed wire `AppInfo`
//! ([`compose_signed_appinfo`]): the [`rustos_abi::AppInfoHeader`] plus its
//! capability body, content-hashed over the canonical
//! [`rustos_abi::digest_bundle_contents`] framing and Ed25519-signed over
//! the header prefix concatenated with the body — exactly the message the
//! bundle verifier reconstructs.
//!
//! The manifest-source grammar is a deliberately tiny, line-based TOML
//! subset, parsed fail-closed: `#` comments, and exactly the keys `id`,
//! `name`, `version`, `kind`, and `capabilities` (a single-line array of
//! canonical `CAP_*` names), each exactly once. Anything else — an unknown
//! key, a duplicate, a multi-line value, an unknown capability name — is a
//! packaging defect that fails the build, never a guessed default.

use std::fmt;
use std::path::{Path, PathBuf};

use ed25519_dalek::{Signer, SigningKey};
use rustos_abi::{
    digest_bundle_contents, AppInfoHeader, BundleFileDigest, CapabilityId, ABI_VERSION_CURRENT,
    APPINFO_MAGIC, APPINFO_MAX_CAPABILITIES, BUNDLE_ID_MAX, BUNDLE_NAME_MAX, BUNDLE_SUFFIX,
    BUNDLE_VERSION_MAX,
};
use rustos_crypto::sha256;

/// File name of a program crate's manifest source, beside its `Cargo.toml`.
pub const APP_MANIFEST_SOURCE: &str = "AppInfo.toml";

/// Where a program's bundle lives on the read-only `/System` volume.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum AppKind {
    /// A command app the shell resolves by its bare name: planted under the
    /// system app store (`/System/Apps/<name>.app/`).
    Command,
    /// A long-running system service (a service is an app): planted under
    /// `/System/Services/<name>.app/`.
    Service,
}

impl AppKind {
    /// The `/System`-volume-relative store directory bundles of this kind
    /// are planted in.
    #[must_use]
    pub fn store_dir(self) -> &'static str {
        match self {
            Self::Command => "Apps",
            Self::Service => "Services",
        }
    }
}

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
    /// Bundle identifier (e.g. `os.rustos.ls`).
    pub id: String,
    /// The program's name — also its bundle directory stem and, for a
    /// command app, the command word the shell resolves.
    pub name: String,
    /// Bundle version string.
    pub version: String,
    /// Which store the bundle is planted in.
    pub kind: AppKind,
    /// The bundle's requested capabilities, in manifest order.
    pub capabilities: Vec<CapabilityId>,
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
    /// name, or a capability list exceeding the manifest bound.
    pub fn parse(text: &str) -> Result<Self, AppImageError> {
        let ctx = APP_MANIFEST_SOURCE;
        let mut id = None;
        let mut name = None;
        let mut version = None;
        let mut kind = None;
        let mut capabilities = None;
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

/// Parse the closed `kind` vocabulary.
fn parse_kind(at: &str, value: &str) -> Result<AppKind, AppImageError> {
    match parse_string(at, value)?.as_str() {
        "command" => Ok(AppKind::Command),
        "service" => Ok(AppKind::Service),
        other => Err(AppImageError::new(at, format!("unknown kind `{other}`"))),
    }
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
            let manifest_path = crate_dir.join(APP_MANIFEST_SOURCE);
            if !manifest_path.is_file() {
                continue;
            }
            let text = std::fs::read_to_string(&manifest_path)
                .map_err(|e| AppImageError::new(manifest_path.display(), e))?;
            let manifest = AppManifestSource::parse(&text)
                .map_err(|e| AppImageError::new(manifest_path.display(), e))?;
            let package = read_package_name(&crate_dir)?;
            if let Some(clash) = found.iter().find(|d| d.manifest.name == manifest.name) {
                return Err(AppImageError::new(
                    manifest_path.display(),
                    format!(
                        "bundle name `{}` already claimed by package `{}`",
                        manifest.name, clash.package
                    ),
                ));
            }
            found.push(DiscoveredApp {
                package,
                crate_dir,
                manifest,
            });
        }
    }
    found.sort_by(|a, b| a.manifest.name.cmp(&b.manifest.name));
    Ok(found)
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

/// A composed, signed wire `AppInfo` plus the public key it was signed
/// with — the trust anchor a verifier must hold to admit the bundle.
pub struct ComposedAppInfo {
    /// The manifest wire bytes: header followed by the capability body.
    pub bytes: Vec<u8>,
    /// The Ed25519 public key derived from the signing seed.
    pub signer_pubkey: [u8; 32],
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
/// # Errors
///
/// Fails closed on an invalid contents list (unsorted, duplicate, or
/// escaping path) or a manifest that violates the wire bounds.
pub fn compose_signed_appinfo(
    seed: &[u8; 32],
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

    let capability_count = u16::try_from(manifest.capabilities.len())
        .map_err(|_| AppImageError::new(ctx, "too many capabilities"))?;
    let header = AppInfoHeader {
        magic: APPINFO_MAGIC,
        abi_version: ABI_VERSION_CURRENT,
        flags: 0,
        capability_count,
        mime_count: 0,
        id_len: inline_len(ctx, "id", &manifest.id, BUNDLE_ID_MAX)?,
        name_len: inline_len(ctx, "name", &manifest.name, BUNDLE_NAME_MAX)?,
        version_len: inline_len(ctx, "version", &manifest.version, BUNDLE_VERSION_MAX)?,
        reserved0: 0,
        id: inline_buf(&manifest.id),
        name: inline_buf(&manifest.name),
        version: inline_buf(&manifest.version),
        syscall_table_hash,
        content_hash,
        signer_pubkey,
        signature: [0; 64],
    };

    let mut bytes = Vec::with_capacity(AppInfoHeader::WIRE_LEN + manifest.capabilities.len() * 2);
    bytes.extend_from_slice(&header.to_le_bytes());
    for cap in &manifest.capabilities {
        bytes.extend_from_slice(&cap.as_u16().to_le_bytes());
    }

    let mut signed = Vec::with_capacity(bytes.len() - 64);
    signed.extend_from_slice(&bytes[AppInfoHeader::signed_range()]);
    signed.extend_from_slice(&bytes[AppInfoHeader::WIRE_LEN..]);
    let signature = signing_key.sign(&signed).to_bytes();
    bytes[AppInfoHeader::signed_range().end..AppInfoHeader::WIRE_LEN].copy_from_slice(&signature);

    Ok(ComposedAppInfo {
        bytes,
        signer_pubkey,
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
    use rustos_abi::decode_capability_ids;
    use rustos_crypto::{Ed25519PublicKey, Ed25519Signature};

    const GOOD: &str = "# a comment\n\
        id = \"os.rustos.example\"\n\
        name = \"example\"\n\
        version = \"1.2.3\"\n\
        kind = \"command\"\n\
        capabilities = [\"CAP_CONSOLE_WRITE\", \"CAP_FS_ACCESS\"]\n";

    #[test]
    fn parses_a_valid_manifest() {
        let manifest = AppManifestSource::parse(GOOD).expect("valid");
        assert_eq!(manifest.id, "os.rustos.example");
        assert_eq!(manifest.name, "example");
        assert_eq!(manifest.version, "1.2.3");
        assert_eq!(manifest.kind, AppKind::Command);
        assert_eq!(
            manifest.capabilities,
            [CapabilityId::CONSOLE_WRITE, CapabilityId::FS_ACCESS]
        );
        assert_eq!(manifest.bundle_dir(), "example.app");
    }

    #[test]
    fn service_kind_selects_the_services_store() {
        let text = GOOD.replace("\"command\"", "\"service\"");
        let manifest = AppManifestSource::parse(&text).expect("valid");
        assert_eq!(manifest.kind, AppKind::Service);
        assert_eq!(manifest.kind.store_dir(), "Services");
        assert_eq!(AppKind::Command.store_dir(), "Apps");
    }

    #[test]
    fn parse_rejects_every_grammar_deviation() {
        // Each mutation of the valid text must fail closed.
        for (broken, why) in [
            (GOOD.replace("id =", "identifier ="), "unknown key"),
            (format!("{GOOD}id = \"twice\"\n"), "duplicate key"),
            (
                GOOD.replace("id = \"os.rustos.example\"\n", ""),
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
            ["devmgr", "elsh", "login", "ls", "man", "ps", "sysinfo", "sysinfod", "top", "users"]
        );
        for app in &found {
            assert!(app.crate_dir.join("Cargo.toml").is_file());
            assert!(!app.package.is_empty());
        }
        let services: Vec<&str> = found
            .iter()
            .filter(|d| d.manifest.kind == AppKind::Service)
            .map(|d| d.manifest.name.as_str())
            .collect();
        assert_eq!(services, ["devmgr", "login", "sysinfod"]);
    }

    #[test]
    fn discovery_rejects_two_crates_claiming_one_bundle_name() {
        let root =
            std::env::temp_dir().join(format!("rustos-app-image-dup-{}", std::process::id()));
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
        let composed =
            compose_signed_appinfo(&seed, &manifest, syscall_hash, &[run]).expect("composes");

        // The wire manifest decodes and carries the source's identity.
        let header = AppInfoHeader::from_bytes(&composed.bytes).expect("decodes");
        assert_eq!(header.bundle_id(), "os.rustos.example");
        assert_eq!(header.bundle_name(), "example");
        assert_eq!(header.bundle_version(), "1.2.3");
        assert_eq!(header.syscall_table_hash, syscall_hash);
        assert_eq!(header.signer_pubkey, composed.signer_pubkey);

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

    #[test]
    fn compose_refuses_an_invalid_contents_list() {
        let manifest = AppManifestSource::parse(GOOD).expect("valid");
        let unsorted = [
            BundleFileDigest {
                path: "Run",
                bytes: b"",
            },
            BundleFileDigest {
                path: "Help/default/example.md",
                bytes: b"",
            },
        ];
        assert!(compose_signed_appinfo(&[7u8; 32], &manifest, [0; 32], &unsorted).is_err());
    }
}
