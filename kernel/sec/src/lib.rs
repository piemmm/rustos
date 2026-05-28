//! RustOS kernel security state.
//!
//! `kernel/sec` owns the in-kernel mirrors of every datum a privileged
//! operation consults: the user/group tables, the per-task capability
//! tables, the manifest verifier that decides whether a binary may load,
//! and the audit-log writer that records every security-relevant
//! decision. It is the single source of truth the rest of the kernel
//! routes its security questions through.
//!
//! The crate is `no_std` (the audit path and the manifest verifier never
//! allocate; the identity tables use `alloc::vec::Vec` because they
//! are sized by the on-disk record count and live for the lifetime of
//! the running kernel). It depends only on `lib/abi`, `lib/caps`,
//! `lib/crypto`, and `lib/log` — the surface specified by the Stage 2.4
//! task brief in `PLAN.md`.
//!
//! # Module map
//!
//! * [`audit`] — stable event IDs and the structured writer used by
//!   every decision recorded by this crate.
//! * [`identity`] — [`UserId`], [`GroupId`], record types and the
//!   verifying builder for [`IdentityTable`].
//! * [`manifest`] — Ed25519 signature, ABI-version, and known-capability
//!   checks for `rxe` manifests; produces a [`VerifiedManifest`].
//! * [`captable`] — [`TaskCapabilities`]: per-task intersection of user
//!   grant and manifest request, with delegation, revocation, and
//!   signed [`rustos_caps::CapabilityToken`] application.
//!
//! # Out of scope for Stage 2.4 (issue brief)
//!
//! * Filesystem ACLs (Stage 5).
//! * IPC dispatch checks (Stage 2.5 in the issue brief).
//! * Syscall plumbing (Stage 2.7 in the issue brief).
//!
//! These modules consume the types defined here through their public
//! API; they do not re-implement the security checks.
//!
//! # No ambient authority
//!
//! Per `AGENTS.md` §4 and §5.1, nothing in `kernel/sec` branches on
//! `uid == 0`. Authority is *purely* the capability bits a record
//! carries. Tests (`identity::uid_zero_is_not_ambient_root` and
//! `captable::uid_zero_gets_no_extra_powers`) lock this invariant in.

#![no_std]
#![forbid(unsafe_op_in_unsafe_fn)]
#![deny(missing_docs)]

pub mod audit;
pub mod captable;
pub mod identity;
pub mod manifest;

pub use audit::AuditEvent;
pub use captable::{CapTable, TaskCapabilities, TaskId};
pub use identity::{
    GroupId, GroupRecord, IdentityTable, IdentityTableBuilder, UserId, UserRecord,
    MAX_SUPPLEMENTARY_GROUPS,
};
pub use manifest::{is_known_capability, verify_manifest, VerifiedManifest};
