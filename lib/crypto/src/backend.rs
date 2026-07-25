//! The authoritative crypto backend-availability decision and the boot-time
//! cryptographic known-answer self-test (POST).
//!
//! TAIRiX ships **one generic image per architecture** compiled against a
//! conservative baseline (no `+aes`/`+sha2` build-time floor), and recovers
//! the acceleration a booted CPU actually offers at runtime from its single
//! authoritative feature detector (the Arch HAL `cpufeatures` slice, read from
//! `ID_AA64ISAR0_EL1` / `CPUID` / `misa`). Every accelerated operation makes
//! that recovery through the one generic dispatch framework (`lib/cpuops`), and
//! crypto is no exception — with one absolute restriction the framework
//! enforces: **crypto is decided on availability only, never benchmarked**.
//! A "fastest AES" or "fastest SHA" benchmark would happily pick a table-driven
//! variant that leaks keys through cache timing, so the crypto family is always
//! [`Selection::ByPriority`] and the self-verify runs the real primitive against
//! fixed known answers, never a timing measurement.
//!
//! # What this module owns, and what it deliberately does not
//!
//! The charter forbids hand-rolling cryptographic primitives: both the
//! "hardware" and "software" SHA-256 paths are the *same audited* `sha2` crate,
//! which selects its own compression backend internally. On `x86_64` `sha2`
//! chooses its SHA-NI path from `CPUID` — a detection that needs no operating
//! system, so it is already correct on the freestanding kernel target. On
//! `aarch64` `sha2`'s hardware path is gated by `HWCAP`, which yields nothing
//! without an OS, so it stays on software there; TAIRiX cannot override that
//! internal gate without transcribing the SHA-256 round function over
//! intrinsics itself — i.e. hand-rolling the primitive, which the charter
//! forbids. Recovering hardware SHA-256 on `aarch64` therefore waits on a
//! vetted, driveable audited backend (a supply-chain decision), and until then
//! this module records the honest `Software` answer there rather than a
//! backend that does not run.
//!
//! So this module does **not** fork the crypto computation (that lives inside
//! the audited crate). What it owns is the part TAIRiX must own to be *better*
//! than a scatter of per-crate ad-hoc detection:
//!
//! 1. **One authoritative availability decision.** Which backend is active is
//!    derived from TAIRiX's single feature detector — not each crate's private,
//!    bare-metal-broken detection — and expressed through the uniform
//!    `lib/cpuops` registry so it is observable and consistent with every other
//!    accelerated family.
//! 2. **A mandatory boot-time known-answer self-test (POST).** The framework's
//!    self-verify *is* the self-test: before the availability decision is
//!    trusted, the live SHA-256 path is run over FIPS 180-4 §A.1 vectors and
//!    compared to their published digests. A crypto core that computes a wrong
//!    answer cannot be reported as working — it drives a fatal boot halt in the
//!    kernel, exactly as a FIPS power-on self-test failure renders the module
//!    inoperable rather than letting it run with broken crypto.
//! 3. **An audit record.** The kernel records the typed [`Decision`] this
//!    returns, so the active crypto backend for the boot is on the audit log.
//!
//! The result is never a panic and never a benchmark; a target with no
//! runtime-selected hardware path resolves to the audited constant-time
//! software backend, which is a real, correct primitive and always the last
//! resort (fail closed).

#[cfg(crypto_hw_sha256)]
use tairix_abi::cpufeatures::CpuFeature;
use tairix_abi::cpufeatures::CpuFeatureSet;
use tairix_cpuops::{
    Candidate, CoreKey, Decision, DecisionReason, Family, FamilyId, Selection, Selector,
};

use crate::hash::{sha256, Sha256Digest};

/// Which audited backend the SHA-256 primitive resolves to on a core.
///
/// This is the [`Candidate`] handle the [`Selector`] carries for the crypto
/// family; it names the *availability tier* the boot's feature set admits, not
/// two forked computations (the audited `sha2` crate owns the actual
/// compression backend — see the module docs). It exists so the audit record
/// and any future consumer can name the active backend without a magic string.
#[derive(Copy, Clone, Debug, Eq, PartialEq)]
pub enum CryptoBackend {
    /// A CPU-instruction-accelerated backend is available and used (today only
    /// where the audited crate's own no-OS-safe detection selects it — see the
    /// module docs).
    Hardware,
    /// The audited constant-time software backend — always correct, always
    /// feature-legal, and the fail-closed last resort.
    Software,
}

