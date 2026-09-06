# `tairix-rng`

The single place TAIRiX gets randomness. It separates three generators by the
**property** that decides which one a call site may use — not by how fast they
are — so the wrong one is hard to reach for by accident:

| Type           | Property                   | Use it for |
|----------------|----------------------------|------------|
| `CsRng`        | Cryptographically secure   | Long-lived key material: `ARXFS` volume keys, the swap key (§4), the KASLR/ASLR seed (§19.2), capability material |
| `FastRng`      | Fast **and unpredictable** | Anything that must not be guessable but is not long-lived key material: task ids, network payloads, the kernel output reserve |
| `NonCryptoRng` | Fast and **predictable**   | Decorrelation and reproducible fixtures: per-CPU work-stealing scan starts, seeded test streams |

The axis is unpredictability, not statistical quality, because the two are
unrelated. `NonCryptoRng` (xoshiro256++) passes the full BigCrush/PractRand
batteries and is still trivially invertible: four consecutive outputs carry
its whole 256-bit state, and recovering it is arithmetic rather than
cryptanalysis. A type named only for its speed would say nothing about which
property it has, which is how bulk randomness ends up drawn from an
invertible generator.

## The cryptographic core is composed, not hand-rolled

Both unpredictable generators are standard constructions over `lib/crypto`'s
audited primitives — HMAC-DRBG over `hmac_sha256`, fast key erasure over
`chacha12_keystream`. No cryptographic primitive is written in this crate; the
only first-party algorithm is xoshiro256++, which is an ordinary PRNG rather
than a security primitive (§2.12).

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

## Two draw styles: fallible and blocking

A draw may trigger a reseed, and a reseed needs fresh entropy that can be
momentarily unavailable. `CsRng` lets the caller choose how to cope, and
neither style spins or panics (§2.1, §2.9):

* **Fallible** (`try_fill_bytes`, `try_next_u64`/`try_next_u32`, `reseed`):
  a required reseed with no entropy *right now* returns the typed, transient
  `EntropyError::Reseeding` without disturbing the generator, so the caller
  fails closed (§5.4) or retries. A *hard* failure — no source at all, e.g.
  at instantiation — is the distinct `EntropyError::Unavailable`.
* **Blocking** (`fill_bytes_blocking`, `try_next_u64_blocking`/
  `try_next_u32_blocking`, `reseed_blocking`): a required reseed instead
  **waits** for entropy through the source's `EntropySource::fill_blocking`
  seam — the platform source parks the calling task, it never busy-spins —
  and then returns the bytes. It fails with `EntropyError::Unavailable` only
  when the source is genuinely dead. When no reseed is due (the common case)
  a blocking draw does exactly the same work as a fallible one and never
  blocks.

`fill_blocking` is a defaulted trait method (it delegates to `fill`), so an
always-ready source needs no extra code; only a source whose pool can be
momentarily exhausted overrides it to park. `CombinedSource` threads the
choice through its single XOR-combine loop (§2.2), waiting out a transient
source under `fill_blocking` while still skipping a hard-dead one.

`CsRng` reseeds automatically every `DEFAULT_RESEED_INTERVAL` draws — far
below the DRBG's hard `2^48` reseed limit — buying forward secrecy cheaply.

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

A hardware source is **entropy input and nothing else**: `HardwareEntropy`
adapts it to `EntropySource`, so it is one input among several feeding
`CsRng`, never the sole one, and its bytes never reach a caller
unconditioned (§22 — "hardware RNG output is input material only"). A caller
wanting speed takes `FastRng`, whose output is conditioned by a cipher and
costs *less* per byte than a hardware RNG instruction.

## Kernel output reserve and the random syscall (§22)

`AGENTS.md` §22 mandates exactly one kernel cryptographic random
subsystem, reached from userland only through a single versioned random
syscall. The contract lives in `lib/abi`
(`tairix_abi::random` — `RandomFlags`, `RANDOM_RESERVE_DEFAULT_BYTES` =
2 KiB, `RANDOM_REQUEST_MAX_BYTES`) and the syscall is
`SyscallNumber::RANDOM_GET` (`abi-v1`, appended to the table). Drawing
randomness needs no capability; an over-large request is refused with
`LengthOutOfRange`, and a request issued before the RNG is seeded fails
closed with `Errno::EntropyNotReady` rather than returning weak bytes or
waiting on entropy sources that are, by then, dead.

