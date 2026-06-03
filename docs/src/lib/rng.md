# `rustos-rng`

The single place RustOS gets randomness. It separates three generators
by purpose so the wrong one is hard to reach for by accident:

| Type                     | Kind                        | Use it for |
|--------------------------|-----------------------------|------------|
| `CsRng`                  | Cryptographically secure    | Keys, nonces, the swap key (§4), the KASLR/ASLR seed (§19.2), capability material |
| `FastRng`                | Fast, **non**-cryptographic | Scheduler decisions, collection seeds, backoff jitter, fuzzing |
| `hardware::PlatformFast` | Fast, hardware-preferring   | The same fast uses, when a motherboard hardware RNG is present |

## The cryptographic core is composed, not hand-rolled

`CsRng` is a NIST SP 800-90Ar1 **HMAC-SHA256 DRBG** (`drbg::HmacDrbg`):
Instantiate, Reseed, Generate, and the internal Update, built entirely
over `lib/crypto`'s audited `hmac_sha256`. Per `AGENTS.md` §1/§2.12 no
cryptographic primitive is hand-rolled here — HMAC *is* the conditioner,
so the DRBG needs no derivation function of its own, exactly as
`lib/crypto::kdf` layers single-block HKDF-Expand over the same PRF. The
only new code added to `lib/crypto` is `hmac_sha256_parts`, a multi-part
form of the existing HMAC wrapper that lets the DRBG hash
`Key ‖ V ‖ separator ‖ data` without an allocator.

`HmacDrbg` zeroes its `Key`/`V` working state on drop (§4) and its
`Debug` redacts it. Every `generate` call ends with an Update, so the
state that produced a block cannot be recovered from the post-call state
(backtracking resistance); *prediction* resistance comes from reseeding.

## Fallible by construction

A draw may trigger a reseed, and a reseed needs fresh entropy that can be
momentarily unavailable. Rather than block, spin, or panic (§2.1, §2.9),
`CsRng`'s draws return `Result<_, EntropyError>` and the caller fails
closed (§5.4). `CsRng` reseeds automatically every
`DEFAULT_RESEED_INTERVAL` draws — far below the DRBG's hard `2^48`
reseed limit — buying forward secrecy cheaply.

## Entropy seam and the hardware RNG

`EntropySource` is the one seam through which raw entropy enters. Platform
sources implement it without naming an architecture, so the crate stays
architecture-neutral (§17.2): the concrete probing (x86 `RDRAND`/`RDSEED`,
ARMv8.5 `RNDR`, RISC-V `Zkr`, virtio-rng) lives in `kernel/arch/<target>`
or a `drivers/*` crate, never here. `CombinedSource` XOR-mixes several
sources so the pool never trusts a single — possibly weak or backdoored —
source alone; XOR is entropy-preserving for independent inputs.

A motherboard hardware RNG (`hardware::HardwareRng`) plays both roles the
issue calls for:

1. **Extra entropy.** `HardwareEntropy` adapts it to `EntropySource`, so
   it is one input among several feeding `CsRng`, never the sole one.
2. **A fast source.** `hardware::PlatformFast` draws fast `u64`s from the
   hardware directly when present, and falls back to the software
   `FastRng` when it is absent or momentarily fails — there is no
   busy-retry-until-it-works loop (§2.1).

## Fast, non-cryptographic generator

`FastRng` is xoshiro256++ (Blackman & Vigna), seeded via SplitMix64. It is
an ordinary PRNG, not a security primitive (§2.12), so rolling it
ourselves is allowed and adds no dependency. Its state is recoverable from
output, so it must never produce keys or nonces. `RandU64` carries the
shared, generator-independent sampling logic — byte filling and Lemire's
unbiased bounded integers — once, so no consumer re-derives it (§2.2).

## Test vectors

* HMAC-DRBG: the NIST SP 800-90Ar1 CAVP `[SHA-256]` vector (prediction
  resistance false, reseed enabled, additional input) exercising
  Instantiate + Reseed + two Generate calls against the published
  1024-bit output.
* `hmac_sha256_parts`: equality against the single-shot HMAC over the
  joined parts, for several splits.
* SplitMix64: the published reference outputs for seed `0`.
* `RandU64::next_below`: range and rejection-zone checks for Lemire's
  method, plus a deterministic uniformity histogram.
* Entropy combination: XOR equivalence, dead-source skipping, and the
  all-sources-failed fail-closed result.
* Hardware paths: hardware-backed entropy seeding, hardware-preferring
  fast draws, and the software fallback on absence or transient failure.
* Statistical balance: deterministic mean and per-bit-position checks
  over 1 MiB of `FastRng` and `CsRng` output (fixed seed — never flaky).
