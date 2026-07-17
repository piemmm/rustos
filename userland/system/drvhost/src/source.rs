//! [`ImageSource`] abstraction over `.rxe` image storage.
//!
//! `drvhost` is `no_std` and must not assume a filesystem API: real
//! deployments fetch image bytes through the host's filesystem driver,
//! tests fetch them from an in-memory map, and the QEMU integration test
//! fetches them from a `.rodata` blob. The [`ImageSource`] trait is the
//! single seam between the host's verification pipeline and whichever of
//! these stores supplies the bytes.
//!
//! A logical *path* is an opaque `&str` chosen by the caller; the host
//! stores it verbatim so that [`crate::Host::reload`] can re-fetch the
//! same image without re-deriving its location.

use alloc::vec::Vec;
use tairix_abi::Errno;

/// Source of `.rxe` image bytes consumed by [`crate::Host::load`] and
/// [`crate::Host::reload`].
///
/// Implementations must:
///
/// * **append** the image bytes to `buf` rather than overwriting it
///   (the host pre-clears and pre-sizes `buf`); and
/// * return [`Errno::NotFound`] if `path` does not name an image known
///   to the source.
///
/// # Errors
///
/// Returns the source-supplied [`Errno`] verbatim. The host wraps it
/// in [`crate::HostError::SourceFailed`] before surfacing it to its
/// caller, preserving the original `Errno` for audit.
///
/// # Capabilities
///
/// The trait does not require a capability itself; the host has
/// already checked `CAP_DRV_LOAD` (and `CAP_DRV_KERNEL` when relevant)
/// against the caller before invoking the source.
pub trait ImageSource {
    /// Read the bytes for `path` into `buf`, appending to whatever is
    /// already there.
    fn read(&self, path: &str, buf: &mut Vec<u8>) -> Result<(), Errno>;
}

#[cfg(test)]
mod tests {
    use super::*;
    use alloc::collections::BTreeMap;
    use alloc::string::{String, ToString};
    use alloc::vec;

    /// Minimal in-memory implementation used to anchor the trait
    /// surface; the real `MockSource` used by the host tests lives in
    /// `crate::host::tests` so it can share fixtures with the rest of
    /// the test machinery.
    struct MemMap(BTreeMap<String, Vec<u8>>);

    impl ImageSource for MemMap {
        fn read(&self, path: &str, buf: &mut Vec<u8>) -> Result<(), Errno> {
            match self.0.get(path) {
                Some(bytes) => {
                    buf.extend_from_slice(bytes);
                    Ok(())
                }
                None => Err(Errno::NotFound),
            }
        }
    }

    #[test]
    fn happy_path_appends_to_buf() {
        let mut m = MemMap(BTreeMap::new());
        m.0.insert("/d/one".to_string(), vec![1u8, 2, 3]);
        let mut out = vec![0xFFu8];
        m.read("/d/one", &mut out).expect("known path");
        assert_eq!(out, vec![0xFF, 1, 2, 3]);
    }

    #[test]
    fn unknown_path_returns_not_found() {
        let m = MemMap(BTreeMap::new());
        let mut out = Vec::new();
        assert_eq!(m.read("/d/missing", &mut out), Err(Errno::NotFound));
        assert!(out.is_empty());
    }
}