`OutputReserve<E, const N>` is the bounded reserve of CSPRNG **output**
(not raw entropy) the kernel keeps — preferably one per CPU in
kernel-only, non-swappable memory. It is the whole chain in one type:

```text
entropy pool → CsRng (HMAC-DRBG) → FastRng<2048> (ChaCha12) → userland
```

Exactly Linux's shape, with an extra NIST-approved stage in front. `CsRng` is
the *authority*: it keys the fast generator at seed time and re-keys it at
every boundary. `FastRng` is what every served byte comes from, so a userland
request costs a cipher block rather than a DRBG generate. The reserve keeps
**no byte buffer of its own** — `FastRng` already is a buffered generator that
wipes each byte as it is consumed, and a second such buffer beside it would be
one more zeroisation path to keep correct (§2.2).

* **Unseeded before the RNG initialises.** `fill` returns
  `ReserveError::NotReady` until `seed` succeeds, which the kernel maps to
  `EntropyNotReady`.
* **No weak fallback, and no blocking, once ready.** Serving needs no fresh
  entropy at all, so an exhausted buffer is regenerated from the cipher on the
  spot. A request of any length is served in one call, and a seeded reserve
  neither returns short nor waits on the entropy source (§22).
* **Zeroised on consumption and reuse.** Consumed bytes are wiped immediately,
  and a refill destroys the key that produced the previous buffer, so a
  paged-out or cloned copy can replay nothing.
* **Discarded across boundaries.** `discard` drops buffered output *and*
  rotates the key behind it for the
  suspend/hibernate/fork-clone/crash-dump/reseed boundaries §22 enumerates —
  rotating unconditionally is what keeps a suspend image or a cloned task from
  continuing its original's stream. Both generators' state is wiped on drop
  via `zeroize`.
* **Periodically prediction-resistant.** Every `PERTURB_INTERVAL_BYTES` of
  output the reserve reseeds the DRBG from the entropy pool and folds a fresh
  32 bytes into the cipher key. The reseed comes first and is the point:
  perturbing with output of a DRBG state compromised at the same moment buys
  nothing. It also keeps fresh entropy entering the chain on a bounded
  *output* cadence — without it, putting a cipher in front would have dropped
  the DRBG's reseed rate from once per ~128 MiB of userland randomness to once
  per ~64 TiB, because the reserve draws from the DRBG so rarely. A momentary
  shortage defers the fold to the next request rather than denying the
  caller's bytes, which are cipher output under a DRBG-derived key either way;
  `RandomFlags::NON_BLOCKING` chooses only between deferring and waiting.

The reserve does not hand bytes to userland by itself: the production
`kernel/core` `random_get` handler (increment D.4 of `PLAN.md` Stage 7's
staged copy path) composes an `OutputReserve` into `KernelState` behind
a `RandomReserve` trait object, enforces the request cap, draws CSPRNG
output in fixed kernel-staging chunks, and copies each chunk into the
caller's buffer through the `copy_to_user` boundary, wiping the staging
afterwards (§22). The reserve boots **unseeded** over a `NullEntropy`
source, so a draw fails closed with `Errno::EntropyNotReady` until the
platform-RNG `EntropySource` re-seeds it at boot (next section); a
faulting buffer maps to `BadAddress`. Every consumer of the
staged copy path (`ipc_send`, `ipc_recv`, `cap_delegate`, `random_get`)
is wired.

## Platform entropy: the Arch HAL seam and boot seeding

The reserve is only useful once it is **seeded**, and the raw entropy that
seeds it is an architecture resource. The Arch HAL exposes it as a closed
slice, `tairix_arch_api::entropy` (modelled on the memory-tagging and
side-channel slices, §17.2): a port implements `PlatformEntropy` — a
`HardwareRng` plus an honest `EntropyProfile` declaring its hardware-RNG
source `Supported`, `Unsupported`, or `Pending` (with a justification, like
the memory-tagging profile). Each port runs the slice's `conformance`
vertical, which checks the profile is honest and that a port claiming **no**
source fails a draw closed (never returns predictable bytes).

