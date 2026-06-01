//! The seams the loader reaches the outside world through, and the
//! description of a successfully loaded bundle.
//!
//! [`BundleStore`] reads a bundle off the filesystem and [`Verifier`] checks
//! a signature; on a running system they are backed by the VFS and
//! `lib/crypto`, while tests wire in-memory fixtures. Keeping them as traits
//! is what makes the otherwise-pure [`AppLoader`](crate::AppLoader) testable
//! without a kernel.

use alloc::string::String;
use alloc::vec::Vec;

use rustos_caps::CapabilitySet;

use rustos_abi::Errno;

/// Reads an application bundle from storage.
///
/// All three methods address a bundle by its root path (e.g.
/// `/Apps/Example.app`). The implementation enforces the filesystem's own
/// permission checks (`AGENTS.md` §5.3); the loader treats every method as
/// fallible and **never** assumes a trusted result.
pub trait BundleStore {
    /// The names of the entries directly under the bundle root, in any
    /// order. The loader validates them against the fixed §16.5 layout.
    ///
    /// # Errors
    ///
    /// Returns the store's [`Errno`] if the bundle directory cannot be read.
    fn entries(&self, bundle: &str) -> Result<Vec<String>, Errno>;

    /// The raw bytes of the bundle's `AppInfo` manifest (header followed by
    /// its capability/MIME body).
    ///
    /// # Errors
    ///
    /// Returns the store's [`Errno`] if the manifest cannot be read.
    fn read_appinfo(&self, bundle: &str) -> Result<Vec<u8>, Errno>;

    /// The cryptographic digest of the bundle's contents, computed by the
    /// store over every file the signature covers (`AGENTS.md` §16.5). The
    /// loader compares it against the hash embedded in the signed manifest.
    ///
    /// # Errors
    ///
    /// Returns the store's [`Errno`] if the contents cannot be hashed.
    fn content_hash(&self, bundle: &str) -> Result<[u8; 32], Errno>;
}

/// Verifies a detached Ed25519 signature over a byte range.
///
/// The real implementation calls into `lib/crypto` (`AGENTS.md` §2.12 — the
/// one place cryptographic primitives live). The loader passes the manifest
/// bytes the signature covers, the signature, and the signer's public key;
/// trust-rooting that key against the local capability authority is the
/// implementation's concern, not the loader's.
pub trait Verifier {
    /// Verify `signature` over `signed` under `signer_pubkey`.
    ///
    /// # Errors
    ///
    /// Returns [`Errno::SignatureInvalid`] (or another [`Errno`]) if the
    /// signature does not verify or the key is not trusted; the loader maps
    /// any error to [`AppError::Signature`](crate::AppError::Signature).
    fn verify(
        &self,
        signed: &[u8],
        signature: &[u8; 64],
        signer_pubkey: &[u8; 32],
    ) -> Result<(), Errno>;
}

/// A bundle the loader has accepted: its identity, the validated entry-point
/// path, and the capability ceiling it may run with.
///
/// Holding a `LoadedApp` is proof that the §16.5 layout, the manifest
/// signature, the content hash, and the syscall-interface hash all checked
/// out, and that `granted` is the manifest request intersected with the
/// launching user's grants (`AGENTS.md` §5.2). The caller spawns
/// [`run_path`](Self::run_path) with **at most** [`granted`](Self::granted).
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct LoadedApp {
    id: String,
    name: String,
    version: String,
    run_path: String,
    granted: CapabilitySet,
}

impl LoadedApp {
    pub(crate) fn new(
        id: String,
        name: String,
        version: String,
        run_path: String,
        granted: CapabilitySet,
    ) -> Self {
        Self {
            id,
            name,
            version,
            run_path,
            granted,
        }
    }

    /// The bundle identifier from the manifest.
    #[must_use]
    pub fn id(&self) -> &str {
        &self.id
    }

    /// The human-readable bundle name from the manifest.
    #[must_use]
    pub fn name(&self) -> &str {
        &self.name
    }

    /// The bundle version string from the manifest.
    #[must_use]
    pub fn version(&self) -> &str {
        &self.version
    }

    /// The absolute path of the entry-point `Run` binary.
    #[must_use]
    pub fn run_path(&self) -> &str {
        &self.run_path
    }

    /// The capability ceiling the app may run with — the manifest request
    /// intersected with the launching user's grants.
    #[must_use]
    pub fn granted(&self) -> &CapabilitySet {
        &self.granted
    }
}
