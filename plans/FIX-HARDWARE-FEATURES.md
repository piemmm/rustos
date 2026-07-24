# FIX-HARDWARE-FEATURES — Boot-time CPU feature detection and self-optimising routine selection

Status: **planned** (design fixed below; no code landed).

Binding under `AGENTS.md` (§3, §15.18). This plan turns the standing
performance defect — *every* TAIRiX image is built and run against the
lowest-common-denominator baseline of its architecture and never uses the
extra instructions or the faster routine the booted CPU actually offers —
into a first-class, modular, boot-time hardware-feature framework that is
**better than Linux's** `raid6_algos` / `xor` self-benchmark and its
`alternatives`/ifunc feature dispatch, because it splits the two decisions
Linux tangles together and makes both auditable, deterministic where
correctness demands it, and fail-closed.

The framework selects, once per boot per distinct core type, the best
*correct* implementation of each accelerated operation (CRC32, IP/RFC-1071
checksum, `memcpy`/`memset`/page-zero, framebuffer blit/blend/fill,
XOR/parity, and the crypto-backend *availability* decision) from a registry
of candidates, using: (1) a deterministic **capability gate** read from CPU
ID registers, (2) a mandatory **self-verify** against a portable reference,
and (3) either declared **priority** or a bounded **microbenchmark** — with
a portable baseline that is always feature-legal and always the last resort,
so the system never traps on a missing instruction and never panics.

## Read first (§15.18)

- `AGENTS.md` §2.16 (performance is first-class), §2.20/§2.21 (platform
  neutrality; least arch-specific code), §5.4/§2.9 (fail closed, no panic),
  §17.2 (Arch HAL is a closed trait set), §17.5 (`cfg-check`/`deps-check`),
  §18.1 (the hardware tree is the discovery precedent), §19.1/§2.12 (crypto
  constant-time; audited backends), §19.4 (audit log), §27 (foundational
  implementations are complete, not minimal).
- `kernel/arch/api/src/lib.rs` — how the closed HAL trait set is assembled
  and re-exported; the `conformance` vertical pattern.
- `kernel/arch/api/src/memtag.rs` and `kernel/arch/api/src/sidechannel.rs`
  — the template for a new closed HAL slice with honest
  `Supported`/`Unsupported(reason)`/`Pending(note)` profiles and a
  per-slice `conformance::run_all`.
- `kernel/arch/api/src/timer.rs` and `kernel/arch/x86_64/src/apic_timer.rs`
  (`Rdtsc`) — the timer surface and the existing x86_64 cycle reader the
  benchmark harness must generalise into a HAL cycle-counter primitive.
- `kernel/arch/api/src/platform.rs` (`PlatformDiscovery`/`HwNodeSink`) — the
  §18.1/§18.2 discovery precedent this framework mirrors for CPUs.
- `.cargo/config.toml`, `tools/xtask/src/commands.rs`,
  `tools/xtask/src/commands/pie_build.rs` — the build-time floor layer
  (`-C target-cpu`/`target-feature`), which this plan **layers under**, not
  replaces.
- `lib/crypto`, `lib/net` (RFC-1071 checksum), the ARXFS record-checksum
  path, `lib/raster` (blit/blend/fill) — the first consumers.

## The defect (why the status quo is unacceptable)

1. **Build-time floor only, and set to the baseline.** Nothing in the tree
   sets `-C target-cpu`/`target-feature`, so LLVM emits `target-cpu=generic`
   — plain ARMv8.0-A on the Cortex-A72 RPi4 image, generic x86-64 on the
   ISO. CRC32, the AArch64 crypto extension (AES/PMULL/SHA1/SHA2), NEON/AVX
   widths, and the core scheduling model are all left unused on exactly the
   §2.16 hot paths (crypto, checksums, allocator, FS/net, compositor).

