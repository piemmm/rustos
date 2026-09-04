//! Boot-time aggregation of the system-wide common CPU-feature set.
//!
//! Every process is handed, in its startup vector
//! ([`tairix_abi::process::ProcessStart::cpu_features`]), the set of CPU
//! instruction-set extensions common to **every** core it may run on. Its
//! runtime resolves its self-optimising accelerated routines (`lib/cpuops` /
//! `lib/crc32c`) against that set: because it is the *intersection* over all
//! cores, any instruction the set advertises is legal on every core the
//! scheduler may migrate the task to, so a dispatched routine can never trap
//! after a migration.
//!
//! # Why an intersection folded per core
//!
//! A core can only read its *own* ID registers, so the intersection cannot be
//! computed from the boot CPU alone: each CPU folds the feature set it detects
//! for itself into a running AND as it comes online (the boot CPU in
//! [`crate::kernel_main`], each secondary in [`crate::run_secondary`]). Until
//! **every** expected core has contributed, [`system_features`] returns the
//! empty set — a program that resolves against it uses the portable baseline
//! everywhere, which is always correct (fail closed). The delivered value only
//! ever *grows* toward the true intersection as cores report in, never
//! advertises a feature some core lacks.
//!
//! The value is a non-secret capability fact (which instructions the silicon
//! implements), so publishing it to user space grants no authority.

use core::sync::atomic::{AtomicU32, AtomicU64, Ordering};

use tairix_abi::cpufeatures::CpuFeatureSet;
use tairix_cpuops::{Decision, DecisionReason};
use tairix_log::{Field, FieldValue, Level, Sink};

use crate::audit::{emit, AuditEvent};

/// The running intersection of every contributed core's feature set.
///
/// Initialised to all-ones (the identity for AND); the first real
/// contribution clears every bit no core sets — including the unused reserved
/// bit positions — so once finalised it holds exactly the features present on
/// *all* cores.
static INTERSECTION: AtomicU64 = AtomicU64::new(u64::MAX);

/// How many cores have folded their feature set in so far.
static CONTRIBUTED: AtomicU32 = AtomicU32::new(0);

/// How many cores must contribute before the intersection is final. Zero
/// means "no CPU-feature detection is available" (a port without the HAL
/// slice, or a host build), so the empty set is delivered forever.
static EXPECTED: AtomicU32 = AtomicU32::new(0);

/// Declare how many cores will contribute before the common set is final.
///
/// Called once at boot, before the boot CPU or any secondary folds its set in,
/// only when the port exposes a CPU-feature detector. A port without one never
/// calls this, so [`system_features`] stays the empty set (fail closed).
pub fn expect_contributions(cpu_count: u32) {
    EXPECTED.store(cpu_count, Ordering::Release);
}

/// Fold one core's detected feature set into the running intersection.
///
/// Each core calls this exactly once, with the set it detected for *itself*
/// (a core can only read its own ID registers). Safe to call from any CPU:
/// the AND and the count are atomic, and the count is incremented only after
/// the AND is published, so a reader that observes the final count also
/// observes every fold.
pub fn contribute(features: CpuFeatureSet) {
    INTERSECTION.fetch_and(features.bits(), Ordering::AcqRel);
    CONTRIBUTED.fetch_add(1, Ordering::AcqRel);
}

/// The migration-safe common CPU-feature set, or the empty set until every
/// expected core has contributed (fail closed).
///
/// This is the value stamped into each spawned process's startup vector.
#[must_use]
pub fn system_features() -> CpuFeatureSet {
    let expected = EXPECTED.load(Ordering::Acquire);
    if expected == 0 || CONTRIBUTED.load(Ordering::Acquire) < expected {
        return CpuFeatureSet::EMPTY;
    }
    CpuFeatureSet::from_bits(INTERSECTION.load(Ordering::Acquire))
}

