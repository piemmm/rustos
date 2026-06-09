//! Stateful property model for `lib/caps` (`AGENTS.md` §19.7 Bronze).
//!
//! §19.7 requires the capability-critical paths to carry a `proptest`-style
//! stateful model in addition to their unit tests. This is that model for
//! `lib/caps`: a randomised sequence of mutating commands is replayed against
//! the live [`CapabilitySet`] and an independent reference model (a
//! `BTreeSet<u16>`), and after every command the two are checked for
//! agreement on membership, cardinality, ordering, and the delegation
//! invariant. A second model signs real Ed25519 [`CapabilityToken`]s and
//! checks [`CapabilityToken::verify`] against a reference oracle of its
//! documented error precedence.
//!
//! Unlike the §19.6 fuzz harnesses — which hammer raw bytes looking for
//! crashes — this model generates *structured* command sequences and lets
//! proptest **shrink** any counterexample to a minimal failing program.
//!
//! ## Wall-clock budget (`AGENTS.md` §19.7)
//!
//! The shared `rustos_fuzzseed::prop::drive` runner owns the seed/budget
//! policy (one definition, §2.2): a plain `cargo test` runs [`SMOKE_CASES`]
//! sequences **once** from a fresh, logged seed; `cargo xtask proptest --soak`
//! exports `RUSTOS_PROPTEST_BUDGET_SECS` and the runner keeps drawing
//! [`BUDGET_BATCH_CASES`] batches off the same continuing RNG until the
//! deadline. The seed is logged at the start of each run (and pinnable via
//! `--seed`), so a fresh-seed counterexample is still reproducible (§2.1).

use std::collections::BTreeSet;

use ed25519_dalek::{Signer, SigningKey};
use proptest::prelude::*;
use proptest::test_runner::TestCaseError;
use rustos_abi::{CapabilityId, Errno, ABI_VERSION_CURRENT};
use rustos_caps::{CapabilitySet, CapabilityToken, RevocationEpoch};
use rustos_crypto::{Ed25519PublicKey, Ed25519Signature};

/// Sequences run once by a plain `cargo test` (no budget set).
const SMOKE_CASES: u32 = 256;

/// Sequences per batch under a wall-clock budget; the batch is repeated
/// until the deadline so the model keeps drawing fresh programs.
const BUDGET_BATCH_CASES: u32 = 512;

/// Highest capability id the model draws from. Spans the well-known
/// `abi-v1` ids plus headroom so raw-id construction is exercised too.
const CAP_MAX: u16 = 20;

/// All capability ids `[0, CAP_MAX]` are valid by construction.
fn cap(id: u16) -> CapabilityId {
    CapabilityId::from_raw(id).expect("id within CAPABILITY_ID_MAX")
}

/// Build a [`CapabilitySet`] from a list of ids.
fn build(ids: &[u16]) -> CapabilitySet {
    let mut s = CapabilitySet::empty();
    for &id in ids {
        s.insert(cap(id));
    }
    s
}

/// One mutating or observing operation on the set under test.
#[derive(Clone, Debug)]
enum Cmd {
    Insert(u16),
    Remove(u16),
    Revoke(u16),
    Delegate(Vec<u16>),
    CheckAgainst(Vec<u16>),
}

fn id() -> impl Strategy<Value = u16> {
    0u16..=CAP_MAX
}

fn id_vec() -> impl Strategy<Value = Vec<u16>> {
    prop::collection::vec(id(), 0..=8)
}

fn command() -> impl Strategy<Value = Cmd> {
    prop_oneof![
        id().prop_map(Cmd::Insert),
        id().prop_map(Cmd::Remove),
        id().prop_map(Cmd::Revoke),
        id_vec().prop_map(Cmd::Delegate),
        id_vec().prop_map(Cmd::CheckAgainst),
    ]
}

fn program() -> impl Strategy<Value = Vec<Cmd>> {
    prop::collection::vec(command(), 0..=64)
}

