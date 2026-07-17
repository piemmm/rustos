# tairix-rng

Random number generation for TAIRiX. One crate, three layers, separated by
purpose so the wrong tool is hard to reach for by accident:

| Type                      | Kind                         | Use it for |
|---------------------------|------------------------------|------------|
| `CsRng`                   | Cryptographically secure     | Keys, nonces, swap key (§4), KASLR/ASLR seed (§19.2), capability material |
| `FastRng`                 | Fast, **non**-cryptographic  | Scheduler decisions, collection seeds, backoff jitter, fuzzing |
| `hardware::PlatformFast`  | Fast, hardware-preferring    | The same fast uses, when a motherboard hardware RNG is present |

## Design

- **`CsRng`** is a NIST SP 800-90Ar1 **HMAC-SHA256 DRBG** (`drbg::HmacDrbg`)
  that reseeds from a pluggable `EntropySource` on a fixed schedule (forward
  secrecy). The DRBG is composed entirely over `lib/crypto`'s audited
  HMAC-SHA256 — no cryptographic primitive is hand-rolled here (`AGENTS.md`
  §1, §2.12), mirroring how `lib/crypto::kdf` layers HKDF-Expand over the same
  PRF. Its working state is zeroed on drop (§4) and its draws are fallible and
  fail closed (§5.4) — they never block, spin, or panic (§2.1, §2.9).
- **`EntropySource`** is the seam through which raw entropy enters. Platform
  sources (a hardware RNG, timing jitter, an interrupt pool) implement it
  without naming an architecture, so this crate stays architecture-neutral
  (§17.2). `CombinedSource` XOR-mixes several sources so the pool never trusts
  a single — possibly weak or backdoored — source alone.
- **Hardware RNG** (`hardware::HardwareRng`) plays two roles, supplied by
  `kernel/arch/<target>` (e.g. an `RDRAND` wrapper) or a `drivers/*` crate —
  never probed here:
  1. Extra entropy: `HardwareEntropy` adapts it to `EntropySource` for the mix.
  2. A fast source: `PlatformFast` draws from it directly, falling back to the
     software `FastRng` when it is absent or momentarily fails.
- **`FastRng`** is xoshiro256++ seeded via SplitMix64 — an ordinary
  non-cryptographic PRNG, not a security primitive.

## Tests

Unit tests live next to the code (`AGENTS.md` §7). Coverage includes the NIST
CAVP HMAC-DRBG known-answer vector (instantiate + reseed + generate), the
SplitMix64 reference vector, Lemire bounded-sampling bias/range checks,
entropy-combination and fail-closed behaviour, the hardware fallback paths,
and deterministic statistical balance checks over 1 MiB of output.

## Stability

**experimental.** The public API may change until the first tagged release;
nothing here is frozen yet.
