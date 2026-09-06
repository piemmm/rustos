# FIX-RANDOMNESS — the two-tier random split, done honestly

Status: **planned**

## Context

`lib/rng` today offers one fast generator, `FastRng` (xoshiro256++), and one
cryptographic one, `CsRng` (HMAC-SHA256 DRBG, NIST SP 800-90Ar1 §10.1.2). The
split is drawn on the wrong axis and the naming hides it:

* xoshiro256++ passes every statistical battery (BigCrush, PractRand) and is
  **trivially invertible** — four consecutive outputs carry 256 bits, the whole
  state, and recovering it is arithmetic, not cryptanalysis. Statistical
  quality and unpredictability are unrelated properties; the type name claims
  only speed and says nothing about which one it has.
* `CsRng` is the only unpredictable source and costs ~1500–2000 cycles per
  `u64`, so every consumer that wanted bulk randomness reached for the
  invertible one — including the process-wide task-id generator.
* `hardware::PlatformFast` hands raw `RDRAND` output to callers as final
  output, which the charter forbids in terms (§22: "Hardware RNG output is
  input material only"), and is *slower* than what it replaces.

The outcome this plan delivers: two generators whose names state what they
are, a fast source that is genuinely unpredictable, a scheduler that keeps its
~4-cycle draw, and a test suite that can tell the difference.

## The two tiers

| Type | Module | Algorithm | Use for |
|---|---|---|---|
| `NonCryptoRng` | `lib/rng/src/noncrypto.rs` | xoshiro256++ + `SplitMix64` seeder | Decorrelation and reproducible fixtures. Statistically excellent, **predictable**. |
| `FastRng` | `lib/rng/src/fast.rs` | Buffered ChaCha12, fast key erasure | Everything that should be unpredictable but is not long-lived key material. |
| `CsRng` | `lib/rng/src/csprng.rs` | HMAC-SHA256 DRBG (unchanged) | Long-lived key material: ARXFS volume keys, the swap key, KASLR/ASLR seeds. |

`NonCryptoRng` is named for the property, not the purpose: it reads as the
correct tool in `next_victim` and as obviously wrong beside a key or a nonce,
with no prior knowledge of xoshiro required. The current `FastRng` body moves
into `noncrypto.rs` unchanged — the algorithm is right for its remaining
consumers and swapping it would be premature pessimisation.

## `FastRng` — buffered ChaCha12 with fast key erasure

Bernstein's fast-key-erasure design, as used by OpenBSD `arc4random` and Linux
≥5.17 `get_random_u64`. Backed by the **audited** `chacha20` crate, nothing
hand-rolled.

```
pub struct FastRng<const N: usize = FAST_BUFFER_BYTES> {
    key: [u8; 32],          // Zeroize on drop
    buffer: [u8; N],        // Issued output; each byte zeroed as consumed
    cursor: usize,
    bytes_since_perturb: u64,
}
```

Refill, on exhaustion:

1. `ChaCha12::new(key, nonce = 0)` over a zeroed `N + 32` scratch → keystream.
2. `key = keystream[..32]` — **the key that produced this buffer is destroyed
   before a byte of it is issued.**
3. `buffer = keystream[32..]`, `cursor = 0`.
4. Zeroise the scratch.

A constant zero nonce is correct and deliberate: the key is fresh every
refill, so a `(key, nonce)` pair can never repeat. A counter would be extra
state buying nothing.

**Properties.** Output is indistinguishable from uniform at 256-bit security.
Backtracking-resistant: the key behind already-issued bytes exists nowhere, so
a full memory capture reveals nothing about past output. Deterministic from a
seed, so `lib/raster`'s fixtures and every reproducible test keep working.

**Periodic perturbation** gives prediction resistance (recovery after state
compromise), which genuinely needs fresh entropy and therefore a fallible,
caller-driven path — mirroring how `CsRng::reseed` is already separate from
generation:

* `perturb(&mut self, fresh: &[u8; 32])` XORs into the key and discards the
  buffer. XOR is the whole point: a dead, stuck, or hostile source can never
  *lower* the key's quality.
* `perturb_due(&self) -> bool` reports the cadence from
  `bytes_since_perturb >= PERTURB_INTERVAL_BYTES`. Expressed in **bytes
  generated, not refills**, so the cadence is independent of `N`.
* A `FastRng` whose owner never perturbs stays forward-secure but is not
  prediction-resistant. Say so in the rustdoc; do not pretend otherwise.

**Hard constraint.** `kernel/sched/api/src/task.rs:95` holds a
`static SpinLock<FastRng>`, so `FastRng::seed_from_u64` **must stay `const`**.
Achievable: the constructor stores the key and marks the buffer empty; no
cipher work happens until the first draw.

`FAST_BUFFER_BYTES = 256` (224 bytes issued per 4-block refill). This is a
containment bound, not a capacity (§24.4): it bounds unissued output resident
in memory. A 1 KiB buffer amortises only ~15% better for 4× the resident
exposure — not worth it.

### Cost, stated honestly

`.cargo/config.toml:31-43` pins `chacha20_force_soft` on
`x86_64-unknown-none`, because that target is soft-float and SSE-disabled and
lowering the AVX2/SSE2 intrinsics there crashes codegen. The other bare-metal
targets are scalar by default. **So in-kernel and in userland this is the
scalar backend on every Tier-1 target** — no SIMD, and no plan here to chase
it.

| | cycles/u64, amortised |
|---|---|
| `NonCryptoRng` (xoshiro256++) | ~4 |
| `FastRng` (ChaCha12, scalar) | ~40 |
| `CsRng` (HMAC-DRBG) | ~1500–2000 |

~10× the cost of xoshiro, ~40× cheaper than the DRBG. That is why the
scheduler keeps xoshiro and everything else moves to `FastRng`.

## Consumer migration

| Consumer | Moves to | Why |
|---|---|---|
| `kernel/sched/{cfq,eevdf,mlfq}` steal scan | `NonCryptoRng` | Sole output is the *start offset* of a round-robin scan that visits every CPU anyway. Its job is decorrelation — stop every idle CPU probing CPU 0 first and convoying on one queue lock — not unpredictability. Hot path; keep the 4-cycle draw. |
| `kernel/sched/api` task ids | `FastRng` | See below. |
| `userland/apps/ping` payload | `FastRng` | Payload only needs to be incompressible, but a draw happens once per echo — cost is irrelevant, so take the stronger generator. |
| `lib/raster` test fixtures | `NonCryptoRng` | Want determinism, not unpredictability. |
| `kernel/mem` `OutputReserve` | `FastRng` | See below. |

### Task ids move to `FastRng`

`kernel/sched/api/src/task.rs:95` draws task ids from xoshiro. The existing
rustdoc argues non-crypto is deliberate because "reaching a task is authorised
by capability and never by naming its id" — and that claim **is correct**:
`kernel/ipc/src/port.rs:392` gates every send on
`required_send_caps.is_subset_of(sender.effective())`, so guessing an endpoint
id grants nothing.

What remains is not an authority bypass but a real information leak:
`registry.rs:151` `lookup`/`contains` is an existence oracle, and because
xoshiro is invertible, a process that observes a handful of ids recovers the
generator state and can then enumerate **every live and future task and
endpoint id system-wide** — across tenants, on a machine that may host many
simultaneous users (§26.2). Task creation costs thousands of cycles, so a
~40-cycle draw is free. Take it.

Also widen `seed_task_ids` to take `[u8; 32]` rather than a `u64`
(`kernel/core/src/boot_id.rs:87` currently stretches 8 bytes into 256 bits of
state, capping effective entropy at 64 bits). The boot path already supplies
real CSPRNG bytes at `init.rs:2155`; give it the full key width.

### `OutputReserve` moves to `FastRng`

`lib/rng/src/reserve.rs` currently holds `rng: Option<CsRng<E>>` plus its own
2 KiB zero-on-consume buffer, so every `random_get` byte userland asks for is
paid at DRBG prices. New chain:

```
entropy pool → CsRng (HMAC-DRBG) → FastRng<2048> (ChaCha12) → userland
```

Exactly Linux's shape, with an extra NIST-approved stage in front.
`OutputReserve` becomes `CsRng<E>` (the reseed and perturb authority) plus
`FastRng<DEFAULT_RESERVE_BYTES>`, and **its own byte buffer is deleted** —
`FastRng` already is a buffered generator with zero-on-consume, and two such
implementations side by side is the duplication §2.2 forbids. One buffer, one
zeroisation path, and the reserve stays the §22-sanctioned 2 KiB.

The reserve's fill path checks `perturb_due()` and, when due, draws 32 bytes
from `CsRng` and perturbs — so the perturbation is structural, not a hope.
`CsRng` remains reachable directly for long-lived key material, so nothing
that needs the SP 800-90A generator loses it.

`discard()` keeps its §22 role (wipe on suspend/hibernate/clone/crash-dump/
reseed) and now also replaces the key, not just the buffer.

## Defects fixed in the same change

1. **Delete `hardware::PlatformFast`** (`lib/rng/src/hardware.rs:81-132`).
   Returns raw hardware-RNG output to callers as final output, which §22
   forbids outright; it is also a pessimisation (~200–500 cycles per 64 bits
   vs xoshiro's ~4) and has zero consumers outside its own tests.
   `HardwareEntropy` already covers the correct entropy-input role. Remove the
   re-export in `lib.rs:100` and the references in `README.md`,
   `docs/src/lib/rng.md`, and `rand.rs:5`.

2. **Delete the duplicated seed constant, don't hoist it.**
   `0x9E37_79B9_7F4A_7C15 ^ cpu` appears in `cfq/scheduler.rs:95`,
   `eevdf/scheduler.rs:114`, and `mlfq/scheduler.rs:258` — and is
   `SplitMix64`'s own increment (`fast.rs:36`) reused as a seed, so there are
   four copies that can silently diverge. The replacement needs no constant at
   all: derive each CPU's stream from the CPU index alone. `SplitMix64` already
   avalanches, so adjacent seeds give unrelated streams.

   Deliberately **no** seed field on `SchedulerConfig`: unpredictability is not
   load-bearing for a scan rotation, so a seed with no real supplier would be
   the speculative surface §2.4 forbids. Record in the shared type's rustdoc
   the condition under which that stops being true — if this generator ever
   feeds a security decision, it needs a real seed.

3. **Hoist the shared victim RNG into `kernel/sched/api`.** The
   `victim_rng: Box<[SpinLock<FastRng>]>` field, its construction loop, and
   `next_victim` are byte-identical across the three schedulers. §2.2's
   carve-out covers parallel *policy* implementations; steal-scan
   decorrelation is not policy — the three differ in vruntime, deadline, and
   band, not in this. One type in `kernel/sched/api`, constructed from
   `SchedulerConfig::cpus`, exposing the scan start.

4. **Use `next_below`, delete the hand-rolled modulo.** `rand.rs:47` already
   provides Lemire's unbiased bounded draw; all three schedulers hand-roll
   `s % n` plus a `#[allow(clippy::cast_possible_truncation)]` instead. The
   bias itself is ~10⁻¹⁷ and numerically irrelevant — the defect is three
   copies of a reduction the crate already ships, each carrying a lint
   suppression.

5. **Correct the stale doc line.** `fast.rs:10` advertises "hashed-collection
   seeds" as a `FastRng` use, but `lib/hash` correctly refuses to key itself
   from anything but the CSPRNG (`init.rs:2140`). That line invites a future
   misuse.

## Tests

### Structural — the load-bearing ones, every PR

No statistical test can distinguish a good PRNG from true random, so these are
what actually prove the construction. All in `lib/rng`, deterministic seeds,
never flaky.

* **Key-erasure split** — the generator's first issued bytes equal
  `ChaCha12(seed_key, nonce=0)` keystream bytes `32..`, asserted against the
  raw cipher. Pins the buffer split, key derivation, and byte order.
* **Backtracking resistance** — issue buffer *N*, force a refill to *N+1*,
  assert no state in the struct driven forward reproduces buffer *N*.
* **Zeroisation on consume** — after drawing `k` bytes, `buffer[..k]` is all
  zero.
* **Perturbation diverges** — two clones, perturb one, assert divergence.
* **Perturbation cannot degrade** — an all-zero source leaves the key
  unchanged and the stream intact; a *failing* source returns `Err` and leaves
  the generator usable.
* **Determinism** — same seed, same stream.
* **`const` construction** — a `static SpinLock<FastRng>` compiles.
* **`Debug` elides key and buffer.**

### Statistical — quick in `ci`, full in the nightly soak

A first-party NIST SP 800-22-style battery. Statistical tests are ordinary
numeric algorithms, not crypto primitives, so implementing them here is
legitimate; they live in **host** test code (floats are fine there, unlike
`lib/rng`'s `no_std` body).

Tests: frequency (monobit), block frequency, runs, longest run of ones,
**binary matrix rank**, approximate entropy, cumulative sums, Maurer's
universal statistical test. Skip the DFT/spectral test — it needs an FFT for
marginal added power.

The binary matrix rank test is the interesting one: it detects linear
dependence over GF(2), the structural signature of an LFSR-class generator.

**Every test carries a negative control** — a known-bad generator (a plain
LFSR, a bare counter) it must *reject*. Without one, a statistical test can be
vacuous and assert nothing. Note that xoshiro256++ is **not** a valid negative
control: its `++` scrambler is nonlinear and it passes these tests at
practical sizes, which is precisely the point of this whole plan.

Scope: `FastRng` and `CsRng`. `NonCryptoRng` is excluded by design — it is
predictable on purpose and only the fast structural tests apply to it.

**Where it runs.** New `cargo xtask rngsoak`, following the `fssoak` precedent
(`tools/xtask/src/commands/fssoak.rs`) exactly: a `Target` registry with
`--list`/`--target`/`--soak`/`--secs`, budget env vars declared once in
`tests/fuzzseed/src/lib.rs` (`RNGSOAK_BUDGET_ENV`, `RNGSOAK_SEED_ENV`,
`RNGSOAK_BYTES_ENV`) so the two sides cannot drift, and the harness at
`tests/integration/rng_soak/tests/rng_soak.rs`.

* `cargo xtask ci` runs a fixed **byte** budget — a few seconds, deterministic
  seed, so the PR gate does not grow and the run can never be flaky.
* `tools/ci/soak.sh` gains an `rngsoak` kind, enumerated from the registry
  (never hard-coded), included in `all`, at `throughput_nice` like the other
  deadline-free soaks.

## Files

* `lib/rng/src/{fast,noncrypto,hardware,reserve,rand,lib}.rs`, `Cargo.toml`,
  `README.md`
* `lib/crypto/Cargo.toml` — promote `chacha20` to a direct dependency with
  `default-features = false, features = ["zeroize"]`, plus a narrow
  stream-cipher wrapper in `lib/crypto/src/stream.rs` so `lib/rng` never names
  the upstream `cipher` traits (mirroring how `aead.rs` narrows
  `chacha20poly1305`). **Zero new audit surface**: `chacha20 0.9.1` is already
  in `Cargo.lock` as a transitive dependency and already pinned in
  `supply-chain.toml:53`, so the lockfile does not change.
* `kernel/sched/api/src/{task,lib}.rs` + the new shared victim-RNG module
* `kernel/sched/{cfq,eevdf,mlfq}/src/scheduler.rs` and their `lib.rs` rustdoc
* `kernel/core/src/boot_id.rs` (32-byte task-id seed)
* `userland/apps/ping/src/run.rs`
* `lib/raster/src/{blur,color}_tests.rs`
* `tools/xtask/src/commands/rngsoak.rs`, `commands.rs`, `ci.rs`
* `tests/fuzzseed/src/lib.rs`, `tests/integration/rng_soak/`
* `tools/ci/soak.sh`
* `docs/src/lib/{rng,crypto}.md`, `README.md` support matrix if a row moves

## Deliberately out of scope

* **No SIMD chase.** The `chacha20_force_soft` pin on `x86_64-unknown-none`
  exists because SIMD lowering crashes codegen there. Recovering AVX2 or the
  aarch64 NEON backend is a separate build-glue question with its own risk;
  this plan takes the scalar cost and states it.
* **No AES-CTR alternative.** Hardware AES would be ~0.3 cycles/byte, but it
  needs the `aes` + `ctr` crates (new audit surface), has no hardware
  guarantee on riscv64 or wasm32, and its software fallback is both slower
  than ChaCha and cache-timing-vulnerable. One stream cipher, already audited,
  already present.
* **No FIPS conformance claim.** Moving `random_get` behind ChaCha12 means its
  output is no longer *directly* SP 800-90A DRBG output. TAIRiX makes no FIPS
  claim today, and `CsRng` stays directly reachable, so the option is
  preserved rather than exercised.

## Verification

1. `cargo clean` first — stale artefacts have caused spurious failures here.
2. Structural tests: `cargo test -p tairix-rng` — expect the key-erasure,
   backtracking, and zeroisation tests to fail against any implementation that
   gets the buffer split wrong.
3. Negative controls: assert the statistical battery *rejects* the LFSR and
   counter generators. A battery that passes everything is proving nothing.
4. Scheduler: the `kernel/sched/api` conformance suite still passes for all
   three policies; per-CPU scan starts are decorrelated (no two CPUs share a
   stream) and unbiased across `n` that is not a power of two.
5. `cargo xtask rngsoak --list`, then `--secs 20` as a smoke run; confirm
   `tools/ci/soak.sh rngsoak --secs 20 --dry-run` enumerates from the registry.
6. Whole-project gate, once, on the final tree:
   `{ cargo xtask ci > /tmp/ci.log 2>&1; echo "CI-RC=$?" >> /tmp/ci.log; }`,
   then `cargo xtask fuzz --secs 5` and `tools/ci/soak.sh both --secs 20`.
