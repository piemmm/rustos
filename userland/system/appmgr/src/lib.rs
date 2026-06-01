//! RustOS application-bundle loader (`appmgr`), Stage 6 — `AGENTS.md` §16.4,
//! §16.5.
//!
//! An installed application is a `/Apps/<Name>.app/` bundle with a fixed
//! top-level layout and a signed `AppInfo` manifest (`AGENTS.md` §16.5).
//! This crate is the user-space service that decides whether such a bundle
//! may be launched and with what authority. It owns three responsibilities
//! and **fails closed** at the first problem (`AGENTS.md` §5.4):
//!
//! 1. **Layout** — the bundle's top-level entries must be exactly drawn from
//!    the fixed [`rustos_abi::BundleEntry`] set, with the mandatory `AppInfo`
//!    and `Run` present. Any other entry is a packaging defect and the whole
//!    bundle is refused.
//! 2. **Authenticity** — the signed [`rustos_abi::AppInfoHeader`] manifest is
//!    decoded, its target ABI version and syscall-table hash are matched
//!    against the kernel's, its Ed25519 signature is verified, and the
//!    content hash it carries is checked against the bundle's actual
//!    contents (`AGENTS.md` §16.5 — "signature over the bundle contents").
//! 3. **Authority** — the granted capability set is the manifest's request
//!    *intersected* with the launching user's grants; ambient authority is
//!    forbidden (`AGENTS.md` §4, §5.2), so the loader never widens a request.
//!
//! It additionally enforces the §16.4 dynamic-loader policy
//! ([`AppLoader::resolve_library`]): a shared-library reference resolves only
//! against the bundle's own `Libraries/` directory or `/System/Libraries/`.
//!
//! # Seams
//!
//! The two operations that touch the outside world — reading the bundle off
//! the filesystem and verifying a signature — are injected as the
//! [`BundleStore`] and [`Verifier`] seams. The binary that ships as
//! `/System/Services/appmgr` wires the real kernel-/`lib/crypto`-backed
//! implementations; tests wire in-memory fixtures. This keeps the
//! security-relevant layout, capability, and policy code independent of
//! kernel plumbing and exhaustively testable.
//!
//! The loader never *executes* anything: it computes the capability ceiling
//! and the validated entry-point path and returns them in a [`LoadedApp`].
//! Spawning the verified `rxe` binary with that ceiling is the caller's job
//! (the same load gate `init`/`drvhost` use, `AGENTS.md` §8, §9).
//!
//! # Layering
//!
//! The crate is `no_std` (with `alloc`, `AGENTS.md` §6) and depends only on
//! the audited `lib/*` crates `rustos-abi`, `rustos-caps`, and `rustos-log`,
//! so it links no kernel or driver crate (`AGENTS.md` §17.4).

#![no_std]
#![forbid(unsafe_op_in_unsafe_fn)]
#![deny(missing_docs)]

extern crate alloc;

pub mod bundle;
pub mod error;
pub mod events;
pub mod loader;

pub use bundle::{BundleStore, LoadedApp, Verifier};
pub use error::AppError;
pub use loader::{AppLoader, AppLoaderConfig};
