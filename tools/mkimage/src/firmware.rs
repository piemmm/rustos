//! Pinned, checksummed firmware build inputs.
//!
//! The Raspberry Pi boot blobs are third-party redistributables and are not
//! committed to the tree. `tools/mkimage/firmware.lock` pins each required
//! file's exact byte length and SHA-256; this module parses that manifest
//! and loads the blobs from a caller-supplied directory, refusing — fail
//! closed — any file that is missing, the wrong size, or the wrong hash.
//! There is no "trust the directory" path and mkimage itself performs no
//! network fetch: the manifest also pins the upstream `source` URL so the
//! build orchestrator (`cargo xtask image`) can fetch a missing blob, but
//! every fetched byte still passes this module's pinned-checksum gate
//! before it is used.

use std::fs;
use std::path::Path;

use tairix_crypto::{sha256, SHA256_OUTPUT_LEN};

use crate::MkimageError;

/// The firmware files every bootable Pi 4 image must carry, exactly the
/// set the GPU bootloader reads before the kernel runs. A name is the
/// blob's path on the FAT boot partition, so the `disable-bt` overlay
/// the generated `config.txt` applies (`crate::fatboot::config_txt`)
/// lives under the firmware's fixed `overlays/` directory.
pub const REQUIRED_FIRMWARE: [&str; 4] = [
    "start4.elf",
    "fixup4.dat",
    "bcm2711-rpi-4-b.dtb",
    "overlays/disable-bt.dtbo",
];

/// The optional PSCI secondary-core stub. Allowed in a manifest, never
/// required: the kernel's boot stub parks secondary cores itself, so first
/// boot does not need it (`docs/src/platform/aarch64.md`, "Boot protocol").
pub const OPTIONAL_FIRMWARE: [&str; 1] = ["armstub8.bin"];

/// One pinned manifest entry.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct FirmwareEntry {
    /// Blob file name, also its name on the FAT boot partition.
    pub name: String,
    /// Exact byte length of the pinned blob.
    pub size: u64,
    /// SHA-256 of the pinned blob.
    pub sha256: [u8; SHA256_OUTPUT_LEN],
}

/// The parsed pin manifest (`tools/mkimage/firmware.lock`).
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct FirmwareManifest {
    source: String,
    entries: Vec<FirmwareEntry>,
}

/// One verified firmware blob, ready to be written to the boot partition.
pub struct FirmwareFile {
    /// File name on the FAT boot partition.
    pub name: String,
    /// Verified blob contents.
    pub bytes: Vec<u8>,
}

