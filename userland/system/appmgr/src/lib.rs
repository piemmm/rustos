//! TAIRiX application-manager service (`appmgr`), Stage 6.
//!
//! `appmgr` is the user-space service that loads and launches an installed
//! `/Apps/<Name>.app/` bundle on behalf of a user. The security-relevant
//! *judgement* — validating the fixed bundle layout, verifying the signed
//! `AppInfo` manifest and content hash, computing the granted capability set
//! as the launching user's grants intersected with the manifest request, and
//! enforcing the dynamic-loader shared-library policy — is **not** owned here.
//! It lives in the shared `lib/appload` crate ([`tairix_appload`]) so the one
//! gate is used by both this service and the kernel boot-floor spawn path,
//! never re-implemented (`AGENTS.md` §2.2, §17.4).
//!
//! This crate re-exports that gate so a consumer of `appmgr` sees the same
//! surface; the service binary that wires the real VFS-backed
//! [`BundleStore`] and `lib/crypto`-backed [`Verifier`] and drives
//! [`AppLoader::load`] for user-initiated launches is built on top of it.

#![no_std]
#![forbid(unsafe_op_in_unsafe_fn)]
#![deny(missing_docs)]

pub use tairix_appload::{bundle, error, events, loader};
pub use tairix_appload::{
    AppError, AppLoader, AppLoaderConfig, BundleStore, Clock, LoadedApp, ResolvedLibrary, Verifier,
};
