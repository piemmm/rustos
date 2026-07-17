//! The fail-closed error type for bundle loading.

use tairix_abi::{BundleLayoutError, Errno, LibraryError, RxeError};

/// Why a bundle could not be loaded, or a library reference resolved.
///
/// Every variant is a refusal: the loader maps the first problem it meets to
/// one of these and stops (fail closed). There is no
/// "loaded with warnings" outcome.
#[derive(Copy, Clone, Debug, Eq, PartialEq)]
#[non_exhaustive]
pub enum AppError {
    /// The bundle could not be read from the [`BundleStore`](crate::BundleStore);
    /// carries the underlying [`Errno`].
    Store(Errno),
    /// The bundle's top-level layout deviates from the fixed set.
    Layout(BundleLayoutError),
    /// The `AppInfo` manifest could not be decoded, or targets an
    /// unsupported ABI version; carries the underlying [`Errno`].
    Manifest(Errno),
    /// The manifest's declared syscall-table hash does not match the
    /// kernel's compiled-in hash.
    InterfaceHashMismatch,
    /// The manifest's Ed25519 signature did not verify.
    Signature,
    /// The bundle's contents do not match the content hash the signature
    /// covers.
    ContentHashMismatch,
    /// A shared-library reference violated the dynamic-loader policy.
    Library(LibraryError),
    /// The entry-point `Run` binary is not a valid `rxe` load image, or its
    /// CFI tag does not match the kernel's syscall interface hash; carries the underlying [`RxeError`].
    RunImage(RxeError),
}

impl core::fmt::Display for AppError {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        match self {
            Self::Store(_) => f.write_str("bundle could not be read"),
            Self::Layout(e) => write!(f, "invalid bundle layout: {e}"),
            Self::Manifest(_) => f.write_str("invalid AppInfo manifest"),
            Self::InterfaceHashMismatch => f.write_str("syscall interface hash mismatch"),
            Self::Signature => f.write_str("manifest signature did not verify"),
            Self::ContentHashMismatch => f.write_str("bundle contents do not match signed hash"),
            Self::Library(e) => write!(f, "shared-library reference refused: {e}"),
            Self::RunImage(e) => write!(f, "entry-point binary refused: {e}"),
        }
    }
}
