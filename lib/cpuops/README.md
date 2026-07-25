# `tairix-cpuops` — self-optimising CPU-dispatch framework

Stability tier: **experimental**.

`lib/cpuops` is the one generic, platform-neutral place TAIRiX decides *which*
equally-correct implementation of an accelerated operation to run on the CPU it
actually booted on. It is the runtime-ceiling half of
`plans/FIX-HARDWARE-FEATURES.md`: the build-time floor (`tools/xtask` P0) lets
the compiler emit only the common-baseline instructions image-wide, and this
crate recovers, per booted core, everything the conservative floor gives up.

## Two axes, never conflated

- **Capability** — "does this core have the instruction?" — is a deterministic
  fact read from CPU-ID registers by the architecture ports into a
  `tairix_abi::cpufeatures::CpuFeatureSet`. This crate only *reads* that set; it
  never benchmarks to discover whether an instruction exists (an absent
  instruction traps).
- **Performance** — "which feature-legal, bit-identical implementation is
  fastest here?" — is the only thing the optional benchmark decides.

## What the crate gives you

- `Candidate<T>` / `Family` — one implementation, and the op it belongs to
  (its candidates, a mandatory portable `baseline`, a portable `reference`, and
  the self-verify `vectors`).
- `Selector` — the pure algorithm: **filter** on the capability gate,
  **self-verify** every survivor against the reference (a buggy accelerated
  path is structurally unpickable), **choose** by declared priority or by
  bounded benchmark, and **fail closed** to the baseline. Honours an operator
  **pin** — which still self-verifies, so a pinned buggy candidate falls to
  baseline.
- `BenchHarness` over an injected `CycleCounter` — a bounded, one-shot,
  median-of-rounds microbenchmark. No busy-wait, no "loop until a threshold".
- `OpsTables<Ops>` — the resolved, per-core-type table the hot path consumes,
  grown on demand as each distinct core type comes up (`big.LITTLE`, Intel
  hybrid), never a fixed ceiling.
- `Decision` / `DecisionSink` — a typed record of every choice for the audit
  log. The crate performs no I/O; the caller (`kernel/core`) logs it.

## Rules this crate keeps

- `no_std` + `alloc`, no `unsafe`, no `cfg`, no board/SoC/arch name — the
  routines are generic and gated on discovered bits (§2.20/§2.21).
- Crypto is never benchmarked: a crypto family is `ByPriority` on *availability*
  only, so a benchmark can never pick a key-leaking table-driven variant
  (§19.1/§2.12). The `ByBenchmark` axis is only for secret-free, bit-identical
  routines.
- Fail closed, never panic: the baseline is a real, correct routine and always
  the last resort (§5.4/§2.9).
