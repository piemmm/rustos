//! The kernel volume forest: durable `id::` volume-root publication and
//! resolution (`docs/src/filesystem/drives.md` §8, `plans/DEVICES.md` D3a).
//!
//! TAIRiX storage is a forest of independently addressable named roots. The
//! durable machine form of a root is `id::<volume-id>/path`, where the
//! volume id is the filesystem's own stable identity (the `ARXFS`
//! per-volume UUID). This registry is the kernel-side map from that
//! identity to the live root: a boot or hotplug path **publishes** each
//! mounted volume's identity together with the `/`-view location its root
//! resolves to, and the single kernel path-resolution entry point resolves
//! an `id::`-rooted path against it.
//!
//! Resolving through the forest grants nothing: the resolved location is
//! then authorised by the secured VFS exactly as the equivalent view path
//! would be (per-inode owner/mode/ACL/`required_cap` and mount flags), so
//! the durable spelling is never a policy bypass. An identity that is not
//! published fails closed with "not found" — never a guessed root.
//!
//! Every mounted volume backs a region of the one `/` view, so a
//! publication records the volume root's view prefix: `[]` for the
//! writable root volume, `["System"]` for the read-only system volume,
//! and `["Storage", <name>]` for a runtime-attached volume projected
//! into the `Storage:` catalog (`plans/DEVICES.md` D3b). A hotplug
//! detach withdraws its publication with [`VolumeForest::unpublish`];
//! the boot volumes are published once and never withdrawn.

use alloc::string::String;
use alloc::vec::Vec;

use tairix_sync::RwLock;

/// One published volume root: the volume's stable 16-byte identity and the
/// `/`-view components its root directory resolves to.
struct VolumeRoot {
    id: [u8; 16],
    view_prefix: Vec<String>,
}

/// Why a volume-root publication was refused.
#[derive(Copy, Clone, Debug, Eq, PartialEq)]
pub enum VolumePublishError {
    /// The identity is the reserved all-zero value ("no identity"); a
    /// volume with no stable identity is never published, so a forged
    /// all-zero spelling can never resolve.
    NilIdentity,
    /// The identity is already published. A volume identity binds to
    /// exactly one root; a duplicate is refused, never silently re-bound.
    AlreadyPublished,
}

impl core::fmt::Display for VolumePublishError {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        let msg = match self {
            Self::NilIdentity => "volume has the reserved all-zero identity",
            Self::AlreadyPublished => "volume identity is already published",
        };
        f.write_str(msg)
    }
}

/// The registry of published volume roots the `id::` resolver reads.
///
/// Held as a `&'static` borrow by the syscall handlers, exactly like the
/// other boot-installed seams: the boot or runtime-attach path that mounts
/// a volume publishes its identity here, a runtime detach withdraws it,
/// and an identity that is not published fails closed. Reads take the
/// shared lock only for the tiny lookup, never across a filesystem
/// operation.
pub struct VolumeForest {
    roots: RwLock<Vec<VolumeRoot>>,
}

impl VolumeForest {
    /// Construct an empty forest. `const` so a boot path can place it in a
    /// `static`.
    #[must_use]
    pub const fn new() -> Self {
        Self {
            roots: RwLock::new(Vec::new()),
        }
    }

    /// Publish a mounted volume's stable identity, binding it to the
    /// `/`-view location its root directory resolves to.
    ///
    /// # Errors
    ///
    /// * [`VolumePublishError::NilIdentity`] for the reserved all-zero
    ///   identity.
    /// * [`VolumePublishError::AlreadyPublished`] when the identity is
    ///   already bound — fail closed, never a silent re-bind.
    pub fn publish(&self, id: [u8; 16], view_prefix: &[&str]) -> Result<(), VolumePublishError> {
        if id == [0u8; 16] {
            return Err(VolumePublishError::NilIdentity);
        }
        let mut roots = self.roots.write();
        if roots.iter().any(|root| root.id == id) {
            return Err(VolumePublishError::AlreadyPublished);
        }
        roots.push(VolumeRoot {
            id,
            view_prefix: view_prefix.iter().map(|c| String::from(*c)).collect(),
        });
        Ok(())
    }

