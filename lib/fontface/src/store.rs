//! The on-disk font store: the `FontFamily` manifest every family directory
//! under `/System/Fonts` carries, and the rules a scan of that store reads it
//! by.
//!
//! A directory *is* a family exactly when it holds a [`FAMILY_MANIFEST`]
//! file, so shipping a family is dropping its directory into the store and
//! nothing anywhere names a face. The image builder plants the same
//! directories from `lib/font/assets/`, and both sides read them through this
//! one parser, so a manifest the service accepts is a manifest the build
//! accepts.
//!
//! # Coverage is layered by order alone
//!
//! A family lists its faces in resolution order and may name one fallback
//! family whose faces extend it. A scalar resolves to the first face whose
//! `cmap` maps it — the primary face owns Latin, and a companion is reached
//! only for what the primary does not map. There is no per-face script
//! scoping to keep in sync with the faces themselves.

use alloc::string::{String, ToString};
use alloc::vec::Vec;

use tairix_abi::font_ipc::{FamilyKey, FamilyKind, FONT_FAMILY_LABEL_LEN};

use crate::FontError;

/// The file name that marks a directory under `/System/Fonts` as a family and
/// describes it.
pub const FAMILY_MANIFEST: &str = "FontFamily";

/// Most faces one family may list.
///
/// A family is a primary face plus a handful of script companions; a
/// manifest listing more is malformed rather than ambitious. A validation
/// bound, not a capacity.
pub const MAX_FACES: usize = 8;

/// Largest `FontFamily` manifest, in bytes, that will be read.
///
/// The manifest is a handful of short lines, so anything larger is a
/// corrupt or hostile file and is refused before it is parsed. A validation
/// bound, not a capacity.
pub const MAX_MANIFEST_BYTES: usize = 4096;

/// Longest face file name a manifest may name.
const MAX_FACE_NAME: usize = 64;

/// The extension every face file carries. The engine decodes TrueType
/// outlines only, so a manifest cannot name a container it could not read.
const FACE_EXTENSION: &str = ".ttf";

/// What a family is for.
///
/// A [`Selectable`](Self::Selectable) family is offered to the user and can
/// be named by a theme; a [`Fallback`](Self::Fallback) family exists only to
/// extend another's coverage and is never offered on its own, so the shared
/// Hebrew and CJK faces are stored once without appearing in a font picker.
#[derive(Copy, Clone, Debug, Eq, PartialEq)]
pub enum FamilyRole {
    /// A family a user may choose, laid out as the given kind.
    Selectable(FamilyKind),
    /// A coverage-only family other families name as their fallback.
    Fallback,
}

/// One family's parsed `FontFamily` manifest.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct FamilyManifest {
    key: FamilyKey,
    label: String,
    role: FamilyRole,
    faces: Vec<String>,
    fallback: Option<FamilyKey>,
}

impl FamilyManifest {
    /// Parse the manifest `text` of the family directory named `key`.
    ///
    /// # Errors
    ///
    /// A [`FontError`] when the text is over [`MAX_MANIFEST_BYTES`], carries
    /// a line that is neither blank, a `#` comment, nor `key = value`, names
    /// an unknown key or an unknown `kind`, repeats a single-valued key,
    /// lists no face or more than [`MAX_FACES`], names a face file that is
    /// not a plain `.ttf` name in this directory, names itself as its own
    /// fallback, or omits `label` or `kind`.
    pub fn parse(key: FamilyKey, text: &str) -> Result<Self, FontError> {
        if text.len() > MAX_MANIFEST_BYTES {
            return Err(FontError::new("font family manifest is too large"));
        }
        let mut label = None;
        let mut role = None;
        let mut faces = Vec::new();
        let mut fallback = None;
        for line in text.lines() {
            let line = line.trim();
            if line.is_empty() || line.starts_with('#') {
                continue;
            }
            let (field, value) = line.split_once('=').ok_or(FontError::new(
                "font family manifest line is not key = value",
            ))?;
            let value = value.trim();
            match field.trim() {
                "label" => set_once(&mut label, validate_label(value)?.to_string())?,
                "kind" => set_once(&mut role, parse_role(value)?)?,
                "face" => {
                    if faces.len() == MAX_FACES {
                        return Err(FontError::new("font family lists too many faces"));
                    }
                    faces.push(validate_face(value)?.to_string());
                }
                "fallback" => {
                    let named = FamilyKey::new(value)
                        .map_err(|_| FontError::new("fallback names no valid family"))?;
                    if named == key {
                        return Err(FontError::new("font family falls back to itself"));
                    }
                    set_once(&mut fallback, named)?;
                }
                _ => return Err(FontError::new("unknown font family manifest key")),
            }
        }
        if faces.is_empty() {
            return Err(FontError::new("font family lists no face"));
        }
        Ok(Self {
            key,
            label: label.ok_or(FontError::new("font family has no label"))?,
            role: role.ok_or(FontError::new("font family has no kind"))?,
            faces,
            fallback,
        })
    }

