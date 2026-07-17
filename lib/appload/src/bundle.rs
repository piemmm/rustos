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

use tairix_caps::CapabilitySet;

use tairix_abi::{Errno, LibraryScope};

/// Reads an application bundle from storage.
///
/// Every method addresses a bundle by its root path (e.g.
/// `/Apps/Example.app`). The implementation enforces the filesystem's own
/// permission checks; the loader treats every method as
/// fallible and **never** assumes a trusted result.
pub trait BundleStore {
    /// The names of the entries directly under the bundle root, in any
    /// order. The loader validates them against the fixed layout.
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

    /// Hash every bundle file the signature covers **and** return the
    /// entry-point `Run` image read during that same walk.
    ///
    /// The `Run` binary is one of the files the content hash covers, so a
    /// store must already read it to compute the digest. Returning it here
    /// means the load path reads `Run` from disk exactly **once** — the
    /// content-hash pass and the entry-point read are the same pass — rather
    /// than reading the whole file a second time. The loader compares the
    /// returned [`BundleContents::content_hash`] against the hash embedded in
    /// the signed manifest before it trusts [`BundleContents::run_image`],
    /// so the bytes are authenticated by the same signed hash.
    ///
    /// # Errors
    ///
    /// Returns the store's [`Errno`] if the contents cannot be read or
    /// hashed, or if the bundle carries no `Run` file.
    fn contents(&self, bundle: &str) -> Result<BundleContents, Errno>;
}

/// A bundle's verified contents: the content hash over every file the
/// signature covers, paired with the entry-point `Run` image read out of
/// that same hashing walk.
///
/// Produced by [`BundleStore::contents`]. Pairing the two is what lets the
/// loader read `Run` from disk once instead of twice (once to hash, once to
/// execute).
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct BundleContents {
    /// The cryptographic digest over the canonical framing of every bundle
    /// file the signature covers (every file except the top-level
    /// `AppInfo`). Compared against the signed manifest's embedded hash.
    pub content_hash: [u8; 32],
    /// The raw bytes of the entry-point `Run` binary (an `rxe` load image),
    /// as read during the content-hash walk. Authenticated by
    /// `content_hash` once that hash matches the signed manifest.
    pub run_image: Vec<u8>,
}

/// A monotonic clock the loader reads only to *measure* how long its two
/// observable phases take: the time spent reading the bundle off the
/// [`BundleStore`] (the "load from disk" cost) and the remaining time spent
/// verifying it (layout, manifest, interface-hash, signature, content-hash,
/// and entry-point checks). The readings are for the audit record only; no
/// load decision depends on them, so a coarse or even fixed clock is safe and
/// never widens authority.
pub trait Clock {
    /// A monotonically non-decreasing reading in nanoseconds. Only
    /// *differences* between two readings are used, so the epoch is
    /// irrelevant. A reading that fails to advance yields a zero-length
    /// phase rather than a negative span.
    fn now_ns(&self) -> u64;
}

/// One shared-library reference the entry-point binary declares it needs,
/// paired with the policy root it resolved against.
///
/// Holding a `ResolvedLibrary` is proof the reference passed the
/// dynamic-loader policy: it lies inside the bundle's own `Libraries/` or
/// the curated [`tairix_abi::SYSTEM_LIBRARIES_DIR`], with no `..` component.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ResolvedLibrary {
    /// The shared-library reference path, exactly as the binary declared it.
    pub reference: String,
    /// Which policy root the reference resolved against.
    pub scope: LibraryScope,
}

/// Verifies a detached Ed25519 signature over a byte range.
///
/// The real implementation calls into `lib/crypto` (the
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
/// path and image, and the capability ceiling it may run with.
///
/// Holding a `LoadedApp` is proof that the layout, the manifest
/// signature, the content hash, and the syscall-interface hash all checked
/// out, and that `granted` is the manifest request intersected with the
/// launching user's grants. The caller spawns
/// [`run_image`](Self::run_image) with **at most** [`granted`](Self::granted).
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct LoadedApp {
    id: String,
    name: String,
    version: String,
    run_path: String,
    run_image: Vec<u8>,
    granted: CapabilitySet,
    libraries: Vec<ResolvedLibrary>,
}

impl LoadedApp {
    pub(crate) fn new(
        id: String,
        name: String,
        version: String,
        run_path: String,
        run_image: Vec<u8>,
        granted: CapabilitySet,
        libraries: Vec<ResolvedLibrary>,
    ) -> Self {
        Self {
            id,
            name,
            version,
            run_path,
            run_image,
            granted,
            libraries,
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

    /// The exact entry-point `rxe` bytes the pipeline validated — the bytes
    /// [`tairix_abi::LoadImage::parse`] accepted after the content hash
    /// checked out. A spawner maps **these** bytes, never a re-read of
    /// [`run_path`](Self::run_path), so what runs is what was verified.
    #[must_use]
    pub fn run_image(&self) -> &[u8] {
        &self.run_image
    }

    /// The capability ceiling the app may run with — the manifest request
    /// intersected with the launching user's grants.
    #[must_use]
    pub fn granted(&self) -> &CapabilitySet {
        &self.granted
    }

    /// The shared libraries the entry-point binary declared it needs, each
    /// already resolved against the dynamic-loader policy (the
    /// bundle's own `Libraries/` or the curated
    /// [`tairix_abi::SYSTEM_LIBRARIES_DIR`]).
    #[must_use]
    pub fn libraries(&self) -> &[ResolvedLibrary] {
        &self.libraries
    }
}