The per-target sources, all runtime-detected and fail-closed:

| Target  | Source | Profile |
|---------|--------|---------|
| x86_64  | `RDSEED` (preferred) / `RDRAND`, via `CPUID` detection | `Supported` |
| aarch64 | ARMv8.5 `FEAT_RNG` `RNDR`, via `ID_AA64ISAR0_EL1` detection | `Supported` |
| riscv64 | `Zkr` `seed` CSR | `Pending` (needs the M-mode `mseccfg.sseed` delegation) |
| wasm32  | host `crypto.getRandomValues` | `Pending` (needs the host entropy import) |

The bare-metal instruction sequences are `cfg`-gated to `target_os = "none"`
(the host build fails the draw closed, like the memory-tagging `stg`), so the
real path is exercised by the QEMU verticals. Each draw retries a
momentarily-underfull generator a **bounded** number of times, then fails
closed — never an unbounded spin (§2.1).

At boot, `kernel/core` reaches the source through `KernelArch::platform_entropy`
and seeds the reserve once — but **never from the hardware RNG alone**, per
§22's "no single source is trusted alone". It wraps the hardware handle as an
`EntropySource` (`ArchEntropy`) and XOR-mixes it with *three* independent
sources — a CPU-timing-jitter source (next section), the asynchronous
interrupt-arrival-timing pool (the section after), and the firmware-provided
**boot seed** (below) — through nested `MixedPair`s (`KernelEntropy<A> =
MixedPair<MixedPair<MixedPair<ArchEntropy, JitterSource<ArchTicks<A>>>,
InterruptPoolSource<'static>>, BootSeedSource>`), builds a `SeededReserve<A>`
over that mix, and swaps it in for the `NullEntropy` boot reserve. Because the
mix owns the reseeding sources, every automatic reseed re-draws from them (the
one-shot boot seed excepted — it is consumed once and then contributes the XOR
identity). The decision is audited (`EntropyReserveSeeded` records the
seed-time contributors — `hardware+jitter`, `hardware+jitter+bootseed`,
`hardware+bootseed`, or `hardware` — while `EntropyReserveUnseeded` records a
cause). XOR is entropy-preserving for independent inputs, so a backdoored,
stuck, or observable hardware RNG cannot lower the seed's quality below the
other sources' contribution, and vice versa; only if *every* source is
unavailable does the reserve stay unseeded and `random_get` keep failing
closed — the kernel never weakens to predictable output.

### Boot seed: the firmware `rng-seed`, source of last resort