2. **A single generic-per-arch image cannot bake in any optional
   extension.** The product model is **one generic floor image per
   architecture, not per-board** (decided below): a single `aarch64`
   installation media that boots RPi 4, ClockworkPi CM4, OrangePi, and other
   ARMv8 SBCs, and a single `x86_64` ISO that boots arbitrary PCs, each
   working the hardware out at runtime from device-tree/ACPI discovery. The
   build-time floor is therefore forced to the *common* feature set of every
   SoC/PC that image must boot; anything above that floor is reachable
   **only** by runtime detection + dispatch. A universal `aarch64` image's
   floor is essentially baseline ARMv8.0-A (A53∩A72∩A76∩Allwinner∩… ≈
   baseline); the generic x86_64 ISO's floor is very low too (unknown PCs),
   yet a booted PC may have AES-NI/AVX2/SHA-NI. Runtime dispatch is what
   recovers, per booted CPU, everything the conservative floor gives up — so
   one image per arch loses nothing a per-board image would have gained.

3. **No mechanism exists to pick the faster of two equally-correct
   routines.** Even within one feature level, method X can beat method Y on
   core A and lose on core B (the raid6/xor case). TAIRiX has no framework
   to measure and choose, so it always ships one hard-coded choice.

**Scope note — the kernel binary is already universal; the *media* is the
real open work.** "One image per arch" is two questions with the same answer
for different reasons. The **kernel/CPU** half — one binary per arch that
adapts to any board by reading its device tree (`plans/PI.md` §0.2/§0.3:
boards of the same arch differ only in runtime-discovered data, never
`cfg(board=…)`) plus the build-time floor + runtime ceiling below — is
settled here. The **boot/firmware/DTB/media** half (a multi-board boot
partition carrying the single kernel plus every supported board's DTB and
each firmware family's loader, board-detect → DTB-select) is a packaging
problem that differs board-to-board and is owned by `plans/BOOTLOADER.md`
(and `plans/PI.md` for RPi-family specifics), **not** this plan. Per-board
images are a rare escape hatch for a board whose boot handoff genuinely
cannot be unified onto shared media, never the default.

The fix is **layered**, and both layers are mandatory:

- **Build-time floor (already the right home, just unset):** the compiler
  may emit the common-baseline instructions inline, image-wide. Chosen per
  image in `tools/xtask`/`tools/mkimage` (see P0), never as a `cfg` in
  shared source.
- **Runtime ceiling (this plan):** an ops table selects the
  extension-using or measured-fastest implementation *only on cores that
  have the extension*, baseline everywhere else.

## Goal / invariants (the design that survives review)

1. **Two axes, never conflated.** *Capability* ("does this core have the
   instruction?") is a deterministic fact read from CPU ID registers, never
   a benchmark. *Performance* ("which equally-correct, feature-legal
   implementation is fastest here?") is the only thing the benchmark
   decides. Benchmarking to discover whether an instruction exists is a
   defect; see also invariant 8 (crypto).

2. **Detection is arch-specific and lives behind a closed HAL slice.**
   Reading `ID_AA64ISAR0_EL1`/`ID_AA64PFR0_EL1` (aarch64), `CPUID`
   (x86_64), `misa` + SBI/device-tree (riscv64), or the host capability
   query (wasm32) is target-divergent (§2.21) and belongs **only** under
   `kernel/arch/<target>/`, exposed through a new closed
   `kernel/arch/api/src/cpufeatures.rs` trait. The output is a normalised,
   arch-neutral `CpuFeatureSet` — the CPU analogue of the §18.1 hardware
   tree. No `cfg(target_arch)` leaks above the HAL (enforced by
   `cargo xtask cfg-check`, §17.5).

3. **The dispatch framework is generic and platform-neutral (§2.20).** The
   registry, selector, self-verify, benchmark harness, and ops-table type
   are one `no_std` `lib/*` crate (`lib/cpuops`) with **no** board/SoC/`cfg`
   reference. Adding it updates §3 and `PLAN.md`.

4. **Every candidate declares its required features; the gate is absolute.**
   A candidate is a filter-survivor only if *all* the `CpuFeatureSet` bits
   it requires are present. An unsupported instruction is never reached —
   that would trap. This is the correctness gate.

5. **Mandatory self-verify — an accelerated path with a bug is structurally
   unpickable.** Every filter-survivor is run against the portable
   reference over a fixed vector of sizes, alignments, and edge cases; a
   candidate whose output differs on any input is *rejected*, never
   selected. This is also the regression-test hook (P4).

6. **Fail closed, never panic (§5.4/§2.9).** The portable baseline is
   always the last-registered candidate, always feature-legal, and always
   self-verifies. If everything above it is filtered or rejected, the
   baseline wins. Selection never panics and never busy-waits (§2.23).

7. **Select once into an ops table; do not runtime-patch.** The winner per
   family is stored as a struct of `extern "C" fn` pointers consumed on the
   hot path. A select-once fn-pointer table avoids the W^X churn (§19.2) of
   Linux-style code patching and is far simpler. The indirect-call cost is
   *measured* (§2.16), not assumed; only if a specific ultra-hot path
   provably cannot afford the indirect call is patching reconsidered, with
   evidence and a stop-and-ask (§15.7).

8. **Crypto is capability-gated only — never benchmarked, never "fastest"
   (§19.1/§2.12).** For anything touching a secret, selection is
   *availability* only: hardware AES/PMULL/SHA present → the audited
   constant-time hardware backend; else the audited constant-time software
   backend. A "fastest AES" benchmark would happily pick a table-driven
   variant that leaks keys through cache timing — a §2.17 security
   regression. The `ByBenchmark` axis is restricted to routines that are
   (a) bit-identical in output and (b) handle no secret and have no
   timing-security requirement.

9. **Per-core-type, because cores are not uniform (§4 SMP).** big.LITTLE and
   heterogeneous SMP mean a measurement on the boot CPU does not describe
   another cluster. Results are keyed by CPU model / core type; the ops
   table is per-core-type (or per-cluster), resolved as each CPU comes up,
   never measured once and imposed globally.

10. **Deterministic, observable, pinnable.** The chosen implementation per
    family per core type is logged through the audit log (§19.4) with a
    stable event ID, and a boot parameter can *pin* a specific
    implementation. Builds stay bit-reproducible (§19.3); only runtime
    *selection* varies, and pinning makes it deterministic for CI,
    debugging, and reproducible-build validation.

11. **Foundational-complete from the start (§27).** The registry, both
    selection policies (`ByPriority` and `ByBenchmark`), the mandatory
    self-verify, per-core-type keying, pinning, and logging are all present
    in the first landed increment — not a one-family, one-policy slice.

## Layering (where each piece lives)

```
lib/cpuops                     generic: CpuFeatureSet consumer, Family/Candidate
  (no_std, no cfg, no SoC)     registry, Selector (filter→verify→choose),
                               BenchHarness, OpsTable<T>, pin/log seams.
kernel/arch/api/cpufeatures    closed HAL trait: read CpuFeatureSet + cycle
                               counter; conformance vertical.
kernel/arch/api/cpucycles      (or fold into timer.rs) arch-neutral cycle-
                               counter primitive for the bench harness.
kernel/arch/<target>/          the ONLY place that reads ID registers/CPUID/
                               misa/host query; per-port cycle counter.
concrete routines              portable ones in the owning lib/*/kernel/*;
                               ISA-divergent ones under kernel/arch/<target>/,
                               each gated on a discovered feature bit — one
                               generic impl per bit, never copy-pasted per arch.
kernel/core                    the single point that builds the per-core-type
                               ops tables at bring-up and hands them to consumers.
.cargo + tools/xtask/mkimage   build-time floor: image → target-cpu/feature.
```

## Phases (ordered, each independently reviewable and complete)

### P0 — Build-time floor (the layer under everything)

- Add a single build-layer mapping "image → `target-cpu`/`target-feature`"
  in `tools/xtask` (consumed by both the kernel build and the
  `cross_compile_pie_elf` PIE recipe, so kernel and user-space binaries in
  the same image share one floor — §2.2). It is **not** a blanket edit to
  the shared `[target.aarch64-unknown-none]` block.
- Choose each image's floor from *which SoCs/PCs it must boot*: a universal
  `aarch64` image's floor is baseline **ARMv8.0-A** (the common set of every
  ARMv8 SBC it boots — A53∩A72∩A76∩Allwinner∩… ≈ baseline), **not**
  `cortex-a72`; the generic x86_64 ISO's floor stays low (default
  `x86-64-v1` for maximum reach, raised only if a minimum-hardware
  requirement is published and documented); QEMU-virt images pick a
  documented model. Record the choice and its rationale. Everything above
  the floor is recovered per booted CPU by P1–P3 runtime dispatch, so the
  low floor costs nothing at runtime.
- **Product-model decision (settled — was the P0 open question).** TAIRiX
  ships **one generic floor image per architecture, not per-board**: a
  single `aarch64` media boots RPi 4 / CM4 / OrangePi / other ARMv8 SBCs and
  a single `x86_64` ISO boots arbitrary PCs, hardware worked out at runtime
  via discovery (§18.1) and CPU extensions via P1–P3 dispatch. This matches
  the charter's discovery-first design and `plans/PI.md` §0.2/§0.3, and is
  mandatory for x86_64 (per-board is impractical there). Per-board images
  are reserved as a rare boot-layer escape hatch (a board whose firmware
  handoff cannot be unified onto shared media), never the default; that
  escape hatch and the multi-board boot partition/DTB packaging are
  `plans/BOOTLOADER.md`'s concern, not this plan's. Reconciled against
  `plans/UNIVERSAL.md` (universal `.app`/Wasm distribution) and
  `plans/PI.md`.
- Validate that any floor feature actually lowers on the freestanding
  targets (the x86_64 block already pins *soft* crypto backends to dodge
  codegen crashes; expect to validate the aarch64 path the same way and
  **fail the build** rather than ship broken codegen — §2.1).
- Tests: an xtask unit test that the floor mapping is total (every shipped
  image resolves to a documented floor) and that kernel + PIE builds in one
  image get identical flags.

### P1 — The `cpufeatures` Arch HAL slice (deterministic capability layer)

- New closed slice `kernel/arch/api/src/cpufeatures.rs`: the
  `CpuFeatures` trait (`detect(CpuId) -> CpuFeatureSet`), the arch-neutral
  `CpuFeatureSet` bitset + `CoreType`/model id, honest per-feature
  `Supported`/`Unsupported(reason)`/`Pending(note)` where a probe is not
  yet trustworthy, and a `conformance::run_all` vertical. Record this new
  HAL surface in `PLAN.md` and §17.2 per the charter (mirrors the
  `shadowstack` authorisation precedent in `plans/FIX-PROTECTION.md`).
- Per-port impls under `kernel/arch/{x86_64,aarch64,riscv64,wasm32}/`
  reading the real ID sources. `wasm32` reports the host query; a feature
  the silicon genuinely lacks is honestly `Unsupported(reason)`, never a
  fabricated bit.
- The cycle-counter primitive the harness needs (generalise the x86_64
  `Rdtsc` into a HAL `cpu_cycles()` — PMCCNTR_EL0/RDTSC/rdcycle, host
  `performance.now()` on wasm32), added to the HAL (its own slice or
  folded into `timer.rs`, decided in P1) with a conformance check that it
  is monotonic within a measurement window.
- Tests: per-port conformance (detection returns a coherent set; masking a
  bit is honoured), host tests with a fake ID source.

### P2 — The generic `lib/cpuops` framework (registry + `ByPriority` + self-verify + fail-closed baseline)

- New `no_std` crate `lib/cpuops` (updates §3 + `PLAN.md`,
  stability tier in its `README.md`). Contents:
  `Family`/`Candidate<T>` (T = the op's fn-pointer signature),
  required-`CpuFeatureSet`, `Selection::{ByPriority, ByBenchmark}`,
  `Selector` (filter → self-verify → choose → fail-closed baseline),
  `OpsTable<T>`, and the pin/log seams (injected, capability-clean — the
  crate does no I/O and no logging itself; it emits typed decisions the
  caller logs via `lib/log`, §19.4).
- Land the full abstraction (§27): both policies present, self-verify
  mandatory, per-core-type keying, pinning, logging — even though P2's
  first consumers use only `ByPriority`.
- First deterministic consumers (biggest safe wins, zero nondeterminism):
  CRC32 (`+crc`/CRC32 vs portable table) and the **crypto backend
  availability** decision (capability-gated per invariant 8). Wire the
  per-core-type ops-table build into `kernel/core` bring-up.
- Tests: host-test the selector with a fake `CpuFeatureSet` (filters
  correctly, rejects a deliberately-wrong candidate, falls back closed,
  honours a pin); conformance that every candidate is bit-identical to the
  reference across sizes/alignments/edge cases.

### P3 — The `ByBenchmark` axis (bounded, deterministic boot microbenchmark)

- Add the `BenchHarness`: fixed iteration budget over a fixed, warmed input
  buffer, measured with the HAL cycle counter, median-of-N to reject noise,
  **never** "loop until a threshold" (that is the §2.1 retry-until-it-works
  hack). Bounded one-shot during bring-up; no busy-wait (§2.23).
- Per-core-type measurement (invariant 9): benchmark per distinct core type
  / cluster and keep a per-core-type ops table; do not impose the boot
  CPU's result globally.
- `ByBenchmark` consumers — secret-free, bit-identical families only:
  `memcpy`/`memset`/page-zero, framebuffer blit/blend/fill (`lib/raster`),
  RFC-1071 IP checksum (`lib/net`), XOR/parity. Crypto stays strictly on
  P1/P2's capability axis.
- Log the chosen impl per family per core type (§19.4, stable event ID);
  honour the pin boot parameter to force determinism.
- Tests: host-test the harness with a fake timer (bounded, deterministic,
  picks the faster of two fakes, honours a pin); QEMU verticals per arch
  proving the extension-using family is chosen when the feature is present
  and the baseline is chosen when features are masked off.

### P4 — Fuzzing, docs, and burn-down

- Fuzz every accelerated routine against its portable reference (§19.6);
  crashing/divergent inputs enter the regression corpus with a unit test
  (§7). Add `cargo xtask fuzz` harnesses for each family.
- `docs/src/architecture/` page describing the two axes, the crypto
  carve-out, per-core-type keying, pinning, and the build-time-floor vs
  runtime-ceiling layering; rustdoc on every public `lib/cpuops` and HAL
  item (§2.8/§13). Add the P0 image-floor rationale to the platform docs.
- Update the `README.md` feature/architecture matrix (§13) with the
  per-target accelerated-path state.

## Testing (§7)

- Selector: filter, self-verify rejection, fail-closed baseline, pin
  honoured — host tests with fakes.
- Conformance: every candidate bit-identical to the reference across a
  fixed vector of sizes/alignments/edge cases (empty, 1 byte, unaligned,
  page-crossing, max).
- Benchmark harness: bounded, deterministic under a fake timer, correct
  winner, pin honoured.
- Per-arch QEMU verticals: correct family chosen on the real target;
  baseline chosen when features are masked; heterogeneous-core keying
  exercised where the emulator supports it.
- Fuzz (§19.6) each routine vs reference; crashes → corpus + unit test.
- The whole-project validation gate (§7) is green before any phase is done.

## Explicit non-goals / guardrails

- **No crypto benchmarking, ever** (invariant 8). Crypto selection is
  availability-only and stays in the audited `lib/crypto` backends (§2.12).
- **No `cfg(target_arch)` above the HAL** (§17.2/§17.5). Detection lives
  only under `kernel/arch/<target>/`; the framework and routines are
  generic and gated on discovered bits (§2.20/§2.21).
- **No runtime code patching / ifunc** unless a specific ultra-hot path
  proves the fn-pointer indirection unaffordable, with measurement and a
  stop-and-ask (§15.7). Select-once fn-pointer tables are the default.
- **No build reproducibility regression** (§19.3): the build is
  bit-reproducible; only runtime selection varies, pinnable for
  determinism.
- **No "for now" / no-op selection** (§2.19): the baseline is a real,
  correct routine, not a stub; every phase lands complete.
