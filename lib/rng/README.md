# tairix-rng

Random number generation for TAIRiX. One crate, three generators, each named
for the **property** that decides whether a call site may use it — not for how
fast it is — so the wrong tool is hard to reach for by accident:

| Type            | Property                      | Use it for |
|-----------------|-------------------------------|------------|
| `CsRng`         | Cryptographically secure      | Long-lived key material: `ARXFS` volume keys, the swap key, the KASLR/ASLR seed |
| `FastRng`       | Fast **and unpredictable**    | Anything that must not be guessable but is not long-lived key material: task ids, network payloads, the kernel output reserve |
| `NonCryptoRng`  | Fast and **predictable**      | Decorrelation and reproducible fixtures: per-CPU work-stealing scan starts, seeded test streams |

The split matters because *statistical quality and unpredictability are
unrelated properties*. `NonCryptoRng` (xoshiro256++) passes the full
BigCrush/PractRand batteries and is still trivially invertible — four
consecutive outputs carry its whole 256-bit state, and recovering it is
arithmetic rather than cryptanalysis. A type named only for its speed would
say nothing about which of the two properties it has, which is how bulk
randomness ends up drawn from an invertible generator.

## Design

- **`CsRng`** is a NIST SP 800-90Ar1 **HMAC-SHA256 DRBG** (`drbg::HmacDrbg`)
  that reseeds from a pluggable `EntropySource` on a fixed schedule (forward
  secrecy). Its working state is zeroed on drop and its draws are fallible and
  fail closed — they never block, spin, or panic.
- **`FastRng`** is buffered **ChaCha12 with fast key erasure** — Bernstein's
  construction, as used by OpenBSD `arc4random` and Linux `get_random_u64`.
  One refill runs the cipher once: the first 32 keystream bytes *become the
  key* and the rest fill the issue buffer, so the key behind already-issued
  output exists nowhere (backtracking resistance) and each byte is wiped as it
  is consumed. Roughly ten times the cost of xoshiro per byte and forty times
  cheaper than the DRBG, which is why everything that should be unpredictable
  can afford it. Prediction resistance needs fresh entropy and is therefore
  the owner's job: `perturb_due` reports the cadence and `perturb` XOR-folds
  32 fresh bytes into the key, XOR so a dead or hostile source can never
  *lower* its quality.
- **`NonCryptoRng`** is xoshiro256++ seeded via SplitMix64 — an ordinary
  non-cryptographic PRNG, not a security primitive, so implementing it here is
  not hand-rolled cryptography.
- **`OutputReserve`** is the kernel's bounded reserve of CSPRNG output, and it
  is the whole chain in one type: `entropy pool → CsRng → FastRng<2048> →
  userland`. Serving a userland request costs a cipher block rather than a
  DRBG generate, and every 1 MiB of output the reserve reseeds the DRBG from
  the pool and folds fresh entropy into the cipher key.
- **`EntropySource`** is the seam through which raw entropy enters. Platform
  sources (a hardware RNG, timing jitter, an interrupt pool) implement it
  without naming an architecture, so this crate stays architecture-neutral.
  `CombinedSource` XOR-mixes several so the pool never trusts a single —
  possibly weak or backdoored — source alone.
- **Hardware RNG** (`hardware::HardwareRng`, supplied by
  `kernel/arch/<target>` or a `drivers/*` crate, never probed here) is entropy
  **input and nothing else**: `HardwareEntropy` adapts it to `EntropySource`
  for the mix. Its bytes never reach a caller unconditioned — a vendor RNG
  could be weak or backdoored, and it is slower than `FastRng` anyway.

## Cryptography is composed, not hand-rolled

Both secure generators are standard constructions over `lib/crypto`'s audited
primitives: HMAC-DRBG over `hmac_sha256`, fast key erasure over
`chacha12_keystream`. No cryptographic primitive is written in this crate.

## Tests

Unit tests live next to the code. The load-bearing ones are structural rather
than statistical, because no statistical test can tell a good PRNG from true
randomness: the key-erasure split asserted against the raw cipher's keystream,
backtracking resistance, zeroise-on-consume, XOR-folding that cannot degrade a
key, `const` construction for a `static`, and `Debug` that elides key and
buffer. Alongside them: the NIST CAVP HMAC-DRBG known-answer vector, the
SplitMix64 reference vector, Lemire bounded-sampling bias/range checks, the
entropy-combination and fail-closed paths, and the reserve's seed / refill /
discard / reseed boundaries.

The outer statistical check — nine SP 800-22 tests over the bytes the two
unpredictable generators actually emit, each held against a known-bad
generator it must reject — lives in `tests/integration/rng_soak` and runs both
as a fixed-seed pass in the PR gate and as `cargo xtask rngsoak` nightly.

## Stability

**experimental.** The public API may change until the first tagged release;
nothing here is frozen yet.
