# `tairix-cpuops` — self-optimising CPU-dispatch framework

`lib/cpuops` is the runtime-ceiling half of the boot-time hardware-feature
framework (`plans/FIX-HARDWARE-FEATURES.md`). Every TAIRiX image is compiled
against a conservative **build-time floor** — the common instruction set of
every machine that image must boot — so the compiler may only emit
common-baseline instructions image-wide. This crate recovers, per booted core,
everything the conservative floor gives up: it selects the fastest *correct*,
feature-legal implementation of each accelerated operation from a registry of
candidates.

## Two axes, never conflated

- **Capability** — "does this core have the instruction?" — is a deterministic
  fact read from CPU-ID registers by the architecture ports into a
  `tairix_abi::cpufeatures::CpuFeatureSet`. This crate reads that set; it never
  benchmarks to discover whether an instruction exists (an absent instruction
  traps).
- **Performance** — "which feature-legal, bit-identical implementation is
  fastest here?" — is the only thing the optional benchmark decides.

Keeping the vocabulary in the dependency-free ABI crate
(`tairix_abi::cpufeatures`, mirroring `tairix_abi::hwtree`) lets the Arch HAL
*produce* a `CpuFeatureSet` and this framework *consume* the identical
definition without the framework taking a forbidden dependency on `kernel/*`.

## The selection algorithm

For each op `Family` on each distinct core type, the `Selector`:

1. **filters** out any `Candidate` whose required `CpuFeature` bits are not all
   present — the absolute capability gate, so an unsupported instruction is
   never reached;
2. **self-verifies** every survivor against the family's portable `reference`
   over a fixed vector of sizes, alignments, and edge cases — a candidate whose
   output differs on any input is rejected, so a buggy accelerated path is
   structurally unpickable;
3. **chooses** by declared priority (`ByPriority`) or by a bounded median
   microbenchmark (`ByBenchmark`) over an injected `CycleCounter`;
4. **fails closed** to the mandatory portable `baseline` if nothing above it
   survives. Selection never panics and never busy-waits.

An operator **pin** forces a named candidate for determinism (CI, debugging,
reproducible-build validation) — but the pinned candidate still self-verifies,
so pinning can never defeat correctness or reach an absent instruction.

The chosen implementations are stored in a per-core-type `OpsTables` (grown on
demand as each distinct core type comes up on `big.LITTLE` / hybrid silicon)
and consumed on the hot path. Each choice is a typed `Decision` the caller
records through a `DecisionSink` for the audit log; the crate itself performs
no I/O.

## The crypto carve-out

A family that touches a secret is `ByPriority` on **availability only** — the
audited hardware backend when its feature bits are present, else the audited
constant-time software backend. It is never benchmarked: a "fastest AES"
measurement could pick a table-driven variant that leaks keys through cache
timing. The `ByBenchmark` axis is restricted to routines that are bit-identical
in output and handle no secret.

## Rules

- `no_std` + `alloc`, no `unsafe`, no `cfg`, and no board / SoC / architecture
  name: concrete routines live in their owning crates and are gated on the
  discovered feature bits this framework matches against.
- The build stays bit-reproducible; only runtime *selection* varies, and a pin
  makes even that deterministic.

Stability tier: **experimental**.
