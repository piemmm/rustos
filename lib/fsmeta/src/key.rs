//! The namespaced-key grammar.
//!
//! An attribute key is `namespace "." rest`, compared byte-for-byte
//! (case-sensitive, matching `RustFS` directory-name comparison). The namespace
//! is drawn from a **closed, curated set** — an unknown namespace is rejected
//! at set time (fail closed), never stored. Each namespace carries a fixed
//! [`NamespaceAccess`] class that says whether reading or writing its
//! attributes needs only the file's own read/write permission or a dedicated
//! capability the VFS enforces.

use alloc::vec::Vec;

use crate::{MetadataError, KEY_MAX};

/// The access class a namespace's attributes fall under. The VFS consults this
/// before delegating an attribute operation to a driver; the driver itself
/// makes no permission decision.
#[derive(Copy, Clone, Debug, Eq, PartialEq)]
pub enum NamespaceAccess {
    /// Governed entirely by the file's own owner/mode/ACL: reading or writing
    /// the attribute needs only the file's read or write permission, with no
    /// additional capability. This is ordinary file metadata.
    FilePermission,
    /// Guards a real security boundary: reading or writing the attribute
    /// requires a dedicated capability the VFS enforces before delegating.
    /// Introduced with its enforcement point, never ahead of it.
    Privileged,
}

/// The closed, curated set of attribute namespaces.
///
/// The set is evolved in place (a namespace is renamed, merged, or removed as
/// a deliberate change), never opened up: a key whose namespace is not one of
/// these is refused.
#[derive(Copy, Clone, Debug, Eq, PartialEq, Hash)]
pub enum Namespace {
    /// Free-form user metadata. File-permission access.
    User,
    /// Acorn / RISC OS (ADFS, `FileCore`) preset metadata. File-permission
    /// access.
    Acorn,
    /// `AmigaDOS` preset metadata. File-permission access.
    Amiga,
    /// Atari GEMDOS / TOS preset metadata. File-permission access.
    Atari,
    /// Classic Mac OS / HFS preset metadata. File-permission access.
    Mac,
    /// RustOS-native extended metadata. File-permission access.
    Rustos,
    /// Security-sensitive, ACL-adjacent metadata. Privileged access.
    System,
    /// Metadata only privileged services may set. Privileged access.
    Trusted,
}

impl Namespace {
    /// Every namespace, in a stable order.
    pub const ALL: [Namespace; 8] = [
        Namespace::User,
        Namespace::Acorn,
        Namespace::Amiga,
        Namespace::Atari,
        Namespace::Mac,
        Namespace::Rustos,
        Namespace::System,
        Namespace::Trusted,
    ];

    /// The namespace's on-disk / on-wire name (the text before the `.`).
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Namespace::User => "user",
            Namespace::Acorn => "acorn",
            Namespace::Amiga => "amiga",
            Namespace::Atari => "atari",
            Namespace::Mac => "mac",
            Namespace::Rustos => "rustos",
            Namespace::System => "system",
            Namespace::Trusted => "trusted",
        }
    }

    /// Resolve a namespace name to a [`Namespace`], or `None` if it is not one
    /// of the closed set.
    #[must_use]
    pub fn from_name(name: &str) -> Option<Namespace> {
        Namespace::ALL.into_iter().find(|ns| ns.as_str() == name)
    }

    /// The access class governing this namespace's attributes.
    #[must_use]
    pub const fn access(self) -> NamespaceAccess {
        match self {
            Namespace::User
            | Namespace::Acorn
            | Namespace::Amiga
            | Namespace::Atari
            | Namespace::Mac
            | Namespace::Rustos => NamespaceAccess::FilePermission,
            Namespace::System | Namespace::Trusted => NamespaceAccess::Privileged,
        }
    }

    /// Whether the namespace guards a security boundary that the VFS gates
    /// with a capability. Convenience over [`Self::access`].
    #[must_use]
    pub const fn is_privileged(self) -> bool {
        matches!(self.access(), NamespaceAccess::Privileged)
    }
}

/// A validated attribute key: its full namespaced bytes plus the resolved
/// [`Namespace`]. Construction is the only way to obtain one, so an
/// `AttrKey` in hand is always well-formed and in-bounds.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct AttrKey {
    bytes: Vec<u8>,
    namespace: Namespace,
}

impl AttrKey {
    /// Validate and parse `bytes` into an [`AttrKey`].
    ///
    /// Fails closed with [`MetadataError`] if the key is empty, longer than
    /// [`KEY_MAX`], not valid UTF-8, contains a `/` or NUL byte, has no
    /// `namespace.rest` split, has an empty `rest`, or names a namespace
    /// outside the closed set.
    ///
    /// # Errors
    ///
    /// See above; every failure is a fail-closed rejection.
    pub fn parse(bytes: &[u8]) -> Result<AttrKey, MetadataError> {
        if bytes.len() > KEY_MAX {
            return Err(MetadataError::KeyTooLong);
        }
        if bytes.is_empty() {
            return Err(MetadataError::MalformedKey);
        }
        if bytes.iter().any(|&b| b == 0 || b == b'/') {
            return Err(MetadataError::MalformedKey);
        }
        let text = core::str::from_utf8(bytes).map_err(|_| MetadataError::MalformedKey)?;
        let dot = text.find('.').ok_or(MetadataError::MalformedKey)?;
        let (name, rest) = text.split_at(dot);
        // `rest` still carries the leading '.'; the component after it must be
        // non-empty.
        if rest.len() <= 1 {
            return Err(MetadataError::MalformedKey);
        }
        let namespace = Namespace::from_name(name).ok_or(MetadataError::UnknownNamespace)?;
        Ok(AttrKey {
            bytes: bytes.to_vec(),
            namespace,
        })
    }

    /// The key's full namespaced bytes.
    #[must_use]
    pub fn as_bytes(&self) -> &[u8] {
        &self.bytes
    }

    /// The key's namespace.
    #[must_use]
    pub fn namespace(&self) -> Namespace {
        self.namespace
    }

    /// The key's access class (shorthand for `self.namespace().access()`).
    #[must_use]
    pub fn access(&self) -> NamespaceAccess {
        self.namespace.access()
    }
}