    /// The key naming this family, which is its directory name.
    #[must_use]
    pub const fn key(&self) -> FamilyKey {
        self.key
    }

    /// The label a font picker shows.
    #[must_use]
    pub fn label(&self) -> &str {
        &self.label
    }

    /// What the family is for.
    #[must_use]
    pub const fn role(&self) -> FamilyRole {
        self.role
    }

    /// The face file names, in resolution order.
    #[must_use]
    pub fn faces(&self) -> &[String] {
        &self.faces
    }

    /// The family whose faces extend this one's coverage, if any.
    #[must_use]
    pub const fn fallback(&self) -> Option<FamilyKey> {
        self.fallback
    }

    /// How this family lays text out, or `None` when it is a fallback set a
    /// user never chooses.
    #[must_use]
    pub const fn selectable_kind(&self) -> Option<FamilyKind> {
        match self.role {
            FamilyRole::Selectable(kind) => Some(kind),
            FamilyRole::Fallback => None,
        }
    }
}

/// Record `value` in `slot`, refusing a key the manifest states twice — a
/// second spelling would silently win over the first.
fn set_once<T>(slot: &mut Option<T>, value: T) -> Result<(), FontError> {
    if slot.is_some() {
        return Err(FontError::new("font family manifest repeats a key"));
    }
    *slot = Some(value);
    Ok(())
}

/// The role `value` names.
fn parse_role(value: &str) -> Result<FamilyRole, FontError> {
    match value {
        "proportional" => Ok(FamilyRole::Selectable(FamilyKind::Proportional)),
        "monospace" => Ok(FamilyRole::Selectable(FamilyKind::Monospace)),
        "fallback" => Ok(FamilyRole::Fallback),
        _ => Err(FontError::new("unknown font family kind")),
    }
}

/// A label a picker can draw: non-empty, within the wire field, and free of
/// control bytes.
fn validate_label(value: &str) -> Result<&str, FontError> {
    if value.is_empty() || value.len() > FONT_FAMILY_LABEL_LEN {
        return Err(FontError::new("font family label has no usable length"));
    }
    if value.bytes().any(|byte| byte < 0x20 || byte == 0x7F) {
        return Err(FontError::new("font family label carries a control byte"));
    }
    Ok(value)
}

/// A face file name that can only ever name a TrueType file *inside* the
/// family's own directory: no separator, no parent reference, no hidden
/// name, and no other container format.
fn validate_face(value: &str) -> Result<&str, FontError> {
    if value.len() <= FACE_EXTENSION.len() || value.len() > MAX_FACE_NAME {
        return Err(FontError::new("face file name has no usable length"));
    }
    if !value.ends_with(FACE_EXTENSION) {
        return Err(FontError::new("face file is not a TrueType file"));
    }
    if value.starts_with('.') {
        return Err(FontError::new("face file name is hidden"));
    }
    let spelling_is_plain = value
        .bytes()
        .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'-' | b'_' | b'.'));
    if !spelling_is_plain || value.contains("..") {
        return Err(FontError::new("face file name is not a plain file name"));
    }
    Ok(value)
}

#[cfg(test)]
mod tests {
    use super::{FamilyManifest, FamilyRole, MAX_FACES, MAX_MANIFEST_BYTES};
    use alloc::format;
    use alloc::string::String;
    use tairix_abi::font_ipc::{FamilyKey, FamilyKind};

    /// The key a test names a family by.
    fn key(name: &str) -> FamilyKey {
        FamilyKey::new(name).expect("a well-formed family key")
    }

    /// Parse `text` as the manifest of the family `name`.
    fn parse(name: &str, text: &str) -> Result<FamilyManifest, crate::FontError> {
        FamilyManifest::parse(key(name), text)
    }

    #[test]
    fn a_proportional_family_carries_its_faces_in_order_and_its_fallback() {
        let manifest = parse(
            "inter",
            "# The desktop UI face.\nlabel = Inter\nkind = proportional\n\
             face = Inter-Variable.ttf\nfallback = sans-fallback\n",
        )
        .expect("parses");

        assert_eq!(manifest.key(), key("inter"));
        assert_eq!(manifest.label(), "Inter");
        assert_eq!(
            manifest.selectable_kind(),
            Some(FamilyKind::Proportional),
            "a proportional family is offered to the user"
        );
        assert_eq!(manifest.faces(), ["Inter-Variable.ttf"]);
        assert_eq!(manifest.fallback(), Some(key("sans-fallback")));
    }

