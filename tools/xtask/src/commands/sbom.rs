//! `cargo xtask sbom` implementation (§19.3 of `PLAN.md`, item 3 of the
//! §19 Threat Model and Hardening burn-down).
//!
//! `AGENTS.md` §19.3 requires every image to embed a `CycloneDX` Software
//! Bill of Materials "listing every workspace and external crate by
//! version, source URL, and source checksum". This command produces that
//! document from the committed `Cargo.lock`, which already records the
//! authoritative name, version, source, and registry checksum of every
//! resolved package (the same source-hash pinning §19.3 relies on as the
//! defence against the xz-utils class of attack).
//!
//! The generator is deliberately self-contained (`AGENTS.md` §2.12 —
//! "roll your own"): it parses the `[[package]]` blocks of `Cargo.lock`
//! and serialises `CycloneDX` JSON by hand rather than pulling in a
//! `serde`/`cyclonedx` dependency, and it does not shell out to `cargo
//! metadata`. The output is deterministic — components are sorted and no
//! timestamp or random serial number is emitted — so it composes with the
//! reproducible-build verification tracked as a later §19.3 burn-down item.
//!
//! Signing the SBOM with the per-installation key (`AGENTS.md` §19.3 /
//! §11) is deliberately *not* done here: no private-key signing API
//! exists yet (`lib/crypto` is verify-only and the local capability
//! authority is a later stage). That step is tracked in `PLAN.md`
//! alongside the other key-dependent §19.4/§19.3 work; this command emits
//! the unsigned document the signer will wrap.

use std::path::Path;

/// One resolved package as recorded by a `[[package]]` block in
/// `Cargo.lock`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct LockedPackage {
    /// Crate name.
    pub name: String,
    /// Resolved semantic version.
    pub version: String,
    /// The `source` field, verbatim. Absent for workspace-local crates.
    pub source: Option<String>,
    /// The `checksum` field (a registry SHA-256). Absent for
    /// workspace-local crates and for sources that do not pin one.
    pub checksum: Option<String>,
}

impl LockedPackage {
    /// Classify the package by the kind of `source` it declares. A crate
    /// with no `source` is part of this workspace; everything else is an
    /// external dependency that widens the trusted computing base
    /// (`AGENTS.md` §2.12).
    fn source_class(&self) -> &'static str {
        match self.source.as_deref() {
            None => "workspace",
            Some(_) if self.is_external_registry() => "registry",
            Some(s) if s.starts_with("git+") => "git",
            Some(_) => "other",
        }
    }

    /// `true` when this package is an external crate resolved from a Cargo
    /// registry (its `source` carries the `registry+` scheme). These are
    /// exactly the crates whose tarball hash the §19.3 source-hash
    /// allow-list pins; workspace-local crates and git sources are not.
    pub fn is_external_registry(&self) -> bool {
        self.source
            .as_deref()
            .is_some_and(|s| s.starts_with("registry+"))
    }

    /// The distribution URL for an external package: the bare URL with
    /// Cargo's `registry+` / `git+` scheme prefix stripped. `None` for
    /// workspace-local crates.
    fn distribution_url(&self) -> Option<&str> {
        let source = self.source.as_deref()?;
        Some(
            source
                .strip_prefix("registry+")
                .or_else(|| source.strip_prefix("git+"))
                .unwrap_or(source),
        )
    }

    /// The package URL (purl) identifying this crate in the Cargo
    /// ecosystem, used as the `CycloneDX` `bom-ref`.
    fn purl(&self) -> String {
        format!("pkg:cargo/{}@{}", self.name, self.version)
    }
}

/// Workspace-level identity used to populate the SBOM's root component.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct WorkspaceMeta {
    /// `[workspace.package] version`.
    pub version: String,
    /// `[workspace.package] repository`, if declared.
    pub repository: Option<String>,
}