Boot firmware and loaders hand the kernel a block of random seed material at
hand-off — the device tree's `/chosen/rng-seed` on an FDT platform (U-Boot, the
Raspberry Pi firmware, **and QEMU's `virt` board, which always publishes one**).
The architecture boot path reads it (`Fdt::chosen_rng_seed`) and captures it
into a write-once latch (`kernel/core`'s `capture_boot_entropy_seed`), and the
seeding step moves it into a one-shot `BootSeedSource` (`lib/rng`) that expands
it with SHA-256 in counter mode, then zeroises the retained copy.

It is the source of **last resort**: on an emulated or virtualised machine the
guest CPU exposes no on-die hardware RNG *and* its cycle counter advances
deterministically, so both `ArchEntropy` and `JitterSource` fail their health
tests and the interrupt pool is still empty at seed time — without the boot
seed the reserve would never seed at all, leaving `random_get`, the per-boot
machine id, encrypted swap, and the `ramzip` compressed-memory tier's sealing
key all unavailable. Consumed once and wiped (matching the well-established
kernel practice of folding the boot `rng-seed` in once and erasing it), it is
XOR-mixed like every other source, so it can never lower the quality a real
hardware RNG contributes on a machine that has one; a machine that provided no
seed simply gains a source that contributes nothing, and the fail-closed
guarantee is untouched.

## Timing-jitter entropy: the independent second source

`JitterSource` is the software entropy source mixed with the hardware RNG so
neither is trusted alone. Its unpredictability is the **variation in execution
time** of a fixed workload, measured with the platform's high-resolution
monotonic counter (`TimeSource`, backed in the kernel by `ArchTicks` over the
Arch HAL `ticks_now` — x86 `RDTSC`, aarch64 `CNTPCT_EL0`, riscv64 `time`).
This is the well-studied CPU-jitter mechanism (Müller's `jitterentropy`).

The accounting is deliberately conservative and honest — jitter is
defense-in-depth, not the primary source:

* **Only non-stuck samples are credited.** Each raw timing delta runs through
  a "stuck" test (its first/second/third discrete derivatives); a sample a
  deterministic counter could have produced is folded into the conditioner but
  not counted toward the entropy budget.
* **Heavy oversampling.** Many credited samples are folded per output *bit*, so
  the SHA-256-conditioned output is at full entropy even if each sample carries
  well under one bit of min-entropy. Conditioning uses `lib/crypto`'s SHA-256
  (never a hand-rolled mixer); the running chain state is kept separate from
  the emitted block and zeroised on the way out.
* **Health tests fail closed.** A NIST SP 800-90B §4.4.1 repetition-count test
  and a bounded attempt budget mean a clock with no usable jitter — an emulator
  with a lockstep counter, a deterministic host test — returns
  `EntropyError::Unavailable` rather than manufacturing entropy or looping
  forever. In the mix that simply falls back to the hardware source.

`lib/rng` stays architecture-neutral: `JitterSource` is generic over
`TimeSource`, and the target-specific counter read lives behind the Arch HAL in
`kernel/core`'s `ArchTicks`.

## Interrupt-arrival-timing entropy: the asynchronous third source

The hardware RNG and the jitter source are both *synchronous* — the kernel
draws from them when it wants a seed. `InterruptEntropyPool` captures a
different, *asynchronous* physical process: the exact time at which external
device interrupts arrive. It is a classic entropy mechanism, kept honest and
mixed as a third never-sole source.

* **Wait-free recording on the interrupt hot path.** The kernel installs an
  `IrqDispatchObserver` (`IrqEntropyObserver`) on the `IrqTable`. `IrqTable::fire`
  notifies it at every interrupt arrival (bound *and* stray), and the observer
  reads the Arch HAL high-resolution counter (`ticks_now`) and calls
  `InterruptEntropyPool::record` — a single `Relaxed` atomic store into a
  fixed ring. No lock, no allocation, no conditioning on the hot path; the pool
  is a `static` both the interrupt observer and the reseeding reserve reference
  without a lock. Only the arrival *timing* is sampled (not the line), so the
  health test genuinely measures the timing source.
* **Freshness gate.** `InterruptPoolSource` (the `EntropySource` half, owned by
  the reserve) only contributes once a whole ring of samples *it has not already
  drained* has arrived, so a drain never re-conditions stale samples and never
  contributes from a barely-touched ring. Before then it fails closed with
  `EntropyError::Unavailable` — so at boot, before interrupts have flowed, the
  mix simply falls back to the hardware RNG and jitter, and the pool folds fresh
  timing into every later reseed for forward secrecy.
* **Health test fails closed.** A NIST SP 800-90B §4.4.1 repetition-count test
  over the snapshot rejects a stuck/emulated counter that offers no timing
  variance, returning `Unavailable` rather than crediting predictable samples.
  The snapshot is SHA-256-conditioned via `lib/crypto` (never a hand-rolled
  mixer); the running chain state is kept separate from the emitted block and
  zeroised on the way out.

`lib/rng` stays architecture-neutral: the pool and its source name no
architecture, and the counter read + the one-place feed live behind the Arch
HAL and the `IrqTable` in `kernel/core`.

## `FastRng`: buffered ChaCha12 with fast key erasure

`FastRng` is Bernstein's fast-key-erasure construction over `lib/crypto`'s
audited ChaCha12 — the same shape as OpenBSD's `arc4random` and Linux's
`get_random_u64`. One refill runs the cipher once under the current key and a
fixed nonce, producing `FAST_REFILL_BYTES` (256, exactly four cipher blocks,
so none is generated and thrown away) of keystream:

1. the first 32 bytes **become the key**;
2. the remaining 224 fill the issue buffer.

**The key that produced a buffer is destroyed before a byte of that buffer is
issued**, and each byte is wiped from the buffer as it is consumed. A constant
zero nonce is correct and deliberate: the key is fresh on every refill, so a
`(key, nonce)` pair can never recur, and a counter would be extra state buying
nothing.

What that gives, and what it does not:

* **Indistinguishable from uniform** at 256-bit security.
* **Backtracking-resistant.** The key behind already-issued bytes exists
  nowhere, so a full memory capture reveals nothing about past output.
* **Deterministic from its key**, so reproducible fixtures keep working, and
  `seed_from_u64` is `const` so a consumer can hold one in a `static` (the
  scheduler's process-wide task-id generator does).
* **Not prediction-resistant on its own.** Recovery from a state compromise
  needs fresh entropy, which no generator can conjure, so it is the owner's
  job: `perturb_due` reports the cadence (`PERTURB_INTERVAL_BYTES` of output,
  counted in bytes so it does not shift with the buffer size) and `perturb`
  XOR-folds 32 fresh bytes into the key. XOR is the point — a dead, stuck, or
  hostile source contributes zeros or garbage and can never *lower* the key's
  quality.

Cost, stated honestly. `.cargo/config.toml` pins `chacha20_force_soft` on
`x86_64-unknown-none`, because that target is soft-float and SSE-disabled and
lowering the AVX2/SSE2 intrinsics there crashes codegen; the other bare-metal
targets are scalar by default. **So in-kernel and in userland this is the
scalar backend on every Tier-1 target.**

| | cycles per `u64`, amortised |
|---|---|
| `NonCryptoRng` (xoshiro256++) | ~4 |
| `FastRng` (ChaCha12, scalar) | ~40 |
| `CsRng` (HMAC-DRBG) | ~1500–2000 |

Ten times the cost of xoshiro, forty times cheaper than the DRBG. That is why
the scheduler's steal-scan rotation keeps xoshiro — a hot path whose only
output is where a round-robin scan begins — and everything that should be
unpredictable takes `FastRng`.

## `NonCryptoRng`: predictable on purpose

`NonCryptoRng` is xoshiro256++ (Blackman & Vigna), seeded via SplitMix64. It
is an ordinary PRNG, not a security primitive (§2.12), so rolling it ourselves
is allowed and adds no dependency. It is named for its predictability rather
than its speed so that it reads as obviously wrong beside a key or a nonce
with no prior knowledge of xoshiro required.

Its one production consumer is `kernel/sched/api`'s `StealScan`, which gives
each CPU an independent stream deciding where that CPU begins its
work-stealing scan. That is decorrelation — stopping every idle CPU from
probing CPU 0 first and convoying on one queue lock — not unpredictability:
the scan visits every CPU anyway, and an observer who can predict which is
probed first learns nothing they could act on. Streams are seeded with the
bare CPU index, because SplitMix64 avalanches; there is deliberately no seed
field on `SchedulerConfig`, since a seed with no real supplier would be
speculative surface (§2.4).

`RandU64` carries the shared, generator-independent sampling logic — byte
filling and Lemire's unbiased bounded integers (`next_below`) — once, so no
consumer re-derives it (§2.2).

## The statistical soak

No statistical test can distinguish a good PRNG from true randomness, so the
load-bearing tests of these constructions are the *structural* ones listed
below. The outer check that the bytes actually coming out look like nothing at
all is `tests/integration/rng_soak`: a first-party NIST SP 800-22-style
battery — monobit, block frequency, runs, longest run of ones, binary matrix
rank, approximate entropy, cumulative sums (forward and backward), and
Maurer's universal statistical test — over `FastRng` and `CsRng`.

* **Every test carries a negative control it must reject**, because a
  statistical test that rejects nothing is not a weak test, it is not a test.
  A bare counter covers the bias, correlation, and compressibility tests; a
  31-bit LFSR covers the binary matrix rank test, whose whole purpose is
  detecting linear dependence over GF(2). The LFSR's register is deliberately
  narrower than the test's 32-bit matrix span — a wider one would hide the
  linearity and make the control vacuous — and it *passes* the bias and
  correlation tests, which is what makes it a control for the rank test
  specifically rather than a generally bad generator.
* `NonCryptoRng` is excluded by design: it is predictable on purpose and
  passes this battery comfortably, which is precisely why a battery is the
  wrong instrument for judging it.
* The verdict is SP 800-22's two-level rule — pass proportion inside a band,
  plus p-value uniformity by chi-square over ten bins — reached **once**, over
  every sequence a run accumulated. The band is six sigma rather than the
  suggested three, and the uniformity floor `1e-6`, so the whole battery's
  false-alarm probability is around `1e-4`: a gate must never be flaky, and a
  soak whose verdict was noise would be worse than none. Detection power is
  unaffected, because a structural defect does not sit marginally outside the
  band — it pins p-values at zero.
* A plain `cargo test` runs one fixed-seed pass per generator (so the PR gate
  is deterministic); `cargo xtask rngsoak` exports a wall-clock budget and the
  harness keeps drawing, accumulating into one verdict. The band narrows as
  the sequence count grows, so a longer soak is strictly more sensitive rather
  than merely longer. `tools/ci/soak.sh rngsoak` fans the registry out one job
  per generator.

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
  all-sources-failed fail-closed result, plus the blocking combine waiting
  out a transient source while still skipping a hard-dead one; and the owning
  `MixedPair` giving the same XOR, surviving a dead secondary, failing closed
  only when both sources die, and blocking through a transient one.
* Timing jitter: a varying clock yields non-zero, request-to-request-distinct
  output that spans multiple SHA-256 blocks; a lockstep (deterministic) clock
  fails closed rather than manufacturing entropy; the repetition-count health
  test trips on a constant delta and resets on change. In `kernel/core`, the
  mixed reserve seeds from hardware alone when jitter is unavailable, from
  jitter alone when the hardware source is dead, and fails closed when both
  are dead.
* Interrupt-arrival pool: `record` advances the event count and wraps the
  ring without panic; the source fails closed before a full fresh ring has
  arrived, contributes once it has, refuses to re-drain without new samples,
  produces multi-block output, and fails closed on a stuck (constant-sample)
  counter. In `kernel/core`, the full three-way `KernelEntropy` mix seeds from
  the interrupt pool alone when hardware and jitter are both dead, and fails
  closed when all three are unavailable. The `IrqTable` notifies its set-once
  dispatch observer on every fire (bound and stray) and rejects a second
  observer install.
* Draw styles: the default `fill_blocking` matching `fill`; a fallible draw
  surfacing transient `Reseeding`; a blocking draw and `reseed_blocking`
  waiting through a reseed shortage; and the no-reseed fast path producing
  identical output for both styles without blocking.
* Hardware paths: hardware-backed entropy seeding and failure propagation.
* `chacha12_keystream`: the first 96 keystream bytes under RFC 8439's
  test-vector key and nonce, computed independently from the round function
  reduced to twelve rounds, so both the round count and the 32/N split point
  are pinned rather than restated from the dependency.
* `FastRng` structure — the load-bearing set, because no statistical test can
  tell a good PRNG from true randomness: the first issued bytes equal the
  cipher's keystream past the derived key (pinning the split, the key
  derivation, the nonce, and the byte order at once); a state driven past a
  buffer cannot reproduce it; consumed bytes are wiped and live ones are not;
  perturbation diverges two identical generators, discards their buffers, and
  with an all-zero fold cannot change the key or stall the stream; a discard
  rotates the key even with nothing buffered, so a clone cannot continue its
  parent's stream; construction is `const`; `Debug` prints only sizes.
* Statistical balance: deterministic mean and per-bit-position checks
  over 1 MiB of `NonCryptoRng` and `CsRng` output (fixed seed — never flaky).
* Output reserve (§22): unseeded fail-closed, seed-failure handling,
  the served bytes matching a `FastRng` the DRBG keyed, multi-request refill,
  a request larger than the buffer served across refills without repeating,
  the perturbation actually firing on its output cadence (a starved reserve's
  stream diverges from a fed one's), a perturbation shortage never denying a
  draw, `discard` (suspend/clone) rotating the key so a clone cannot replay
  the parent, and `reseed` surfacing a transient `Reseeding` while
  `reseed_blocking` waits through it.
