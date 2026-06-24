//! Absolute-path parsing and the reserved-name policy.
//!
//! A [`Path`] is an *absolute*, already-normalised sequence of name
//! components. Parsing rejects relative paths, empty components, and the
//! `.`/`..` traversal tokens outright: the VFS never resolves a path that
//! could escape the tree, so there is no traversal logic to get wrong.
//!
//! The reserved-name policy is data, not control flow: [`RESERVED_TOP_LEVEL`]
//! lists every legacy POSIX top-level directory the charter forbids,
//! and [`ROOT_TEMPLATE`] lists the only four top-level directories the
//! installer lays out. Both are consulted by the VFS in `super::vfs`.

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

/// Legacy POSIX top-level directory names that are **reserved and
/// forbidden** as top-level directories.
///
/// The VFS refuses to create any of these directly under the root.
pub const RESERVED_TOP_LEVEL: [&str; 18] = [
    "etc", "home", "usr", "var", "proc", "sys", "lib", "lib64", "bin", "sbin", "opt", "root",
    "tmp", "dev", "mnt", "media", "run", "boot",
];

/// The only four top-level directories RustOS has.
///
/// The default root template ([`super::Vfs::with_default_layout`])
/// provides exactly these and nothing else.
pub const ROOT_TEMPLATE: [&str; 4] = ["System", "Users", "Apps", "Storage"];

/// `true` if `name` is a reserved legacy POSIX top-level directory name.
#[must_use]
pub fn is_reserved_top_level(name: &str) -> bool {
    RESERVED_TOP_LEVEL.contains(&name)
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
    fn reserved_names_match_agents_md() {
        for name in RESERVED_TOP_LEVEL {
            assert!(is_reserved_top_level(name), "{name} must be reserved");
        }
        for name in ROOT_TEMPLATE {
            assert!(!is_reserved_top_level(name), "{name} must be allowed");
        }
    }
}