impl FirmwareManifest {
    /// Parse the manifest text.
    ///
    /// Each non-comment line is `<name> <byte length> <sha256 hex>`, plus
    /// exactly one `source <https url>` directive pinning the upstream
    /// directory the blobs are fetched from. The name set is closed: every
    /// [`REQUIRED_FIRMWARE`] file must be pinned, only
    /// [`OPTIONAL_FIRMWARE`] files may be pinned in addition, and a
    /// duplicate or unknown name is refused.
    ///
    /// # Errors
    ///
    /// [`MkimageError::Manifest`] on any malformed, duplicate, unknown, or
    /// missing-required entry, or a missing, duplicate, or non-HTTPS
    /// `source` directive.
    pub fn parse(text: &str) -> Result<Self, MkimageError> {
        let mut source: Option<String> = None;
        let mut entries: Vec<FirmwareEntry> = Vec::new();
        for line in text.lines() {
            let line = line.trim();
            if line.is_empty() || line.starts_with('#') {
                continue;
            }
            let mut fields = line.split_whitespace();
            if line.starts_with("source ") {
                let (Some(_), Some(url), None) = (fields.next(), fields.next(), fields.next())
                else {
                    return Err(MkimageError::Manifest(format!(
                        "malformed source directive: {line}"
                    )));
                };
                if !url.starts_with("https://") {
                    return Err(MkimageError::Manifest(format!(
                        "source must be an https:// URL: {url}"
                    )));
                }
                if source.is_some() {
                    return Err(MkimageError::Manifest(
                        "duplicate source directive".to_owned(),
                    ));
                }
                source = Some(url.trim_end_matches('/').to_owned());
                continue;
            }
            let (Some(name), Some(size), Some(hash), None) =
                (fields.next(), fields.next(), fields.next(), fields.next())
            else {
                return Err(MkimageError::Manifest(format!(
                    "malformed manifest line: {line}"
                )));
            };
            if !REQUIRED_FIRMWARE.contains(&name) && !OPTIONAL_FIRMWARE.contains(&name) {
                return Err(MkimageError::Manifest(format!(
                    "unknown firmware file in manifest: {name}"
                )));
            }
            if entries.iter().any(|e| e.name == name) {
                return Err(MkimageError::Manifest(format!(
                    "duplicate manifest entry: {name}"
                )));
            }
            let size: u64 = size
                .parse()
                .map_err(|_| MkimageError::Manifest(format!("invalid byte length for {name}")))?;
            let sha256 = parse_sha256(hash)
                .ok_or_else(|| MkimageError::Manifest(format!("invalid sha256 for {name}")))?;
            entries.push(FirmwareEntry {
                name: name.to_owned(),
                size,
                sha256,
            });
        }
        for required in REQUIRED_FIRMWARE {
            if !entries.iter().any(|e| e.name == required) {
                return Err(MkimageError::Manifest(format!(
                    "manifest does not pin required firmware file {required}"
                )));
            }
        }
        let Some(source) = source else {
            return Err(MkimageError::Manifest(
                "manifest does not pin the upstream source URL".to_owned(),
            ));
        };
        Ok(Self { source, entries })
    }

    /// The pinned upstream HTTPS directory the blobs are fetched from
    /// (no trailing slash); a blob's URL is `<source>/<name>`.
    #[must_use]
    pub fn source(&self) -> &str {
        &self.source
    }

    /// The pinned entries whose staged file under `dir` is absent,
    /// unreadable, the wrong length, or the wrong SHA-256 — exactly the
    /// set a fetch must (re)download before [`Self::load_dir`] can pass.
    #[must_use]
    pub fn missing_in(&self, dir: &Path) -> Vec<&FirmwareEntry> {
        self.entries
            .iter()
            .filter(|entry| {
                !fs::read(dir.join(&entry.name)).is_ok_and(|bytes| {
                    bytes.len() as u64 == entry.size && sha256(&bytes) == entry.sha256
                })
            })
            .collect()
    }

    /// Whether the manifest pins `name`.
    #[must_use]
    pub fn pins(&self, name: &str) -> bool {
        self.entries.iter().any(|e| e.name == name)
    }

    /// Load and verify every pinned blob from `dir`.
    ///
    /// # Errors
    ///
    /// [`MkimageError::Firmware`] if a pinned file is missing, unreadable,
    /// the wrong length, or the wrong SHA-256.
    pub fn load_dir(&self, dir: &Path) -> Result<Vec<FirmwareFile>, MkimageError> {
        let mut files = Vec::with_capacity(self.entries.len());
        for entry in &self.entries {
            let path = dir.join(&entry.name);
            let bytes = fs::read(&path).map_err(|e| {
                MkimageError::Firmware(format!(
                    "cannot read pinned firmware file {}: {e}",
                    path.display()
                ))
            })?;
            if bytes.len() as u64 != entry.size {
                return Err(MkimageError::Firmware(format!(
                    "{}: pinned length {} but found {} bytes",
                    entry.name,
                    entry.size,
                    bytes.len()
                )));
            }
            if sha256(&bytes) != entry.sha256 {
                return Err(MkimageError::Firmware(format!(
                    "{}: SHA-256 does not match the pinned checksum",
                    entry.name
                )));
            }
            files.push(FirmwareFile {
                name: entry.name.clone(),
                bytes,
            });
        }
        Ok(files)
    }
}

