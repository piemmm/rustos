# FIX-RANDOMNESS — the two-tier random split

Status: **done**

## What this fixed

`lib/rng` drew its fast/slow line on the wrong axis. It offered one fast
generator named `FastRng` (xoshiro256++) and one cryptographic one (`CsRng`,
HMAC-SHA256 DRBG), and the name said only which was *fast* — not which was
*unpredictable*. Since xoshiro is trivially invertible (four consecutive
outputs carry the whole 256-bit state, and recovering it is arithmetic rather
than cryptanalysis) while passing every statistical battery, and since the
DRBG cost ~1500–2000 cycles per `u64`, every consumer wanting bulk randomness
reached for the invertible one — including the process-wide task-id generator.

The generators are now named for the property that decides whether a call site
may use them, and there is a genuinely unpredictable fast one to reach for.

## The three tiers

| Type | Module | Algorithm | For |
|---|---|---|---|
| `NonCryptoRng` | `lib/rng/src/noncrypto.rs` | xoshiro256++ + `SplitMix64` seeder | Decorrelation and reproducible fixtures. Statistically excellent, **predictable**. |
| `FastRng` | `lib/rng/src/fast.rs` | Buffered ChaCha12, fast key erasure | Everything that must not be guessable but is not long-lived key material. |
| `CsRng` | `lib/rng/src/csprng.rs` | HMAC-SHA256 DRBG (unchanged) | Long-lived key material: ARXFS volume keys, the swap key, KASLR/ASLR seeds. |

Costs, amortised, scalar on every Tier-1 target (`chacha20_force_soft` is
pinned on `x86_64-unknown-none`, and the other bare-metal targets are scalar
by default; recovering a SIMD backend is explicitly out of scope):
~4 cycles/`u64` for `NonCryptoRng`, ~40 for `FastRng`, ~1500–2000 for `CsRng`.

## `FastRng` — the invariants

Bernstein's fast-key-erasure design over `lib/crypto`'s audited ChaCha12
(`stream::chacha12_keystream`), as in OpenBSD `arc4random` and Linux
`get_random_u64`.

* One refill runs the cipher once for `FAST_REFILL_BYTES` (256 = exactly four
  cipher blocks, so none is generated and discarded): the first 32 keystream
  bytes **become the key**, the remaining `FAST_BUFFER_BYTES` (224) fill the
  issue buffer. The key that produced a buffer is destroyed before a byte of
  it is issued; each byte is wiped as it is consumed.
* The `lib/crypto` wrapper writes the run into *two* destinations so there is
  no scratch buffer spanning it — and therefore no scratch copy of unissued
  random output to wipe. `N` is a const parameter, so the run is checked
  against the cipher's per-nonce capacity at compile time and the wrapper has
  no fallible path.
* A constant zero nonce is deliberate: the key is fresh every refill, so a
  `(key, nonce)` pair cannot recur.
* `seed_from_u64` stays `const` — `kernel/sched/api`'s task-id generator is a
  `static SpinLock<FastRng>` — by storing the key and marking the buffer
  empty; no cipher work happens until the first draw.
