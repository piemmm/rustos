//! TAIRiX hashing: two hash functions and the key the first of them is
//! keyed with.
//!
//! A hash table over attacker-chosen keys degenerates from O(1) to O(n) per
//! lookup once the attacker can predict which keys collide, and the keys
//! TAIRiX exposes to that are real: filenames from a mounted foreign volume,
//! DNS names, network 5-tuples, IPC method names, bundle identifiers, and
//! user-supplied futex addresses. The defence is a keyed pseudo-random
//! function under a key the attacker cannot observe.
//!
//! | Type | Kind | Use it for |
//! |---|---|---|
//! | [`SipHash13`] | Keyed pseudo-random function | Any hash over a key an attacker can choose or influence. |
//! | [`FastHash`] | Fast, **not** keyed (XXH64) | Kernel-assigned keys, content fingerprints, revision counters. |
//!
//! [`SipHash13`] is the default; [`FastHash`] is opt-in by naming it, so the
//! weaker choice is always visible in review.
//!
//! # The key
//!
//! [`HashSeed`] is 128 bits drawn from the platform CSPRNG and published
//! **once** per boot in the kernel and **once per process** in userland
//! ([`publish`]), so no cross-process collision oracle exists and a
//! compromise of one process does not reveal another's table layout. This
//! crate never draws the key itself — it is injected, which keeps the crate
//! dependency-free and lets the boot path decide where entropy comes from.
//!
//! [`published`] reports whether a key exists yet. A consumer whose hash is
//! over untrusted keys refuses to run unkeyed; a consumer whose hash is not a
//! security decision and must work before the CSPRNG is up names
//! [`HashSeed::UNKEYED`] explicitly, so the choice cannot be made by
//! accident.
//!
//! # Neither function is cryptographic
//!
//! `SipHash13` is a keyed PRF sized for hash-table defence, not a MAC: it is
//! the right tool for "an attacker must not be able to predict my bucket
//! index", and the wrong one for authenticating a message. Message
//! authentication, key derivation, and digests are `lib/crypto`'s.

#![no_std]
#![forbid(unsafe_code)]
#![deny(missing_docs)]

pub mod fast;
pub mod seed;
pub mod siphash;

pub use fast::FastHash;
pub use seed::{is_published, publish, published, AlreadyPublished, HashSeed};
pub use siphash::SipHash13;