/// Parse the `[[package]]` blocks out of a `Cargo.lock` document.
///
/// `Cargo.lock` is generated and has a stable, line-oriented shape: a
/// sequence of `[[package]]` tables, each with `name` and `version` keys
/// and optional `source`/`checksum` keys. We parse exactly that shape
/// rather than embedding a full TOML reader; an unexpected structure
/// (a package missing its name or version) is a hard error so the SBOM
/// can never silently omit a dependency.
pub fn parse_cargo_lock(text: &str) -> Result<Vec<LockedPackage>, String> {
    let mut packages = Vec::new();
    let mut current: Option<PartialPackage> = None;

    for (lineno, raw) in text.lines().enumerate() {
        let line = raw.trim();
        if line == "[[package]]" {
            if let Some(partial) = current.take() {
                packages.push(partial.finish(lineno)?);
            }
            current = Some(PartialPackage::default());
            continue;
        }
        // Keys belong to the most recently opened `[[package]]`. A new
        // top-level table (e.g. a future `[metadata]`) ends the run.
        if line.starts_with('[') {
            if let Some(partial) = current.take() {
                packages.push(partial.finish(lineno)?);
            }
            continue;
        }
        let Some(partial) = current.as_mut() else {
            continue;
        };
        if let Some(value) = parse_key(line, "name") {
            partial.name = Some(value);
        } else if let Some(value) = parse_key(line, "version") {
            partial.version = Some(value);
        } else if let Some(value) = parse_key(line, "source") {
            partial.source = Some(value);
        } else if let Some(value) = parse_key(line, "checksum") {
            partial.checksum = Some(value);
        }
    }
    if let Some(partial) = current.take() {
        packages.push(partial.finish(text.lines().count())?);
    }

    packages.sort_by(|a, b| a.name.cmp(&b.name).then_with(|| a.version.cmp(&b.version)));
    Ok(packages)
}

#[derive(Default)]
struct PartialPackage {
    name: Option<String>,
    version: Option<String>,
    source: Option<String>,
    checksum: Option<String>,
}

impl PartialPackage {
    fn finish(self, lineno: usize) -> Result<LockedPackage, String> {
        let name = self
            .name
            .ok_or_else(|| format!("Cargo.lock: [[package]] before line {lineno} has no `name`"))?;
        let version = self.version.ok_or_else(|| {
            format!("Cargo.lock: package `{name}` (before line {lineno}) has no `version`")
        })?;
        Ok(LockedPackage {
            name,
            version,
            source: self.source,
            checksum: self.checksum,
        })
    }
}

/// Extract the string value of `key = "..."` from a single line, if the
/// line assigns exactly that key. Returns `None` for any other line.
pub(crate) fn parse_key(line: &str, key: &str) -> Option<String> {
    let rest = line.strip_prefix(key)?.trim_start();
    let rest = rest.strip_prefix('=')?.trim_start();
    let inner = rest.strip_prefix('"')?;
    let end = inner.find('"')?;
    Some(inner[..end].to_string())
}

/// Parse `[workspace.package]` `version` and `repository` from the
/// workspace `Cargo.toml`. The version is required (it identifies the
/// SBOM's root component); the repository is optional.
pub fn parse_workspace_meta(cargo_toml: &str) -> Result<WorkspaceMeta, String> {
    let mut in_section = false;
    let mut version = None;
    let mut repository = None;
    for line in cargo_toml.lines() {
        let trimmed = line.trim();
        if trimmed.starts_with('[') {
            in_section = trimmed == "[workspace.package]";
            continue;
        }
        if !in_section {
            continue;
        }
        if let Some(value) = parse_key(trimmed, "version") {
            version = Some(value);
        } else if let Some(value) = parse_key(trimmed, "repository") {
            repository = Some(value);
        }
    }
    let version =
        version.ok_or_else(|| "Cargo.toml: [workspace.package] has no `version`".to_string())?;
    Ok(WorkspaceMeta {
        version,
        repository,
    })
}