/// Select the self-optimising accelerated-routine implementations for this
/// boot against the finalised common feature set, record each choice on the
/// audit log, and run the boot-time cryptographic power-on self-test.
///
/// Called once from [`crate::kernel_main`] after every core has contributed
/// (so [`system_features`] is final). Four families resolve here:
///
/// - **CRC-32C**: ARXFS's fast physical-integrity checksum runs through it
///   in-kernel, so on a core with the `crc32c*` / SSE4.2 instruction the
///   checksum uses the hardware path (self-verified bit-identical to the
///   portable baseline before it can be selected).
/// - **Page-zero**: the frame scrub (zero-before-map and the zero-on-free
///   secret scrub, `kernel/mem`) runs through it, so on a core with a
///   block-zero instruction (`DC ZVA` / ERMS) it uses the hardware path
///   (self-verified bit-identical to the portable byte fill before it can be
///   selected). A pure capability decision, never benchmarked.
/// - **Hash-table group scan**: every `lib/collections` hash container probes
///   through it, so on a core with a vector unit the sixteen-lane control
///   scan is one comparison rather than tens of scalar operations
///   (self-verified bit-identical to the portable scan before it can be
///   selected). Never benchmarked: the control bytes it reads are tags
///   derived from the per-boot hash key.
/// - **Crypto (SHA-256) backend availability**: an availability-only decision
///   whose self-verify is a FIPS known-answer self-test of the live SHA-256
///   path (`tairix_crypto::backend`). Never benchmarked (a benchmark must not
///   choose a key-timing-leaky crypto variant).
///
/// Routine selection never fails: it falls closed to the portable baseline.
/// The crypto self-test *can* fail if the audited primitive computes a wrong
/// answer — a fatal, unrecoverable boot condition (running with broken
/// cryptography is never acceptable), so this returns `false` in that case and
/// [`crate::kernel_main`] halts. Every other outcome returns `true`.
#[must_use = "the kernel must halt when the crypto power-on self-test fails"]
pub fn resolve_accelerated_ops(audit: &dyn Sink) -> bool {
    let features = system_features();
    record(audit, &tairix_crc32c::resolve(features));
    record(audit, &tairix_pagezero::resolve(features));
    record(audit, &tairix_collections::group::resolve(features));

    let crypto = tairix_crypto::backend::resolve(features);
    record(audit, &crypto);
    if tairix_crypto::backend::self_test_passed(&crypto) {
        true
    } else {
        emit(
            audit,
            Level::Error,
            AuditEvent::CryptoSelfTestFailed,
            &[Field {
                key: "family",
                value: FieldValue::Str(crypto.family.0),
            }],
        );
        false
    }
}

/// Emit one [`AuditEvent::CpuOpsRoutineSelected`] record for `decision`.
fn record(audit: &dyn Sink, decision: &Decision) {
    emit(
        audit,
        Level::Info,
        AuditEvent::CpuOpsRoutineSelected,
        &[
            Field {
                key: "family",
                value: FieldValue::Str(decision.family.0),
            },
            Field {
                key: "chosen",
                value: FieldValue::Str(decision.chosen),
            },
            Field {
                key: "reason",
                value: FieldValue::Str(reason_label(decision.reason)),
            },
        ],
    );
}

/// A stable, terse label for a [`DecisionReason`] (the audit `reason` field).
const fn reason_label(reason: DecisionReason) -> &'static str {
    match reason {
        DecisionReason::Priority => "priority",
        DecisionReason::Benchmark => "benchmark",
        DecisionReason::Pinned => "pinned",
        DecisionReason::PinRejected => "pin_rejected",
        DecisionReason::Baseline => "baseline",
        DecisionReason::BaselineUnverified => "baseline_unverified",
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use tairix_abi::cpufeatures::CpuFeature;

    // The aggregation reads process-global atomics, so the whole lifecycle is
    // exercised in one test to keep the shared state deterministic (no other
    // test in this module touches it).
    #[test]
    fn intersection_is_empty_until_all_cores_contribute_then_is_the_and() {
        // No detector declared → always empty (fail closed).
        assert_eq!(system_features(), CpuFeatureSet::EMPTY);

        expect_contributions(3);
        // Not yet finalised.
        assert_eq!(system_features(), CpuFeatureSet::EMPTY);

        // Core 0: {Crc32, Sha2, Aes}. Core 1: {Crc32, Sha2}. Core 2:
        // {Crc32, Aes}. The intersection is {Crc32}.
        contribute(
            CpuFeatureSet::new()
                .with(CpuFeature::Crc32)
                .with(CpuFeature::Sha2)
                .with(CpuFeature::Aes),
        );
        assert_eq!(system_features(), CpuFeatureSet::EMPTY, "still one short");
        contribute(
            CpuFeatureSet::new()
                .with(CpuFeature::Crc32)
                .with(CpuFeature::Sha2),
        );
        assert_eq!(system_features(), CpuFeatureSet::EMPTY, "still one short");
        contribute(
            CpuFeatureSet::new()
                .with(CpuFeature::Crc32)
                .with(CpuFeature::Aes),
        );

        let common = system_features();
        assert!(common.contains(CpuFeature::Crc32));
        assert!(!common.contains(CpuFeature::Sha2));
        assert!(!common.contains(CpuFeature::Aes));
        // Reserved / unused bit positions are cleared, not left set from the
        // all-ones identity.
        assert_eq!(
            common.bits(),
            CpuFeatureSet::new().with(CpuFeature::Crc32).bits()
        );
    }
}