    /// Withdraw a published identity, returning the `/`-view components
    /// its root resolved to so the caller can retract the matching mount.
    ///
    /// Fails closed: an identity that is not published returns `None` and
    /// removes nothing, so a forged or repeated detach can never withdraw
    /// another volume's root.
    pub fn unpublish(&self, id: &[u8; 16]) -> Option<Vec<String>> {
        let mut roots = self.roots.write();
        let pos = roots.iter().position(|root| &root.id == id)?;
        Some(roots.remove(pos).view_prefix)
    }

    /// Resolve a published identity to its root's `/`-view components, or
    /// `None` when the identity is not published (fail closed — the caller
    /// reports "not found", never a guessed root).
    ///
    /// Returns an owned snapshot so no registry borrow is held across the
    /// filesystem operation the caller goes on to perform.
    #[must_use]
    pub fn resolve(&self, id: &[u8; 16]) -> Option<Vec<String>> {
        self.roots
            .read()
            .iter()
            .find(|root| &root.id == id)
            .map(|root| root.view_prefix.clone())
    }
}

impl Default for VolumeForest {
    fn default() -> Self {
        Self::new()
    }
}

/// The fail-closed default forest the syscall handlers hold before a boot
/// path installs the real one: nothing is ever published into it, so every
/// `id::` resolution against it reports "not found".
pub static NULL_VOLUME_FOREST: VolumeForest = VolumeForest::new();

#[cfg(test)]
mod tests {
    use super::*;
    use alloc::string::ToString;
    use alloc::vec;

    const ID_A: [u8; 16] = [1; 16];
    const ID_B: [u8; 16] = [2; 16];

    #[test]
    fn publish_then_resolve_returns_the_prefix() {
        let forest = VolumeForest::new();
        forest.publish(ID_A, &["System"]).expect("publish");
        forest.publish(ID_B, &[]).expect("publish");
        assert_eq!(forest.resolve(&ID_A), Some(vec!["System".to_string()]));
        assert_eq!(forest.resolve(&ID_B), Some(Vec::new()));
    }

    #[test]
    fn unpublished_identity_fails_closed() {
        let forest = VolumeForest::new();
        forest.publish(ID_A, &[]).expect("publish");
        assert_eq!(forest.resolve(&ID_B), None);
    }

    #[test]
    fn nil_identity_is_refused() {
        let forest = VolumeForest::new();
        assert_eq!(
            forest.publish([0u8; 16], &[]),
            Err(VolumePublishError::NilIdentity)
        );
        assert_eq!(forest.resolve(&[0u8; 16]), None);
    }

    #[test]
    fn duplicate_identity_is_refused_and_keeps_the_first_binding() {
        let forest = VolumeForest::new();
        forest.publish(ID_A, &["System"]).expect("publish");
        assert_eq!(
            forest.publish(ID_A, &["Users"]),
            Err(VolumePublishError::AlreadyPublished)
        );
        assert_eq!(forest.resolve(&ID_A), Some(vec!["System".to_string()]));
    }

    #[test]
    fn unpublish_withdraws_exactly_the_named_root() {
        let forest = VolumeForest::new();
        forest.publish(ID_A, &["Storage", "usb1"]).expect("publish");
        forest.publish(ID_B, &[]).expect("publish");
        assert_eq!(
            forest.unpublish(&ID_A),
            Some(vec!["Storage".to_string(), "usb1".to_string()])
        );
        // The withdrawn identity no longer resolves; the other root is
        // untouched; a repeated detach fails closed.
        assert_eq!(forest.resolve(&ID_A), None);
        assert_eq!(forest.resolve(&ID_B), Some(Vec::new()));
        assert_eq!(forest.unpublish(&ID_A), None);
        // The identity can be re-published (the re-insert path).
        forest
            .publish(ID_A, &["Storage", "usb1"])
            .expect("re-publish");
        assert!(forest.resolve(&ID_A).is_some());
    }

    #[test]
    fn unpublish_unknown_identity_fails_closed() {
        let forest = VolumeForest::new();
        assert_eq!(forest.unpublish(&ID_A), None);
        assert_eq!(forest.unpublish(&[0u8; 16]), None);
    }

    #[test]
    fn null_forest_resolves_nothing() {
        assert_eq!(NULL_VOLUME_FOREST.resolve(&ID_A), None);
    }

    #[test]
    fn publish_error_display_is_non_empty() {
        for e in [
            VolumePublishError::NilIdentity,
            VolumePublishError::AlreadyPublished,
        ] {
            assert!(!e.to_string().is_empty());
        }
    }
}
