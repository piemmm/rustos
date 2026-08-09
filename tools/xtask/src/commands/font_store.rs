//! Reading the committed font store under `lib/font/assets/`.
//!
//! One reader serves both consumers of that tree: the image builder plants
//! each family at `/System/Fonts/<key>/` for the `fontd` service, and the
//! console-atlas generator compiles the console family's faces in. Both
//! therefore see exactly the faces a family's `FontFamily` manifest names, in
//! its resolution order, parsed by the same `lib/fontface` parser the service
//! reads the planted copy with — so the shipped store, the service and the
//! compiled-in atlas cannot disagree about what a family is.

use std::path::{Path, PathBuf};

use tairix_abi::font_ipc::FamilyKey;
use tairix_fontface::{FamilyManifest, FAMILY_MANIFEST};

/// Workspace-relative root of the committed font store.
pub const ASSETS_DIR: &str = "lib/font/assets";

/// The family directory the text console is drawn from.
///
/// The console cell grid needs one strictly monospace family, chosen at build
/// time because a kernel console cannot ask a service which to use; this
/// names it. Its faces come from its manifest, so which *faces* the console
/// carries is still recorded in one place only.
pub const CONSOLE_FAMILY: &str = "mono";

/// One family directory of the store, read.
pub struct Family {
    /// The directory name, which is the family key.
    pub key: String,
    /// The manifest text as committed, for planting verbatim.
    pub manifest_text: String,
    /// The face files the manifest names, in resolution order.
    pub faces: Vec<(String, Vec<u8>)>,
}

impl Family {
    /// The face bytes alone, in resolution order.
    pub fn face_bytes(&self) -> Vec<&[u8]> {
        self.faces
            .iter()
            .map(|(_, bytes)| bytes.as_slice())
            .collect()
    }
}

/// Read the family directory `key` of the store rooted at `workspace_root`.
///
/// # Errors
///
/// A string describing a directory with no readable manifest, a manifest the
/// shared parser rejects, a directory name that is not a valid family key, or
/// a face the manifest names that is not there.
pub fn read_family(workspace_root: &Path, key: &str) -> Result<Family, String> {
    read_family_dir(&workspace_root.join(ASSETS_DIR).join(key))
}

/// Read every family directory of the store rooted at `workspace_root`, in
/// key order.
///
/// # Errors
///
/// A string describing a failed read of the store, or any error
/// [`read_family`] reports for one of its directories.
pub fn read_store(workspace_root: &Path) -> Result<Vec<Family>, String> {
    let root = workspace_root.join(ASSETS_DIR);
    let entries = std::fs::read_dir(&root)
        .map_err(|e| format!("font store: reading {}: {e}", root.display()))?;
    let mut dirs: Vec<PathBuf> = Vec::new();
    for entry in entries {
        let path = entry
            .map_err(|e| format!("font store: entry in {}: {e}", root.display()))?
            .path();
        if path.is_dir() {
            dirs.push(path);
        }
    }
    // `read_dir` order is unspecified; sort so every consumer sees one order.
    dirs.sort();
    dirs.iter().map(|dir| read_family_dir(dir)).collect()
}

/// Read the family directory at `dir`.
fn read_family_dir(dir: &Path) -> Result<Family, String> {
    let key = dir
        .file_name()
        .and_then(|name| name.to_str())
        .ok_or_else(|| format!("font store: non-UTF-8 family name in {}", dir.display()))?
        .to_owned();
    let manifest_path = dir.join(FAMILY_MANIFEST);
    let manifest_text = std::fs::read_to_string(&manifest_path).map_err(|e| {
        format!(
            "font store: family {key} has no readable {FAMILY_MANIFEST}: {e} ({})",
            manifest_path.display()
        )
    })?;
    let family_key = FamilyKey::new(&key)
        .map_err(|_| format!("font store: directory {key} is not a valid family key"))?;
    let manifest = FamilyManifest::parse(family_key, &manifest_text)
        .map_err(|e| format!("font store: family {key}: {e}"))?;
    let mut faces = Vec::with_capacity(manifest.faces().len());
    for face in manifest.faces() {
        let path = dir.join(face);
        let bytes = std::fs::read(&path)
            .map_err(|e| format!("font store: face {}: {e}", path.display()))?;
        faces.push((face.clone(), bytes));
    }
    Ok(Family {
        key,
        manifest_text,
        faces,
    })
}
