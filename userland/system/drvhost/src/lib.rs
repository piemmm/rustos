//! RustOS userland driver host (Stage 4 — `AGENTS.md` §8).
//!
//! This crate is the userland service that owns the lifecycle of every
//! `.rxe` driver module on a running RustOS system. It is the single point
//! at which a driver image is parsed, verified, capability-checked, and
//! handed an environment ([`rustos_abi::DriverHost`]) to register itself
//! against. Per `AGENTS.md` §4 / §8 the host runs in user space by default;
//! the same code path also handles `kind = "in-kernel"` drivers by
//! demanding [`CapabilityId::DRV_KERNEL`](rustos_abi::CapabilityId::DRV_KERNEL)
//! on top of the universal
//! [`CapabilityId::DRV_LOAD`](rustos_abi::CapabilityId::DRV_LOAD).
//!
//! # Pipeline
//!
//! For every `load`/`reload` the host executes the following checks in
//! order, **failing closed** at the first failure (`AGENTS.md` §5.4.5):
//!
//! 1. Decode the [`rustos_abi::DriverManifest`] header.
//! 2. Reject if `abi_version` is not the host's accepted version.
//! 3. Reject if `syscall_table_hash` does not match the host's compiled-in
//!    hash (i.e. an `abi-vN` binary on an `abi-vM` host).
//! 4. Reject if the signer key is not on the host's trust anchor list.
//! 5. Verify the Ed25519 signature with [`rustos_crypto`].
//! 6. Decode the capability body and reject if `kind = InKernel` without
//!    `CAP_DRV_KERNEL` in the caller's set.
//! 7. Reject if the manifest's requested capability set is **not a subset**
//!    of the caller's set (`AGENTS.md` §5.2 — capabilities can be
//!    delegated but never widened).
//! 8. Hand the verified manifest + payload to the host's
//!    [`DriverSpawner`], which completes the driver's registration in its
//!    own protection domain and reports the outcome.
//! 9. Issue a fresh [`DriverHandle`] and emit a structured log record via
//!    [`rustos_log`].
//!
//! Buffers that held the manifest signature or capability bitmap are
//! cleared with [`zeroize::secure_clear`] when the load record is dropped
//! (`AGENTS.md` §4 — "Zero-on-free for any allocation that ever held
//! credentials, keys, or capability tokens").
//!
//! # Module map
//!
//! * [`error`] — host-level error type returned across the public API.
//! * [`events`] — stable [`rustos_log::EventId`] constants (`7000` range).
//! * [`image`] — `.rxe` envelope splitter (header + capability body +
//!   payload).
//! * [`zeroize`] — volatile-clear primitive used to wipe sensitive
//!   buffers.
//! * [`source`] — [`ImageSource`] trait abstracting file IO so the host
//!   stays `no_std`.
//! * [`store`] — the signed driver-store scan that turns the installed
//!   `/System/Drivers/` bundles into `devmgr` autoload candidates
//!   (`AGENTS.md` §18.3 / §18.6).
//! * [`spawner`] — [`DriverSpawner`] trait that completes a verified
//!   image's registration in its own protection domain.
//! * [`host`] — the [`Host`] state machine implementing `load` / `unload`
//!   / `reload`.
//!
//! # Layering
//!
//! The crate is `no_std` (`AGENTS.md` §6) and pulls only the audited
//! `rustos-abi`, `rustos-caps`, `rustos-crypto`, `rustos-devmatch`,
//! `rustos-log`, and `rustos-virtio` crates — all under `lib/*`, so the
//! host never links a
//! kernel or driver crate (`AGENTS.md` §17.4). The `VirtioHostFactory`
//! seam consumed by [`HostConfig`] lives in `rustos_virtio`.
//! It exposes no `unsafe` across its crate boundary — the one in-tree
//! `unsafe` block lives in [`zeroize::secure_clear`] and is covered by a
//! unit test (`AGENTS.md` §2.10).
//!
//! # Stability
//!
//! Tier: `experimental` (per `AGENTS.md` §6). The public surface will
//! freeze when Stage 4 lands its first real driver. The wire formats
//! consumed (manifest header, capability body) are already frozen by
//! `rustos-abi`.

#![no_std]
#![forbid(unsafe_op_in_unsafe_fn)]
#![deny(missing_docs)]

extern crate alloc;

pub mod error;
pub mod events;
pub mod host;
pub mod image;
pub mod source;
pub mod spawner;
pub mod store;
pub mod zeroize;

pub use error::HostError;
pub use host::{Host, HostConfig, LoadedSnapshot};
pub use image::ParsedImage;
pub use source::ImageSource;
pub use spawner::{DriverEntry, DriverSpawner, SpawnContext, SpawnRegisterError};
pub use store::{scan_store, DriverStore, ScannedDriver};

// Re-export the canonical match-candidate type the store scan produces
// (the single `lib/devmatch` definition, `AGENTS.md` §2.2) so the
// bin-crate boot wiring can name a candidate without a separate
// `rustos-devmatch` dependency.
pub use rustos_devmatch::DriverCandidate;

// Re-export the `lib/abi` types that appear in the host's public surface
// so callers do not need to take a transitive dependency on `rustos-abi`
// just to name a `DriverHandle`.
pub use rustos_abi::{DriverError, DriverHandle, DriverHost, DriverKind, DriverManifest};

// Re-export the `lib/log` items integration tests must implement to
// observe audit records. Pulling these through the host crate keeps
// downstream `Cargo.toml` files minimal.
pub use rustos_log::{Event, EventId, Field, Level, Sink};
