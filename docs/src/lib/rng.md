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
platform-RNG `EntropySource` (§17.2 — still pending) re-seeds it; a
faulting buffer maps to `BadAddress`. With D.4 in, every consumer of the
staged copy path (`ipc_send`, `ipc_recv`, `cap_delegate`, `random_get`)
is wired.

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
  out a transient source while still skipping a hard-dead one.
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