/// Serialise a `CycloneDX` 1.5 JSON BOM for the resolved package set.
///
/// The output is deterministic: `packages` is expected pre-sorted (as
/// returned by [`parse_cargo_lock`]), and no timestamp or random serial
/// number is emitted. Every package becomes a `library` component carrying
/// its purl, its registry checksum (as a `SHA-256` hash) when one exists,
/// its distribution URL, and a `rustos:source-class` property marking it
/// `workspace`, `registry`, `git`, or `other`.
pub fn build_cyclonedx(packages: &[LockedPackage], meta: &WorkspaceMeta) -> String {
    let mut out = String::new();
    out.push_str("{\n");
    out.push_str("  \"bomFormat\": \"CycloneDX\",\n");
    out.push_str("  \"specVersion\": \"1.5\",\n");
    out.push_str("  \"version\": 1,\n");
    out.push_str("  \"metadata\": {\n");
    out.push_str("    \"tools\": [\n");
    out.push_str("      {\n");
    out.push_str("        \"vendor\": \"RustOS\",\n");
    out.push_str("        \"name\": \"cargo-xtask-sbom\",\n");
    write_field(&mut out, "        ", "version", &meta.version, true);
    out.push_str("      }\n");
    out.push_str("    ],\n");
    out.push_str("    \"component\": {\n");
    out.push_str("      \"type\": \"application\",\n");
    let root_purl = format!("pkg:cargo/rustos@{}", meta.version);
    write_field(&mut out, "      ", "bom-ref", &root_purl, false);
    write_field(&mut out, "      ", "name", "rustos", false);
    write_field(&mut out, "      ", "version", &meta.version, false);
    write_field(
        &mut out,
        "      ",
        "purl",
        &root_purl,
        meta.repository.is_none(),
    );
    if let Some(repo) = &meta.repository {
        out.push_str("      \"externalReferences\": [\n");
        out.push_str("        {\n");
        write_field(&mut out, "          ", "type", "vcs", false);
        write_field(&mut out, "          ", "url", repo, true);
        out.push_str("        }\n");
        out.push_str("      ]\n");
    }
    out.push_str("    }\n");
    out.push_str("  },\n");

    out.push_str("  \"components\": [");
    for (idx, pkg) in packages.iter().enumerate() {
        if idx == 0 {
            out.push('\n');
        }
        write_component(&mut out, pkg);
        if idx + 1 < packages.len() {
            out.push_str(",\n");
        } else {
            out.push('\n');
        }
    }
    if packages.is_empty() {
        out.push_str("]\n");
    } else {
        out.push_str("  ]\n");
    }
    out.push_str("}\n");
    out
}

fn write_component(out: &mut String, pkg: &LockedPackage) {
    let purl = pkg.purl();
    out.push_str("    {\n");
    write_field(out, "      ", "type", "library", false);
    write_field(out, "      ", "bom-ref", &purl, false);
    write_field(out, "      ", "name", &pkg.name, false);
    write_field(out, "      ", "version", &pkg.version, false);
    write_field(out, "      ", "purl", &purl, false);

    if let Some(checksum) = &pkg.checksum {
        out.push_str("      \"hashes\": [\n");
        out.push_str("        {\n");
        write_field(out, "          ", "alg", "SHA-256", false);
        write_field(out, "          ", "content", checksum, true);
        out.push_str("        }\n");
        out.push_str("      ],\n");
    }

    if let Some(url) = pkg.distribution_url() {
        out.push_str("      \"externalReferences\": [\n");
        out.push_str("        {\n");
        write_field(out, "          ", "type", "distribution", false);
        write_field(out, "          ", "url", url, true);
        out.push_str("        }\n");
        out.push_str("      ],\n");
    }

    out.push_str("      \"properties\": [\n");
    out.push_str("        {\n");
    write_field(out, "          ", "name", "rustos:source-class", false);
    write_field(out, "          ", "value", pkg.source_class(), true);
    out.push_str("        }\n");
    out.push_str("      ]\n");
    out.push_str("    }");
}

