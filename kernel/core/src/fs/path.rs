//! Absolute-path parsing and the top-level layout names.
//!
//! A [`Path`] is an *absolute*, already-normalised sequence of name
//! components. Parsing rejects relative paths, empty components, and the
//! `.`/`..` traversal tokens outright: the VFS never resolves a path that
//! could escape the tree, so there is no traversal logic to get wrong.
//!
//! [`ROOT_TEMPLATE`] lists the only four top-level directories the OS lays
//! out; it is data, not control flow.

use alloc::string::{String, ToString};
use alloc::vec::Vec;

use super::VfsError;

/// Maximum bytes in a single path component.
///
/// Matches the long-standing Unix `NAME_MAX`; bounding it keeps a hostile
/// on-disk record from forcing an unbounded component allocation.
pub const MAX_COMPONENT_LEN: usize = 255;

/// Maximum number of components in a single absolute path.
///
/// Bounds resolution work and stack depth. Raising it is a reviewed change
/// here, not a per-call override.
pub const MAX_PATH_COMPONENTS: usize = 64;

/// The only four top-level directories TAIRiX has.
///
/// The default root template ([`super::Vfs::with_default_layout`])
/// provides exactly these and nothing else.
pub const ROOT_TEMPLATE: [&str; 4] = ["System", "Users", "Apps", "Storage"];

/// Resolve a machine-alias name to the top-level view component it roots at.
///
/// TAIRiX storage is a forest of named roots (`docs/src/filesystem/drives.md`):
/// a path names its root explicitly, and `System:` is a canonical first-class
/// root of which `/System` is merely the synthetic-view *projection*. The four
/// **machine aliases** are therefore exactly [`ROOT_TEMPLATE`] — one
/// definition, so the view template and the alias namespace can never drift
/// apart. `System:/Kernel/x` and `/System/Kernel/x` name the same object.
///
/// Session and volume aliases are published by their owning services when
/// those land, never invented ahead of a live publisher, so an unknown name
/// resolves to `None` and the caller fails closed.
#[must_use]
pub fn resolve_machine_alias(name: &str) -> Option<&'static str> {
    ROOT_TEMPLATE.iter().copied().find(|root| *root == name)
}

/// An absolute, normalised filesystem path.
///
/// Invariants, established at [`Path::parse`] and preserved by every
/// method:
///
/// * every component is a non-empty name of at most [`MAX_COMPONENT_LEN`]
///   bytes containing neither `/` nor a NUL byte,
/// * no component is `.` or `..`,
/// * there are at most [`MAX_PATH_COMPONENTS`] components.
///
/// The empty component list is the root (`/`).
#[derive(Clone, Debug, Eq, PartialEq, Hash, Default)]
pub struct Path {
    components: Vec<String>,
}

impl Path {
    /// The root path (`/`).
    #[must_use]
    pub fn root() -> Self {
        Self {
            components: Vec::new(),
        }
    }

    /// Parse an absolute path string into a normalised [`Path`].
    ///
    /// # Errors
    ///
    /// Returns [`VfsError::InvalidPath`] if `text` is not absolute, has an
    /// empty/over-long component, contains a `.`/`..`/NUL token, or has
    /// more than [`MAX_PATH_COMPONENTS`] components.
    pub fn parse(text: &str) -> Result<Self, VfsError> {
        let Some(rest) = text.strip_prefix('/') else {
            return Err(VfsError::InvalidPath);
        };
        let mut components = Vec::new();
        for raw in rest.split('/') {
            if raw.is_empty() {
                // A trailing slash or a `//` run collapses to nothing; an
                // interior empty segment is the same as a redundant slash.
                continue;
            }
            validate_component(raw)?;
            if components.len() == MAX_PATH_COMPONENTS {
                return Err(VfsError::InvalidPath);
            }
            components.push(raw.to_string());
        }
        Ok(Self { components })
    }

    /// The path's components, root-first. Empty for the root path.
    #[must_use]
    pub fn components(&self) -> &[String] {
        &self.components
    }

    /// `true` if this is the root path (`/`).
    #[must_use]
    pub fn is_root(&self) -> bool {
        self.components.is_empty()
    }

    /// Number of components (zero for the root).
    #[must_use]
    pub fn depth(&self) -> usize {
        self.components.len()
    }

    /// The first (top-level) component, or `None` for the root.
    #[must_use]
    pub fn top_level(&self) -> Option<&str> {
        self.components.first().map(String::as_str)
    }

