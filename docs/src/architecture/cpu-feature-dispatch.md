# CPU feature detection and self-optimising dispatch

TAIRiX images are compiled against a conservative **build-time floor** — the
common instruction set of every machine an image must boot (a single generic
image per architecture, not per board). Anything the booted CPU offers *above*
that floor — CRC32, the crypto extension, wide SIMD, a block-zero instruction
— is reached only by asking
the silicon at runtime which extensions it implements and dispatching to an
extension-using routine **only on cores that have it**. This is the runtime
*ceiling* that recovers, per booted CPU, everything the conservative floor
gives up.

The full staged design is `plans/FIX-HARDWARE-FEATURES.md`; this page is the
orientation.

## Two axes, never conflated

- **Capability** — "does this core have the instruction?" — is a deterministic
  fact read from CPU ID registers, never a benchmark. Reading it is
  architecture-specific and lives behind the closed Arch HAL `cpufeatures`
  slice (`kernel/arch/api::CpuFeatures`): CPUID on x86_64,
  `ID_AA64ISAR0_EL1`/`ID_AA64PFR0_EL1` on aarch64, `misa` + the device-tree
  `riscv,isa` string on riscv64, an honest empty set on the wasm32 host. The
  output is the arch-neutral `CpuFeatureSet` bitset (defined once in
  `tairix_abi::cpufeatures`, so the HAL producer and the `lib/cpuops` consumer
  share one definition without a `kernel/*` edge).
- **Performance** — "which equally-correct, feature-legal routine is fastest
  here?" — is the only thing a benchmark may decide, and only for a family that
  is bit-identical in output and handles no secret.

Benchmarking to discover whether an instruction *exists* is a defect: an absent
instruction traps, so the capability gate must be exact.

## The generic framework (`lib/cpuops`)

`lib/cpuops` is a `no_std`, platform-neutral crate — no board, SoC, or
`cfg(target_arch)` — holding the whole selection abstraction: a `Family` of
`Candidate`s plus a mandatory portable `baseline`, and a `Selector` that

1. **filters** out any candidate whose required `CpuFeature` bits are not all
   present (the absolute correctness gate),
2. **self-verifies** each survivor against the portable reference over a fixed
   vector of sizes/alignments/edge cases — a candidate whose output differs on
   any input is *rejected*, so a buggy accelerated path is structurally
   unpickable,
3. **chooses** by declared priority (`ByPriority`) or a bounded, deterministic
   median microbenchmark over an injected cycle counter (`ByBenchmark`),
4. **fails closed** to the baseline if nothing survives — never a trap, never a
   panic.

Results are keyed per **core type** (`OpsTables`), because big.LITTLE and
hybrid SMP mean a measurement on one cluster does not describe another. An
operator **pin** can force a named candidate for reproducibility — but a pinned
candidate still self-verifies, so pinning can never defeat correctness. Each
choice is a typed `Decision` recorded through the audit log.

## Where routine bodies live

- Portable baselines live in the owning `lib/*` crate.
- Hardware candidates that use ISA-specific intrinsics live behind a
  **`build.rs`-emitted per-architecture cfg** (the `lib/abi-trap` precedent),
  so no `cfg(target_arch)` appears in the source `cargo xtask cfg-check`
  guards. `lib/crc32c` is the worked example: its aarch64 `crc32c*` and x86_64
  SSE4.2 `crc32` candidates are gated by `crc32c_aarch64` / `crc32c_x86_64`.

## Delivering the feature set to programs

A user-space program cannot read ID registers, and a task may migrate between
cores, so the kernel hands each process the **migration-safe common feature
set** — the intersection over every core it may run on. Each core folds its own
detected `CpuFeatureSet` into that intersection as it comes online
(`kernel/core::cpuops`); the finalised value is stamped into every process's
startup vector (`ProcessStart::cpu_features`, read through
`tairix_rt::cpu_features`). Because it is the intersection, any instruction the
set advertises is legal on every core, so a dispatched routine can never trap
after a migration. Until the set is finalised it is empty — the program uses
the portable baseline, which is always correct (fail closed). The set is a
non-secret capability fact, so delivering it grants no authority.

## The crypto carve-out

Anything touching a secret is selected by **availability only**, never
benchmarked: a "fastest AES" contest would happily pick a table-driven variant
that leaks keys through cache timing. Cryptographic backends stay in the
audited `lib/crypto` and are chosen on the capability axis alone.

## First consumer: CRC-32C (`lib/crc32c`)

The fast, non-cryptographic block-integrity checksum ARXFS verifies every
at-rest data block with is CRC-32C (Castagnoli), through `lib/crc32c`. It is not
a cryptographic primitive — authenticity rests on the AEAD tag and the SHA-256
logical hash — so a first-party implementation is permitted. On a core with the
`crc32c*` / SSE4.2 instruction it runs in one general-purpose-register
instruction per word; everywhere else it is the portable table baseline, and
the hardware path is self-verified bit-identical to that baseline before it can
be selected. The kernel resolves it once, after SMP bring-up, against the
finalised common feature set.

## Consumer: page-zero (`lib/pagezero`)

Clearing memory to zero is one of the kernel's hottest and most
security-critical primitives: every freshly-allocated frame is zeroed before it
becomes user-visible (no stale bytes cross a process boundary), and every frame
that ever held a secret is scrubbed on free. `lib/pagezero` routes the
`kernel/mem` frame scrub through the framework: on a core with a block-zero
instruction it uses aarch64 `DC ZVA` (which clears a whole cache block without
a read-for-ownership) or x86_64 ERMS `rep stosb`, and the portable byte fill
everywhere else.

Page-zero sits firmly on the **capability** axis: a block-zero primitive is
unconditionally faster than a scalar loop when present and bit-identical in
result, so the choice is `ByPriority` (hardware first, portable baseline last)
— **never** benchmarked. Racing a page-zero benchmark at boot would be
pointless churn, and Linux likewise selects `DC ZVA`/ERMS by feature, not by
timing. The hardware candidate is self-verified against the byte fill — over a
fixed vector of lengths and alignments, including that it zeroes *exactly* the
requested region and touches nothing past it — before it can be selected. Like
CRC-32C, the kernel resolves one routine, after SMP bring-up, against the
finalised common feature set (a kernel routine may migrate between cores, so it
must be legal on all of them); the `ERMS` (x86_64) and `DcZva` (aarch64)
capability bits gate the two candidates.