/// The crypto SHA-256 family's stable id — the `lib/cpuops` log/pin key.
pub const SHA256_FAMILY_ID: FamilyId = FamilyId("crypto-sha256");

/// Stable name of the hardware-availability candidate (the log key).
pub const SHA256_HW_NAME: &str = "sha256-hw";

/// Stable name of the software baseline (the log key and fail-closed default).
pub const SHA256_SW_NAME: &str = "sha256-soft";

/// One boot-time self-test vector: an input and its published digest.
///
/// `expect` is a *published* known answer (FIPS 180-4 §A.1), not a value this
/// crate computed — so the self-verify checks the live SHA-256 path against an
/// external oracle, which is what makes it a genuine power-on self-test rather
/// than a tautology.
struct Sha256Kat {
    input: &'static [u8],
    expect: Sha256Digest,
}

/// The FIPS 180-4 §A.1 SHA-256 known-answer vectors (the empty message and
/// `"abc"`). Both are the exact digests `lib/crypto`'s own hashing unit tests
/// pin, so the boot self-test and the crate's tests can never disagree.
const KATS: &[Sha256Kat] = &[
    Sha256Kat {
        input: b"",
        expect: [
            0xe3, 0xb0, 0xc4, 0x42, 0x98, 0xfc, 0x1c, 0x14, 0x9a, 0xfb, 0xf4, 0xc8, 0x99, 0x6f,
            0xb9, 0x24, 0x27, 0xae, 0x41, 0xe4, 0x64, 0x9b, 0x93, 0x4c, 0xa4, 0x95, 0x99, 0x1b,
            0x78, 0x52, 0xb8, 0x55,
        ],
    },
    Sha256Kat {
        input: b"abc",
        expect: [
            0xba, 0x78, 0x16, 0xbf, 0x8f, 0x01, 0xcf, 0xea, 0x41, 0x41, 0x40, 0xde, 0x5d, 0xae,
            0x22, 0x23, 0xb0, 0x03, 0x61, 0xa3, 0x96, 0x17, 0x7a, 0x9c, 0xb4, 0x10, 0xff, 0x61,
            0xf2, 0x00, 0x15, 0xad,
        ],
    },
];

/// Run the live SHA-256 path over one self-test vector (the `lib/cpuops` `run`
/// adapter). Both candidates route through the one audited [`sha256`]; the
/// [`CryptoBackend`] handle names the availability tier, it does not fork the
/// computation (module docs).
fn run(_backend: CryptoBackend, kat: &Sha256Kat) -> Sha256Digest {
    sha256(kat.input)
}

/// The published known answer for one vector (the `lib/cpuops` `reference`
/// adapter): the external oracle the live path is checked against.
fn reference(kat: &Sha256Kat) -> Sha256Digest {
    kat.expect
}

/// The accelerated-availability candidate on this build: present only where the
/// audited crate's own no-OS-safe detection selects a hardware backend (today
/// `x86_64`, via the `crypto_hw_sha256` build cfg). It requires the exact
/// feature bits `sha2` gates its SHA-NI path on — SHA-NI plus the SSSE3/SSE4.2
/// (which implies SSE4.1) prerequisites — so the recorded availability matches
/// what the crate will actually run.
#[cfg(crypto_hw_sha256)]
const CANDIDATES: &[Candidate<CryptoBackend>] = &[Candidate {
    name: SHA256_HW_NAME,
    requires: &[
        CpuFeature::ShaNi,
        CpuFeature::Sse42,
        CpuFeature::Ssse3,
        CpuFeature::Sse2,
    ],
    impl_: CryptoBackend::Hardware,
}];
#[cfg(not(crypto_hw_sha256))]
const CANDIDATES: &[Candidate<CryptoBackend>] = &[];

/// Build the `lib/cpuops` family describing the SHA-256 backend-availability
/// decision: its candidate(s), the mandatory software baseline, and the
/// known-answer self-test the framework runs before trusting the choice.
///
/// The selection policy is **always** [`Selection::ByPriority`] — a crypto
/// decision is availability, never speed (a benchmark must never choose a
/// key-timing-leaky variant).
fn family() -> Family<'static, CryptoBackend, Sha256Kat, Sha256Digest> {
    Family {
        id: SHA256_FAMILY_ID,
        selection: Selection::ByPriority,
        candidates: CANDIDATES,
        baseline: Candidate {
            name: SHA256_SW_NAME,
            requires: &[],
            impl_: CryptoBackend::Software,
        },
        reference,
        run,
        vectors: KATS,
    }
}

