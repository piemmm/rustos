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

1. **Extra entropy.** `HardwareEntropy` adapts it to `EntropySource`, so
   it is one input among several feeding `CsRng`, never the sole one.
2. **A fast source.** `hardware::PlatformFast` draws fast `u64`s from the
   hardware directly when present, and falls back to the software
   `FastRng` when it is absent or momentarily fails — there is no
   busy-retry-until-it-works loop (§2.1).

## Kernel output reserve and the random syscall (§22)

`AGENTS.md` §22 mandates exactly one kernel cryptographic random
subsystem, reached from userland only through a single versioned random
syscall. The contract lives in `lib/abi`
(`rustos_abi::random` — `RandomFlags`, `RANDOM_RESERVE_DEFAULT_BYTES` =
2 KiB, `RANDOM_REQUEST_MAX_BYTES`) and the syscall is
`SyscallNumber::RANDOM_GET` (`abi-v1`, appended to the table). Drawing
randomness needs no capability; an over-large request is refused with
`LengthOutOfRange`, and a non-blocking request issued before the RNG is
seeded fails closed with `Errno::EntropyNotReady` rather than returning
weak bytes.

`OutputReserve<E, const N>` is the bounded reserve of CSPRNG **output**
(not raw entropy) the kernel keeps — preferably one per CPU in
kernel-only, non-swappable memory — so it serves requests without
running the DRBG on every call:

* **Unseeded before the RNG initialises.** `fill` returns
  `ReserveError::NotReady` until `seed` succeeds; the kernel maps that to
  a block (normal request) or to `EntropyNotReady` (non-blocking).
* **No weak fallback once ready.** If the buffer is exhausted, the
  reserve regenerates synchronously from `CsRng`; a request larger than
  the buffer is generated directly. It never returns short or blocks for
  fresh entropy after initialisation.
* **Zeroised on consumption and reuse.** Consumed bytes are wiped
  immediately and the whole buffer is wiped before each refill, so a
  paged-out or cloned copy cannot replay them.
* **Discarded across boundaries.** `discard` wipes buffered output for
  the suspend/hibernate/fork-clone/crash-dump/reseed boundaries §22
  enumerates; the generator state is wiped on drop via `zeroize`.

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
slice, `rustos_arch_api::entropy` (modelled on the memory-tagging and
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
`EntropySource` (`ArchEntropy`) and XOR-mixes it with *two* independent
software sources — a CPU-timing-jitter source (next section)
and the asynchronous interrupt-arrival-timing pool (the section after) —
through nested `MixedPair`s (`KernelEntropy<A> = MixedPair<MixedPair<ArchEntropy,
JitterSource<ArchTicks<A>>>, InterruptPoolSource<'static>>`),
builds a `SeededReserve<A>` over that mix, and swaps it in for the
`NullEntropy` boot reserve. Because the mix owns all three sources, every
automatic reseed re-draws from *all* of them, not just the hardware source. The
decision is audited (`EntropyReserveSeeded` records the seed-time contributors
`sources = hardware+jitter` or, when the platform offers no usable timing
jitter, `hardware` — the interrupt pool contributes nothing at boot and joins
at reseed; `EntropyReserveUnseeded` records a cause). XOR is entropy-preserving
for independent inputs, so a backdoored, stuck, or observable hardware RNG
cannot lower the seed's quality below the other sources' contribution, and vice
versa; only if *every* source is unavailable does the reserve stay unseeded
and `random_get` keep failing closed — the kernel never weakens to predictable
output.

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
* Hardware paths: hardware-backed entropy seeding, hardware-preferring
  fast draws, and the software fallback on absence or transient failure.
* Statistical balance: deterministic mean and per-bit-position checks
  over 1 MiB of `FastRng` and `CsRng` output (fixed seed — never flaky).
* Output reserve (§22): unseeded fail-closed, seed-failure handling,
  post-seed non-blocking generation, multi-request refill, large-request
  direct generation, zeroise-on-consume, `discard` (suspend/clone) and
  `reseed` boundary wipes, reseed-failure surfacing as transient
  `Reseeding`, and the blocking `fill_blocking`/`reseed_blocking` waiting
  through a reseed shortage.