* Backtracking-resistant and deterministic from its key. **Not**
  prediction-resistant on its own: that needs fresh entropy, so it is the
  owner's job through `perturb_due` (cadence in bytes issued, so it does not
  shift with the buffer size) and `perturb` (XOR-fold, so a dead, stuck, or
  hostile source can never *lower* the key's quality).
* `FAST_BUFFER_BYTES` is a containment bound, not a capacity: it bounds
  unissued output resident in memory.

## `OutputReserve` — the whole chain in one type

```
entropy pool → CsRng (HMAC-DRBG) → FastRng<2048> (ChaCha12) → userland
```

Linux's shape with an extra NIST-approved stage in front. `CsRng` is the
authority (it keys the fast generator at seed time and re-keys it at every
boundary); `FastRng` is what every served byte comes from. The reserve's own
byte buffer is **gone** — `FastRng` already is a buffered generator with
zero-on-consume, and a second such buffer beside it would be one more
zeroisation path to keep correct. The reserve stays the charter-sanctioned
2 KiB.

Consequences worth keeping in mind:

* Serving needs no fresh entropy at all, so a seeded reserve never fails and
  never blocks; the large-request bypass is gone with the second buffer,
  because one path serves any length.
* **The perturbation reseeds the DRBG first.** Perturbing with output of a
  DRBG state compromised at the same moment buys nothing, and without a
  reseed on an *output* cadence the cipher stage would have dropped the DRBG's
  effective reseed rate from once per ~128 MiB of userland randomness to once
  per ~64 TiB, because the reserve draws from the DRBG so rarely.
* A momentary entropy shortage **defers** the perturbation to the next request
  rather than denying the caller's bytes, which are cipher output under a
  DRBG-derived key either way. `RandomFlags::NON_BLOCKING` chooses only
  between deferring and waiting.
* `discard()` rotates the key as well as dropping the buffer, unconditionally
  — that is what stops a suspend image or a cloned task continuing its
  original's stream.

## Task ids

`kernel/sched/api`'s process-wide generator is `FastRng`. Reaching a task is
authorised by capability and never by naming its id, so guessing one grants
nothing — but the endpoint registry's `lookup`/`contains` is an existence
oracle, and with an invertible generator a process that observes a handful of
ids recovers the state and can enumerate every live and future task and
endpoint id on the machine, across every tenant. Admitting a task costs
thousands of cycles, so a cipher-backed draw is free by comparison.

`seed_task_ids` takes a full `StreamKey` rather than a `u64`, so the boot
path's CSPRNG bytes are not stretched from 64 bits of effective entropy.

## Work-stealing scan starts

`StealScan` in `kernel/sched/api/src/steal.rs` owns the per-CPU
`NonCryptoRng` table and the unbiased `start(cpu, cpus)` draw. The three
policies had byte-identical copies of the field, the construction loop, and a
hand-rolled `s % n`; the charter's carve-out covers parallel *policy*
implementations, and a scan rotation is not policy. The three policy crates no
longer depend on `lib/rng` at all.

Streams are seeded with the bare CPU index — `SplitMix64` avalanches, so
adjacent seeds give unrelated streams, and the four copies of
`0x9E37_79B9_7F4A_7C15 ^ cpu` (itself `SplitMix64`'s own increment reused as a
seed) are gone. There is deliberately **no** seed field on `SchedulerConfig`:
unpredictability is not load-bearing for a scan rotation, so a seed with no
real supplier would be speculative surface. `StealScan`'s rustdoc records the
condition under which that stops holding.

## Also fixed

* **`hardware::PlatformFast` deleted.** It handed raw hardware-RNG output to
  callers as final output, which the charter forbids in terms, and was
  *slower* than what it replaced (~200–500 cycles per 64 bits). It had no
  consumer outside its own tests. `HardwareEntropy` remains as the correct
  entropy-input role.
* **`CsRng::fork_fast`** returns a `FastRng<N>` keyed from DRBG output, and
  now has a real consumer: `OutputReserve::seed`.
* **`NonCryptoRng` keeps only `seed_from_u64`.** The raw-state and
  entropy-seeded constructors are gone — seeding a deliberately predictable
  generator from entropy is not a thing to make easy, and neither had a
  consumer left.
* **The stale doc line** advertising `FastRng` for "hashed-collection seeds"
  is gone; `lib/hash` keys itself from the CSPRNG only.
* **A doc/impl divergence in the random ABI**, noticed in passing: the ABI
  prose said a pre-seed request *blocks* until the RNG is seeded, while the
  handler returns `EntropyNotReady` for blocking and non-blocking callers
  alike — and deliberately so, since the only way the RNG is still unseeded
  once userland exists is that every platform entropy source is dead, and a
  wait on a dead source never ends. The prose now says what the system does.

## Tests

Structural tests in `lib/rng` are the load-bearing ones, because no
statistical test can distinguish a good PRNG from true randomness: the
key-erasure split asserted against the raw cipher's keystream (pinning the
buffer split, key derivation, nonce and byte order at once), backtracking
resistance, zeroise-on-consume, XOR-folding that cannot degrade a key, a
discard that rotates the key even with nothing buffered, `const` construction,
and a `Debug` that prints only sizes. `lib/crypto`'s wrapper is pinned against
a ChaCha12 keystream computed independently from the RFC 8439 round function
reduced to twelve rounds, so both the round count and the split point are
tested rather than restated from the dependency.

The statistical battery is `tests/integration/rng_soak` (see its README for
the test list, the negative-control design, and the two-level decision rule).
It runs as a fixed-seed pass in the host test phase of `cargo xtask ci` — so
the gate is deterministic and can never be flaky — and as `cargo xtask
rngsoak` / `tools/ci/soak.sh rngsoak` for depth.

## Deliberately out of scope

* **No SIMD chase.** The `chacha20_force_soft` pin on `x86_64-unknown-none`
  exists because SIMD lowering crashes codegen there. Recovering AVX2 or the
  aarch64 NEON backend is a separate build-glue question with its own risk;
  this work takes the scalar cost and states it.
* **No AES-CTR alternative.** Hardware AES would be ~0.3 cycles/byte, but it
  needs the `aes` + `ctr` crates (new audit surface), has no hardware
  guarantee on riscv64 or wasm32, and its software fallback is both slower
  than ChaCha and cache-timing-vulnerable.
* **No FIPS conformance claim.** `random_get`'s output is no longer *directly*
  SP 800-90A DRBG output. TAIRiX makes no FIPS claim, and `CsRng` stays
  directly reachable, so the option is preserved rather than exercised.
