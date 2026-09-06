# tairix-test-rng-soak

The statistical soak over the kernel random subsystem. Drives `lib/rng`'s two
**unpredictable** generators — `FastRng` (buffered ChaCha12, fast key erasure)
and `CsRng` (NIST SP 800-90A HMAC-SHA256 DRBG) — through a first-party NIST
SP 800-22-style battery.

Host test scaffolding: it lives under `tests/` and is never linked into
TAIRiX. Its arithmetic is floating point, which `lib/rng`'s `no_std` body has
no business carrying, and a statistical test is an ordinary numerical
algorithm rather than a cryptographic primitive, so implementing it here is
legitimate.

## What this can and cannot claim

No statistical test can distinguish a good PRNG from true randomness, so
nothing here certifies a generator. What the battery does is **reject the
structure a broken one leaves behind**: a bias, a short-range correlation,
linear dependence over GF(2), compressibility. The load-bearing proofs of each
construction — the key-erasure split against the raw cipher's keystream,
backtracking resistance, zeroise-on-consume — are unit tests where the
generators live. This crate is the outer check that the bytes actually coming
out look like nothing at all.

## The battery

| Statistic | What it rejects |
|---|---|
| `frequency` | An overall bias between ones and zeros |
| `block-frequency` | A drift, or two opposite biases that cancel |
| `runs` | A sequence too sticky or too jumpy for its balance |
| `longest-run` | A wrong *extreme* of the run-length distribution |
| `matrix-rank` | Linear dependence over GF(2) — the LFSR-class signature |
| `approximate-entropy` | Repeated structure over ten-bit patterns |
| `cusum-forward` / `cusum-backward` | A bias too small to see at this length, accumulated by a walk |
| `maurer-universal` | Any exploitable redundancy, via block repeat distances |

The spectral (DFT) test is deliberately absent: it needs an FFT for detection
power the rank and entropy tests already have. Cumulative sums contributes two
p-values because SP 800-22 defines it that way; keeping them separate matters,
since each is uniform under the null hypothesis on its own where any
combination of the two would not be.

Sequences are 64 KiB (2^19 bits), the smallest power of two satisfying every
test's own validity condition at once — the binding one is Maurer's, which
needs upwards of 387 840 bits at `L = 6`.

## Every statistic carries a negative control

A statistical test that rejects nothing is not a weak test; it is not a test.
So `every_statistic_rejects_a_known_bad_generator` pins, per statistic, that
at least one known-bad generator is rejected — and insists the verdict be a
*statistical* rejection, not merely a run too short to conclude anything.

* **`counter`** — a bare incrementing counter, the classic mistake of reaching
  for one where randomness was wanted. Rejected by all nine.
* **`lfsr`** — a maximal-length 31-bit LFSR. Its output is statistically
  excellent (an m-sequence has ideal balance and an ideal run-length
  distribution) and every bit of it is a linear function of 31 state bits, so
  a 32-bit-wide matrix built from it can never reach full rank. The degree is
  *deliberately* below the matrix span: a wider register would hide the
  linearity and make the control vacuous. It **passes** the bias and
  correlation tests, which is what makes it a control for the rank test
  specifically rather than a generally bad generator.

`NonCryptoRng` is not a target. It is predictable by design and passes this
battery comfortably — xoshiro's `++` scrambler is nonlinear and its state is
wider than the matrix-rank span — which is exactly why a battery is the wrong
instrument for judging it, and precisely the point of the two-tier split.

## The verdict

SP 800-22's two-level rule, reached **once** over every sequence a run
accumulated:

1. **Pass proportion** — the fraction of sequences clearing `ALPHA = 0.01`
   must sit inside a band around `1 - ALPHA`. Two-sided on purpose: a
   statistic that *never* rejects has stopped discriminating.
2. **Uniformity** — the p-values must be uniform on `[0, 1)`, by chi-square
   over ten bins. This is the arm that catches a generator that is *too*
   regular.

The band is **six** sigma rather than the suggested three, and the uniformity
floor `1e-6`, so the whole battery's false-alarm probability is around `1e-4`.
A gate must never be flaky and a soak whose verdict was noise would be worse
than none. Detection power is unaffected: a structural defect does not sit
marginally outside the band, it pins p-values at zero and lands hundreds of
sigma out — as the controls demonstrate.

Deciding once at the end is both the more powerful and the less flaky
arrangement: the band narrows as the sequence count grows, so a long soak
becomes strictly more sensitive, while re-deciding after every pass would
multiply the false-alarm rate by the number of looks.

## Running it

```sh
cargo test -p tairix-test-rng-soak       # one fixed-seed pass per generator
cargo xtask rngsoak --list               # the registry (fast/csprng)
cargo xtask rngsoak --quick              # per-generator budget, ≥ 5 s each
cargo xtask rngsoak --soak               # nightly budget, ≥ 24 h each
cargo xtask rngsoak --target fast --secs 30
tools/ci/soak.sh rngsoak --secs 20       # one job per generator, in parallel
```

A plain `cargo test` — the per-PR gate — runs from a **fixed** seed, unlike
the other soaks. A statistical verdict is probabilistic, so a fresh-seed smoke
run would carry the battery's whole false-alarm probability into every gate
run; with a fixed seed the gate either passes forever or fails forever. A
budgeted soak draws a fresh seed and logs it, so a nightly failure replays
exactly via `TAIRIX_RNGSOAK_SEED`.

The budget and per-pass byte count come from `TAIRIX_RNGSOAK_BUDGET_SECS` and
`TAIRIX_RNGSOAK_BYTES`, named once in `tairix-fuzzseed` so the orchestrator
and the harness cannot drift on a spelling.