/// Append a `"key": "value"` JSON member at `indent`. `last` controls the
/// trailing comma so the caller never has to track member ordering.
fn write_field(out: &mut String, indent: &str, key: &str, value: &str, last: bool) {
    out.push_str(indent);
    out.push('"');
    out.push_str(key);
    out.push_str("\": \"");
    json_escape_into(out, value);
    out.push('"');
    if last {
        out.push('\n');
    } else {
        out.push_str(",\n");
    }
}

/// Escape a string for inclusion in a JSON document per RFC 8259.
fn json_escape_into(out: &mut String, value: &str) {
    for ch in value.chars() {
        match ch {
            '"' => out.push_str("\\\""),
            '\\' => out.push_str("\\\\"),
            '\n' => out.push_str("\\n"),
            '\r' => out.push_str("\\r"),
            '\t' => out.push_str("\\t"),
            c if (c as u32) < 0x20 => {
                use std::fmt::Write as _;
                let _ = write!(out, "\\u{:04x}", c as u32);
            }
            c => out.push(c),
        }
    }
}

/// Generate the SBOM from the workspace's `Cargo.lock` and `Cargo.toml`,
/// writing `CycloneDX` JSON to `output` (or stdout when `None`).
pub fn run(workspace_root: &Path, output: Option<&Path>) -> Result<(), String> {
    let lock_path = workspace_root.join("Cargo.lock");
    let lock = std::fs::read_to_string(&lock_path)
        .map_err(|e| format!("sbom: cannot read {}: {e}", lock_path.display()))?;
    let packages = parse_cargo_lock(&lock)?;

    let manifest_path = workspace_root.join("Cargo.toml");
    let manifest = std::fs::read_to_string(&manifest_path)
        .map_err(|e| format!("sbom: cannot read {}: {e}", manifest_path.display()))?;
    let meta = parse_workspace_meta(&manifest)?;

    let document = build_cyclonedx(&packages, &meta);

    match output {
        Some(path) => {
            if let Some(parent) = path.parent() {
                if !parent.as_os_str().is_empty() {
                    std::fs::create_dir_all(parent)
                        .map_err(|e| format!("sbom: cannot create {}: {e}", parent.display()))?;
                }
            }
            std::fs::write(path, document.as_bytes())
                .map_err(|e| format!("sbom: cannot write {}: {e}", path.display()))?;
            eprintln!(
                "xtask: [sbom] wrote {} components to {}",
                packages.len(),
                path.display()
            );
        }
        None => {
            print!("{document}");
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    const SAMPLE_LOCK: &str = r#"# This file is automatically @generated by Cargo.
# It is not intended for manual editing.
version = 3

[[package]]
name = "aho-corasick"
version = "1.1.4"
source = "registry+https://github.com/rust-lang/crates.io-index"
checksum = "ddd31a130427c27518df266943a5308ed92d4b226cc639f5a8f1002816174301"
dependencies = [
 "memchr",
]

[[package]]
name = "rustos-abi"
version = "0.0.0"

[[package]]
name = "rustos-xtask"
version = "0.0.0"
dependencies = [
 "rustos-abi",
]
"#;

    #[test]
    fn parses_external_and_workspace_packages() {
        let pkgs = parse_cargo_lock(SAMPLE_LOCK).expect("parse");
        assert_eq!(pkgs.len(), 3);
        // Sorted by name.
        assert_eq!(pkgs[0].name, "aho-corasick");
        assert_eq!(pkgs[0].version, "1.1.4");
        assert_eq!(
            pkgs[0].checksum.as_deref(),
            Some("ddd31a130427c27518df266943a5308ed92d4b226cc639f5a8f1002816174301")
        );
        assert_eq!(pkgs[0].source_class(), "registry");
        assert_eq!(
            pkgs[0].distribution_url(),
            Some("https://github.com/rust-lang/crates.io-index")
        );

        let abi = &pkgs[1];
        assert_eq!(abi.name, "rustos-abi");
        assert_eq!(abi.source, None);
        assert_eq!(abi.checksum, None);
        assert_eq!(abi.source_class(), "workspace");
        assert_eq!(abi.distribution_url(), None);
    }

    #[test]
    fn package_without_version_is_rejected() {
        let lock = "[[package]]\nname = \"broken\"\n";
        let err = parse_cargo_lock(lock).unwrap_err();
        assert!(err.contains("has no `version`"), "{err}");
    }

    #[test]
    fn parses_workspace_metadata() {
        let toml = r#"
[workspace]
members = []

[workspace.package]
version = "0.0.0"
edition = "2021"
repository = "https://github.com/rustos-project/rustos"

[workspace.lints.rust]
version = "ignored-outside-section"
"#;
        let meta = parse_workspace_meta(toml).expect("meta");
        assert_eq!(meta.version, "0.0.0");
        assert_eq!(
            meta.repository.as_deref(),
            Some("https://github.com/rustos-project/rustos")
        );
    }

    #[test]
    fn cyclonedx_lists_every_package_with_checksum_and_purl() {
        let pkgs = parse_cargo_lock(SAMPLE_LOCK).expect("parse");
        let meta = WorkspaceMeta {
            version: "0.0.0".to_string(),
            repository: Some("https://github.com/rustos-project/rustos".to_string()),
        };
        let doc = build_cyclonedx(&pkgs, &meta);

        assert!(doc.contains("\"bomFormat\": \"CycloneDX\""));
        assert!(doc.contains("\"specVersion\": \"1.5\""));
        // External crate: purl + checksum + distribution URL.
        assert!(doc.contains("\"purl\": \"pkg:cargo/aho-corasick@1.1.4\""));
        assert!(doc.contains(
            "\"content\": \"ddd31a130427c27518df266943a5308ed92d4b226cc639f5a8f1002816174301\""
        ));
        // Workspace crate: purl present, marked workspace.
        assert!(doc.contains("\"purl\": \"pkg:cargo/rustos-abi@0.0.0\""));
        assert!(doc.contains("\"value\": \"workspace\""));
        // Root component identifies the workspace.
        assert!(doc.contains("\"bom-ref\": \"pkg:cargo/rustos@0.0.0\""));
        // Every parsed package yields exactly one component bom-ref.
        let component_count = doc.matches("\"type\": \"library\"").count();
        assert_eq!(component_count, pkgs.len());
    }

    #[test]
    fn output_is_deterministic() {
        let pkgs = parse_cargo_lock(SAMPLE_LOCK).expect("parse");
        let meta = WorkspaceMeta {
            version: "0.0.0".to_string(),
            repository: None,
        };
        let first = build_cyclonedx(&pkgs, &meta);
        let second = build_cyclonedx(&pkgs, &meta);
        assert_eq!(first, second);
    }

    #[test]
    fn json_escaping_handles_quotes_and_controls() {
        let mut out = String::new();
        json_escape_into(&mut out, "a\"b\\c\n\t");
        assert_eq!(out, "a\\\"b\\\\c\\n\\t");
    }

    #[test]
    fn real_workspace_lock_parses_and_serialises() {
        let mut root = std::path::PathBuf::from(env!("CARGO_MANIFEST_DIR"));
        root.pop(); // tools
        root.pop(); // workspace
        let lock = std::fs::read_to_string(root.join("Cargo.lock")).expect("read Cargo.lock");
        let manifest = std::fs::read_to_string(root.join("Cargo.toml")).expect("read Cargo.toml");
        let pkgs = parse_cargo_lock(&lock).expect("parse real lock");
        assert!(
            pkgs.iter().any(|p| p.name == "rustos-xtask"),
            "workspace crate must appear in the SBOM"
        );
        assert!(
            pkgs.iter()
                .any(|p| p.source_class() == "registry" && p.checksum.is_some()),
            "at least one pinned external crate must appear"
        );
        let meta = parse_workspace_meta(&manifest).expect("real meta");
        let doc = build_cyclonedx(&pkgs, &meta);
        assert!(doc.starts_with("{\n"));
        assert!(doc.ends_with("}\n"));
    }
}