/// Decide the SHA-256 backend for a core with the given `features` and run the
/// boot-time known-answer self-test as part of the decision.
///
/// Returns the typed [`Decision`] for the caller to record on the audit log
/// (§19.4). The framework filters the hardware candidate on the delivered
/// feature bits, runs the FIPS known-answer self-test over the surviving path,
/// and falls closed to the software baseline. It never benchmarks (the family
/// is `ByPriority`), never panics, and never busy-waits.
///
/// Use [`self_test_passed`] on the returned decision to gate trust: a
/// `BaselineUnverified` decision means even the software baseline failed the
/// known-answer test — the crypto core is broken and must not be trusted.
#[must_use = "record the Decision on the audit log and check self_test_passed"]
pub fn resolve(features: CpuFeatureSet) -> Decision {
    // A crypto family is availability-only, so no benchmark harness is ever
    // passed (`None`); the `CoreKey` keys the log record to this feature set.
    Selector::new()
        .select(&family(), features, CoreKey(features.bits()), None)
        .decision
}

/// Whether the boot-time cryptographic known-answer self-test passed.
///
/// `false` only when the software baseline itself failed the FIPS known-answer
/// vectors (`DecisionReason::BaselineUnverified`) — i.e. the audited SHA-256
/// primitive is computing wrong answers. The kernel treats that as a fatal,
/// unrecoverable boot condition: it must not run with broken cryptography.
/// Every other outcome (hardware selected, or the software baseline used) means
/// the live path reproduced the published digests and is trustworthy.
#[must_use]
pub fn self_test_passed(decision: &Decision) -> bool {
    decision.reason != DecisionReason::BaselineUnverified
}

#[cfg(test)]
mod tests {
    use super::*;

    /// With no feature bits the software baseline is selected and the boot
    /// self-test passes (the audited software SHA-256 reproduces the FIPS
    /// vectors).
    #[test]
    fn resolve_falls_closed_to_software_without_features() {
        let decision = resolve(CpuFeatureSet::EMPTY);
        assert_eq!(decision.chosen, SHA256_SW_NAME);
        assert_eq!(decision.reason, DecisionReason::Baseline);
        assert!(self_test_passed(&decision));
    }

    /// On a build with the hardware-availability candidate, presenting its
    /// feature bits selects it by priority (still self-verified against the
    /// FIPS answers); on a build without one the software baseline stands.
    /// Either way the self-test passes — the point of the known-answer gate.
    #[test]
    fn hardware_availability_recorded_when_its_features_are_present() {
        let decision = resolve(hw_feature_set());
        assert!(self_test_passed(&decision));
        if CANDIDATES.is_empty() {
            assert_eq!(decision.chosen, SHA256_SW_NAME);
            assert_eq!(decision.reason, DecisionReason::Baseline);
        } else {
            assert_eq!(decision.chosen, SHA256_HW_NAME);
            assert_eq!(decision.reason, DecisionReason::Priority);
        }
    }

    /// The live SHA-256 path reproduces every published known answer — the
    /// self-test the framework runs, asserted directly so a broken primitive
    /// fails the crate's own tests, not only boot selection.
    #[test]
    fn every_kat_matches_the_published_answer() {
        for kat in KATS {
            assert_eq!(run(CryptoBackend::Software, kat), reference(kat));
        }
    }

    /// The crypto family is availability-only: it must never be benchmarked
    /// (a benchmark could pick a key-timing-leaky variant).
    #[test]
    fn crypto_family_is_never_benchmarked() {
        assert_eq!(family().selection, Selection::ByPriority);
    }

    /// The feature set that admits this build's hardware candidate (empty on a
    /// build without one, so the assertion above degrades to the baseline).
    fn hw_feature_set() -> CpuFeatureSet {
        #[cfg(crypto_hw_sha256)]
        {
            CpuFeatureSet::new()
                .with(CpuFeature::ShaNi)
                .with(CpuFeature::Sse42)
                .with(CpuFeature::Ssse3)
                .with(CpuFeature::Sse2)
        }
        #[cfg(not(crypto_hw_sha256))]
        {
            CpuFeatureSet::EMPTY
        }
    }
}