/// Decode a 64-character lowercase/uppercase hex SHA-256.
fn parse_sha256(hex: &str) -> Option<[u8; SHA256_OUTPUT_LEN]> {
    let hex = hex.as_bytes();
    if hex.len() != SHA256_OUTPUT_LEN * 2 {
        return None;
    }
    let mut out = [0u8; SHA256_OUTPUT_LEN];
    for (i, pair) in hex.chunks_exact(2).enumerate() {
        let hi = (pair[0] as char).to_digit(16)?;
        let lo = (pair[1] as char).to_digit(16)?;
        out[i] = u8::try_from(hi * 16 + lo).ok()?;
    }
    Some(out)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Build a valid manifest text over the given `(name, bytes)` blobs.
    fn manifest_for(blobs: &[(&str, &[u8])]) -> String {
        use std::fmt::Write as _;

        let mut text = String::from("# test manifest\nsource https://firmware.example/boot/\n");
        for (name, bytes) in blobs {
            let _ = write!(text, "{name} {} ", bytes.len());
            for byte in sha256(bytes) {
                let _ = write!(text, "{byte:02x}");
            }
            text.push('\n');
        }
        text
    }

    /// The four required blobs with distinct test contents.
    fn test_blobs() -> Vec<(&'static str, &'static [u8])> {
        vec![
            ("start4.elf", b"start4 contents".as_slice()),
            ("fixup4.dat", b"fixup4 contents".as_slice()),
            ("bcm2711-rpi-4-b.dtb", b"dtb contents".as_slice()),
            ("overlays/disable-bt.dtbo", b"overlay contents".as_slice()),
        ]
    }

    /// Write `blobs` into a fresh unique temp dir and return its path.
    fn stage(blobs: &[(&str, &[u8])], tag: &str) -> std::path::PathBuf {
        let dir =
            std::env::temp_dir().join(format!("tairix-mkimage-fw-{tag}-{}", std::process::id()));
        fs::create_dir_all(&dir).expect("create temp firmware dir");
        for (name, bytes) in blobs {
            let path = dir.join(name);
            if let Some(parent) = path.parent() {
                fs::create_dir_all(parent).expect("create staged subdirectory");
            }
            fs::write(path, bytes).expect("stage blob");
        }
        dir
    }

    #[test]
    fn parses_and_verifies_a_staged_directory() {
        let blobs = test_blobs();
        let manifest = FirmwareManifest::parse(&manifest_for(&blobs)).expect("manifest parses");
        let dir = stage(&blobs, "ok");
        let files = manifest.load_dir(&dir).expect("blobs verify");
        assert_eq!(files.len(), 4);
        assert!(files.iter().any(|f| f.name == "start4.elf"));
        assert!(files.iter().any(|f| f.name == "overlays/disable-bt.dtbo"));
        fs::remove_dir_all(dir).expect("cleanup");
    }

    #[test]
    fn the_committed_manifest_parses_and_pins_the_required_set() {
        let text = include_str!("../firmware.lock");
        let manifest = FirmwareManifest::parse(text).expect("committed manifest parses");
        for required in REQUIRED_FIRMWARE {
            assert!(manifest.pins(required));
        }
        assert!(!manifest.pins("armstub8.bin"));
        assert!(manifest.source().starts_with("https://"));
        assert!(!manifest.source().ends_with('/'));
    }

    #[test]
    fn parses_the_source_directive_without_a_trailing_slash() {
        let manifest =
            FirmwareManifest::parse(&manifest_for(&test_blobs())).expect("manifest parses");
        assert_eq!(manifest.source(), "https://firmware.example/boot");
    }

    #[test]
    fn rejects_a_missing_duplicate_malformed_or_non_https_source() {
        let blobs = test_blobs();
        let without_source = manifest_for(&blobs)
            .lines()
            .filter(|l| !l.starts_with("source "))
            .collect::<Vec<_>>()
            .join("\n");
        assert!(matches!(
            FirmwareManifest::parse(&without_source),
            Err(MkimageError::Manifest(_))
        ));

        let duplicate = format!(
            "source https://firmware.example/dup\n{}",
            manifest_for(&blobs)
        );
        assert!(matches!(
            FirmwareManifest::parse(&duplicate),
            Err(MkimageError::Manifest(_))
        ));

        let malformed = format!("source https://a.example b\n{without_source}");
        assert!(matches!(
            FirmwareManifest::parse(&malformed),
            Err(MkimageError::Manifest(_))
        ));

        let plaintext = format!("source http://firmware.example/boot\n{without_source}");
        assert!(matches!(
            FirmwareManifest::parse(&plaintext),
            Err(MkimageError::Manifest(_))
        ));
    }

    #[test]
    fn missing_in_reports_absent_corrupt_and_short_blobs_only() {
        let blobs = test_blobs();
        let manifest = FirmwareManifest::parse(&manifest_for(&blobs)).expect("manifest parses");

        let empty = stage(&[], "missing-all");
        let missing = manifest.missing_in(&empty);
        assert_eq!(missing.len(), 4);
        fs::remove_dir_all(empty).expect("cleanup");

        let staged = stage(&blobs, "missing-none");
        assert!(manifest.missing_in(&staged).is_empty());
        fs::write(staged.join("fixup4.dat"), b"tampered bytes!").expect("tamper");
        let missing = manifest.missing_in(&staged);
        assert_eq!(missing.len(), 1);
        assert_eq!(missing[0].name, "fixup4.dat");
        fs::remove_dir_all(staged).expect("cleanup");
    }

    #[test]
    fn rejects_missing_required_entry() {
        let blobs = test_blobs();
        let text = manifest_for(&blobs[..2]);
        assert!(matches!(
            FirmwareManifest::parse(&text),
            Err(MkimageError::Manifest(_))
        ));
    }

    #[test]
    fn rejects_unknown_duplicate_and_malformed_lines() {
        let mut blobs = test_blobs();
        blobs.push(("evil.bin", b"nope".as_slice()));
        assert!(matches!(
            FirmwareManifest::parse(&manifest_for(&blobs)),
            Err(MkimageError::Manifest(_))
        ));

        let blobs = test_blobs();
        let mut dup = manifest_for(&blobs);
        dup.push_str(&manifest_for(&blobs[..1]));
        assert!(matches!(
            FirmwareManifest::parse(&dup),
            Err(MkimageError::Manifest(_))
        ));

        assert!(matches!(
            FirmwareManifest::parse("start4.elf not-a-number deadbeef\n"),
            Err(MkimageError::Manifest(_))
        ));
        assert!(matches!(
            FirmwareManifest::parse("start4.elf 4\n"),
            Err(MkimageError::Manifest(_))
        ));
    }

    #[test]
    fn refuses_a_wrong_hash_wrong_size_or_missing_blob() {
        let blobs = test_blobs();
        let manifest = FirmwareManifest::parse(&manifest_for(&blobs)).expect("manifest parses");

        let tampered: Vec<(&str, &[u8])> = vec![
            ("start4.elf", b"start4 contents".as_slice()),
            ("fixup4.dat", b"tampered bytes!".as_slice()),
            ("bcm2711-rpi-4-b.dtb", b"dtb contents".as_slice()),
            ("overlays/disable-bt.dtbo", b"overlay contents".as_slice()),
        ];
        let dir = stage(&tampered, "hash");
        assert!(matches!(
            manifest.load_dir(&dir),
            Err(MkimageError::Firmware(_))
        ));
        fs::remove_dir_all(dir).expect("cleanup");

        let short: Vec<(&str, &[u8])> = vec![
            ("start4.elf", b"start4 contents".as_slice()),
            ("fixup4.dat", b"short".as_slice()),
            ("bcm2711-rpi-4-b.dtb", b"dtb contents".as_slice()),
            ("overlays/disable-bt.dtbo", b"overlay contents".as_slice()),
        ];
        let dir = stage(&short, "size");
        assert!(matches!(
            manifest.load_dir(&dir),
            Err(MkimageError::Firmware(_))
        ));
        fs::remove_dir_all(dir).expect("cleanup");

        let dir = stage(&blobs[..2], "missing");
        assert!(matches!(
            manifest.load_dir(&dir),
            Err(MkimageError::Firmware(_))
        ));
        fs::remove_dir_all(dir).expect("cleanup");
    }
}