#[test]
fn capability_set_tracks_reference_model() {
    rustos_fuzzseed::prop::drive(
        "capability_set_tracks_reference_model",
        SMOKE_CASES,
        BUDGET_BATCH_CASES,
        program(),
        |cmds| {
            let mut live = CapabilitySet::empty();
            let mut model: BTreeSet<u16> = BTreeSet::new();

            for c in &cmds {
                match c {
                    Cmd::Insert(i) => {
                        live.insert(cap(*i));
                        model.insert(*i);
                    }
                    Cmd::Remove(i) => {
                        live.remove(cap(*i));
                        model.remove(i);
                    }
                    Cmd::Revoke(i) => {
                        let was = live.revoke(cap(*i));
                        let model_was = model.remove(i);
                        prop_assert_eq!(was, model_was, "revoke must report prior membership");
                    }
                    Cmd::Delegate(req) => {
                        let requested = build(req);
                        let req_model: BTreeSet<u16> = req.iter().copied().collect();
                        let res = live.delegate(&requested);
                        if req_model.is_subset(&model) {
                            let granted = match res {
                                Ok(g) => g,
                                Err(e) => {
                                    return Err(TestCaseError::fail(format!(
                                        "a subset delegation was refused: {e:?}"
                                    )))
                                }
                            };
                            // Delegation returns exactly the requested subset and
                            // never widens the parent's authority (§5.2).
                            prop_assert_eq!(granted, requested);
                            prop_assert!(granted.is_subset_of(&live));
                        } else {
                            prop_assert_eq!(res, Err(Errno::DelegationWiden));
                        }
                    }
                    Cmd::CheckAgainst(other) => {
                        let rhs = build(other);
                        let rhs_model: BTreeSet<u16> = other.iter().copied().collect();
                        let union = live.union(&rhs);
                        let inter = live.intersection(&rhs);
                        for k in 0..=CAP_MAX {
                            let in_live = model.contains(&k);
                            let in_rhs = rhs_model.contains(&k);
                            prop_assert_eq!(union.contains(cap(k)), in_live || in_rhs);
                            prop_assert_eq!(inter.contains(cap(k)), in_live && in_rhs);
                        }
                        prop_assert!(live.is_subset_of(&union));
                        prop_assert!(inter.is_subset_of(&live));
                        prop_assert_eq!(live.is_subset_of(&rhs), model.is_subset(&rhs_model));
                    }
                }

                // Global invariants after every command.
                prop_assert_eq!(live.len() as usize, model.len());
                prop_assert_eq!(live.is_empty(), model.is_empty());
                for k in 0..=CAP_MAX {
                    prop_assert_eq!(live.contains(cap(k)), model.contains(&k));
                }
                let observed: Vec<u16> = live.iter().map(CapabilityId::as_u16).collect();
                let expected: Vec<u16> = model.iter().copied().collect();
                prop_assert_eq!(
                    observed,
                    expected,
                    "iteration must be ascending and complete"
                );
            }
            Ok(())
        },
    );
}

/// Fixed, deterministic authority key. Tests must not depend on RNG for the
/// key material (the signing seed is the constant the unit tests use too).
fn signing_key() -> SigningKey {
    SigningKey::from_bytes(&[42u8; 32])
}

fn authority_key() -> Ed25519PublicKey {
    Ed25519PublicKey::from_bytes(signing_key().verifying_key().as_bytes()).expect("valid key")
}

fn sign(subject: u64, epoch: RevocationEpoch, caps: &CapabilitySet) -> CapabilityToken {
    let body = CapabilityToken::signing_input(ABI_VERSION_CURRENT, subject, epoch, caps);
    let sig = signing_key().sign(&body);
    CapabilityToken {
        abi_version: ABI_VERSION_CURRENT,
        subject,
        epoch,
        caps: *caps,
        signature: Ed25519Signature::from_bytes(sig.to_bytes()),
    }
}

#[test]
fn token_verify_matches_error_precedence_oracle() {
    // The token is always issued to subject 7; the verifier checks against
    // a subject drawn from a small range so the mismatch path is exercised.
    const ISSUE_SUBJECT: u64 = 7;
    let strategy = (id_vec(), id_vec(), 0u64..4, 0u64..4, any::<bool>(), 6u64..9);
    rustos_fuzzseed::prop::drive(
        "token_verify_matches_error_precedence_oracle",
        SMOKE_CASES,
        BUDGET_BATCH_CASES,
        strategy,
        |(caps_ids, parent_ids, issue_epoch, verify_epoch, tamper, verify_subject)| {
            let caps = build(&caps_ids);
            let parent = build(&parent_ids);
            let mut token = sign(ISSUE_SUBJECT, RevocationEpoch(issue_epoch), &caps);
            if tamper {
                let mut bytes = *token.signature.as_bytes();
                bytes[0] ^= 0x01;
                token.signature = Ed25519Signature::from_bytes(bytes);
            }

            let res = token.verify(
                &authority_key(),
                &parent,
                RevocationEpoch(verify_epoch),
                verify_subject,
            );

            // Mirror of `CapabilityToken::verify`'s documented precedence:
            // abi version (always current here) → epoch → subject →
            // signature → subset. A stale epoch and a foreign subject are
            // both reported as `NotFound`, so they share a branch.
            let expected = if issue_epoch != verify_epoch || verify_subject != ISSUE_SUBJECT {
                Err(Errno::NotFound)
            } else if tamper {
                Err(Errno::SignatureInvalid)
            } else if !caps.is_subset_of(&parent) {
                Err(Errno::DelegationWiden)
            } else {
                Ok(())
            };
            prop_assert_eq!(res, expected);
            Ok(())
        },
    );
}