    /// The final component (the file or directory name), or `None` for the
    /// root.
    #[must_use]
    pub fn file_name(&self) -> Option<&str> {
        self.components.last().map(String::as_str)
    }

    /// The parent path, or `None` for the root.
    #[must_use]
    pub fn parent(&self) -> Option<Path> {
        if self.components.is_empty() {
            return None;
        }
        let mut components = self.components.clone();
        components.pop();
        Some(Self { components })
    }

    /// `true` if `self` is a prefix of (or equal to) `other`, comparing
    /// component-by-component. The root is a prefix of every path.
    #[must_use]
    pub fn is_prefix_of(&self, other: &Path) -> bool {
        if self.components.len() > other.components.len() {
            return false;
        }
        self.components
            .iter()
            .zip(other.components.iter())
            .all(|(a, b)| a == b)
    }
}

/// Validate a single path component against the [`Path`] invariants.
fn validate_component(name: &str) -> Result<(), VfsError> {
    if name == "." || name == ".." {
        return Err(VfsError::InvalidPath);
    }
    if name.len() > MAX_COMPONENT_LEN {
        return Err(VfsError::InvalidPath);
    }
    if name.bytes().any(|b| b == 0 || b == b'/') {
        return Err(VfsError::InvalidPath);
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn root_parses_and_is_root() {
        let p = Path::parse("/").expect("root parses");
        assert!(p.is_root());
        assert_eq!(p.depth(), 0);
        assert_eq!(p.top_level(), None);
        assert_eq!(p.file_name(), None);
        assert_eq!(p.parent(), None);
    }

    #[test]
    fn redundant_slashes_collapse() {
        let p = Path::parse("//System///Logs/").expect("collapses");
        assert_eq!(p.components(), &["System".to_string(), "Logs".to_string()]);
        assert_eq!(p.top_level(), Some("System"));
        assert_eq!(p.file_name(), Some("Logs"));
    }

    #[test]
    fn relative_path_is_rejected() {
        assert_eq!(Path::parse("System/x"), Err(VfsError::InvalidPath));
        assert_eq!(Path::parse(""), Err(VfsError::InvalidPath));
    }

    #[test]
    fn dot_and_dotdot_are_rejected() {
        assert_eq!(Path::parse("/System/.."), Err(VfsError::InvalidPath));
        assert_eq!(Path::parse("/./System"), Err(VfsError::InvalidPath));
    }

    #[test]
    fn nul_byte_component_is_rejected() {
        assert_eq!(Path::parse("/Sys\0tem"), Err(VfsError::InvalidPath));
    }

    #[test]
    fn over_long_component_is_rejected() {
        let mut s = String::from("/");
        s.push_str(&"a".repeat(MAX_COMPONENT_LEN + 1));
        assert_eq!(Path::parse(&s), Err(VfsError::InvalidPath));
    }

    #[test]
    fn too_many_components_is_rejected() {
        let mut s = String::new();
        for _ in 0..=MAX_PATH_COMPONENTS {
            s.push_str("/a");
        }
        assert_eq!(Path::parse(&s), Err(VfsError::InvalidPath));
    }

    #[test]
    fn parent_drops_the_last_component() {
        let p = Path::parse("/System/Logs/boot").expect("parses");
        let parent = p.parent().expect("has parent");
        assert_eq!(parent, Path::parse("/System/Logs").expect("parses"));
    }

    #[test]
    fn prefix_is_component_wise() {
        let sys = Path::parse("/System").expect("parses");
        let logs = Path::parse("/System/Logs").expect("parses");
        let syslike = Path::parse("/SystemX").expect("parses");
        assert!(sys.is_prefix_of(&logs));
        assert!(Path::root().is_prefix_of(&logs));
        assert!(!logs.is_prefix_of(&sys));
        assert!(!sys.is_prefix_of(&syslike));
    }

    #[test]
    fn every_machine_alias_roots_at_its_own_top_level_component() {
        for name in ROOT_TEMPLATE {
            assert_eq!(resolve_machine_alias(name), Some(name));
        }
    }

    #[test]
    fn unknown_machine_alias_fails_closed() {
        assert_eq!(resolve_machine_alias("Home"), None);
        assert_eq!(resolve_machine_alias("system"), None);
        assert_eq!(resolve_machine_alias(""), None);
        assert_eq!(resolve_machine_alias("Storage2"), None);
    }
}
