//! RustOS application-bundle load gate (`rustos-appload`).
//!
//! An installed application — and every OS-provided command app and service
//! (a service is an app) — is a `<Name>.app/` bundle with a fixed top-level
//! layout and a signed `AppInfo` manifest. This crate is the one place a
//! bundle is *judged*: it decides whether a bundle may be launched and with
//! what authority, and **fails closed** at the first problem. It owns three
//! responsibilities:
//!
//! 1. **Layout** — the bundle's top-level entries must be exactly drawn from
//!    the fixed [`rustos_abi::BundleEntry`] set, with the mandatory `AppInfo`
//!    and `Run` present. Any other entry is a packaging defect and the whole
//!    bundle is refused.
//! 2. **Authenticity** — the signed [`rustos_abi::AppInfoHeader`] manifest is
//!    decoded, its target ABI version and syscall-table hash are matched
//!    against the kernel's, its Ed25519 signature is verified, and the
//!    content hash it carries is checked against the bundle's actual
//!    contents ("signature over the bundle contents").
//! 3. **Authority** — the granted capability set is the manifest's request
//!    *intersected* with the launching user's grants; ambient authority is
//!    forbidden, so the loader never widens a request.
//!
//! It additionally validates the entry-point `Run` binary through
//! [`rustos_abi::LoadImage::parse`] (enforcing the PIE / W^X / CFI-tag
//! invariants on a C binary identically to a Rust one) and resolves every
//! shared library that binary declares it needs under the dynamic-loader
//! policy ([`AppLoader::resolve_library`]): a reference resolves only against
//! the bundle's own `Libraries/` directory or `/System/Libraries/`. The whole
//! pipeline is language-agnostic, so a C-compiled bundle
//! (`plans/CCOMPAT.md` stage CC4) is judged exactly like a Rust one.
//!
//! # Seams
//!
//! The two operations that touch the outside world — reading the bundle off
//! the filesystem and verifying a signature — are injected as the
//! [`BundleStore`] and [`Verifier`] seams. A consumer wires the real
//! kernel-/`lib/crypto`-backed implementations; tests wire in-memory
//! fixtures. This keeps the security-relevant layout, capability, and policy
//! code independent of kernel plumbing and exhaustively testable.
//!
//! The loader never *executes* anything: it computes the capability ceiling
//! and the validated entry-point path and returns them in a [`LoadedApp`].
//! Spawning the verified `rxe` binary with that ceiling is the caller's job
//! (the same load gate `init`/`drvhost` use).
//!
//! # Layering
//!
//! This is a `lib/*` crate (`no_std`, with `alloc`) depending only on the
//! audited `lib/*` crates `rustos-abi`, `rustos-caps`, and `rustos-log`, so
//! it links no kernel or driver crate and both the kernel boot-floor spawn
//! path and the user-space application-manager service (`userland/system/
//! appmgr`) can share this one gate rather than each re-implementing it.

#![no_std]
#![forbid(unsafe_op_in_unsafe_fn)]
#![deny(missing_docs)]

extern crate alloc;

pub mod bundle;
pub mod error;
pub mod events;
pub mod loader;

pub use bundle::{BundleStore, LoadedApp, ResolvedLibrary, Verifier};
pub use error::AppError;
pub use loader::{AppLoader, AppLoaderConfig};