    #[test]
    fn a_fallback_family_is_never_offered_to_a_user() {
        let manifest = parse(
            "sans-fallback",
            "label = Noto Sans fallback\nkind = fallback\n\
             face = NotoSansHebrew-Variable.ttf\nface = NotoSansSC-Variable.ttf\n",
        )
        .expect("parses");

        assert_eq!(manifest.role(), FamilyRole::Fallback);
        assert_eq!(manifest.selectable_kind(), None);
        // Face order is the resolution order, so it must survive verbatim.
        assert_eq!(
            manifest.faces(),
            ["NotoSansHebrew-Variable.ttf", "NotoSansSC-Variable.ttf"]
        );
        assert_eq!(manifest.fallback(), None);
    }

    #[test]
    fn the_shipped_manifests_parse() {
        for (name, text) in [
            ("inter", include_str!("../../font/assets/inter/FontFamily")),
            (
                "noto-sans",
                include_str!("../../font/assets/noto-sans/FontFamily"),
            ),
            (
                "noto-serif",
                include_str!("../../font/assets/noto-serif/FontFamily"),
            ),
            ("mono", include_str!("../../font/assets/mono/FontFamily")),
            (
                "sans-fallback",
                include_str!("../../font/assets/sans-fallback/FontFamily"),
            ),
        ] {
            let manifest = parse(name, text).unwrap_or_else(|e| panic!("{name}: {e:?}"));
            assert!(!manifest.faces().is_empty(), "{name} lists no face");
        }
        let mono = parse("mono", include_str!("../../font/assets/mono/FontFamily"))
            .expect("the console family parses");
        assert_eq!(mono.selectable_kind(), Some(FamilyKind::Monospace));
        assert_eq!(
            mono.fallback(),
            None,
            "the console family is self-contained"
        );
    }

    #[test]
    fn a_malformed_manifest_is_refused_rather_than_half_read() {
        let cases = [
            ("no label", "kind = proportional\nface = A.ttf\n"),
            ("no kind", "label = A\nface = A.ttf\n"),
            ("no face", "label = A\nkind = proportional\n"),
            (
                "unknown key",
                "label = A\nkind = proportional\nface = A.ttf\nsize = 3\n",
            ),
            ("unknown kind", "label = A\nkind = cursive\nface = A.ttf\n"),
            (
                "not a pair",
                "label = A\nkind = proportional\nface = A.ttf\nnonsense\n",
            ),
            (
                "repeated label",
                "label = A\nlabel = B\nkind = proportional\nface = A.ttf\n",
            ),
            (
                "self fallback",
                "label = A\nkind = proportional\nface = A.ttf\nfallback = inter\n",
            ),
            (
                "bad fallback key",
                "label = A\nkind = proportional\nface = A.ttf\nfallback = ../mono\n",
            ),
            (
                "empty label",
                "label =\nkind = proportional\nface = A.ttf\n",
            ),
        ];
        for (what, text) in cases {
            assert!(parse("inter", text).is_err(), "{what} must be refused");
        }
    }

    #[test]
    fn a_face_name_can_never_escape_its_family_directory() {
        for name in [
            "../mono/Inconsolata-EX.ttf",
            "sub/dir/Face.ttf",
            ".hidden.ttf",
            "Face.otf",
            "Face.ttf.exe",
            ".ttf",
            "Fa ce.ttf",
            "Fac\u{e9}.ttf",
        ] {
            let text = format!("label = A\nkind = proportional\nface = {name}\n");
            assert!(parse("inter", &text).is_err(), "{name} must be refused");
        }
    }

    #[test]
    fn a_manifest_is_bounded_in_faces_and_bytes() {
        use core::fmt::Write as _;
        let mut text = String::from("label = A\nkind = proportional\n");
        for i in 0..=MAX_FACES {
            writeln!(text, "face = Face{i}.ttf").expect("write to String");
        }
        assert!(parse("inter", &text).is_err(), "too many faces");

        let padded = format!(
            "label = A\nkind = proportional\nface = A.ttf\n#{}\n",
            "x".repeat(MAX_MANIFEST_BYTES)
        );
        assert!(parse("inter", &padded).is_err(), "an oversized manifest");
    }
}
