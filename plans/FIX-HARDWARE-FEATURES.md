# FIX-HARDWARE-FEATURES — Boot-time CPU feature detection and self-optimising routine selection

Status: **in progress** — P0 (build-time floor), P1 (the `cpufeatures`
/`cpucycles` Arch HAL slices), the P2 **framework crate** (`lib/cpuops`,
foundational-complete), and the P2 **first consumer + delivery** done: the
`lib/crc32c` CRC-32C family (portable baseline + per-arch `crc32c*`/SSE4.2
candidates, `lib/cpuops`-selected + self-verified + host-fuzzed), the
migration-safe common-`CpuFeatureSet` delivery (kernel folds each core's set
into an intersection, stamps it into every `ProcessStart`, exposes it via
`lib/rt::cpu_features`), the `kernel/core` bring-up resolve+audit
(`AuditEvent::CpuOpsRoutineSelected`), and the ARXFS `physical_checksum`
consumer (FNV-1a → CRC-32C, on-disk trailer 8→4). The P2 **crypto-availability
consumer** is also done: `lib/crypto::backend` is the authoritative SHA-256
backend-availability decision routed through `lib/cpuops` as an
availability-only (`ByPriority`, never benchmarked) family, whose mandatory
self-verify is a boot-time FIPS-180-4 known-answer self-test (POST) of the live
SHA-256 path; `kernel/core` records the decision and **halts** on a POST failure
(`AuditEvent::CryptoSelfTestFailed`), the FIPS discipline. It does not fork the
crypto computation (§2.12 forbids hand-rolling; the audited `sha2` crate owns
backend selection): on `x86_64` the crate's own no-OS-safe `CPUID` detection
selects SHA-NI, so the hardware-availability candidate is offered/recorded
there; on `aarch64`/`riscv64`/`wasm32` there is no runtime-selected hardware
SHA-256 path, so the honest software answer is recorded. Recovering hardware
SHA-256 on `aarch64` (whose `sha2` HWCAP gate is inert on `target_os="none"`)
awaits a **vetted, driveable audited backend** — a supply-chain decision,
deliberately not faked.

The P3 **page-zero** consumer is also done: `lib/pagezero` is a capability-gated
(`ByPriority`) family — portable byte-fill baseline + per-arch hardware
candidates (aarch64 `DC ZVA` cache-block zero, x86_64 ERMS `rep stosb`), gated
on the new `DcZva`/`Erms` `CpuFeatureSet` bits, `lib/cpuops`-selected +
self-verified + host-fuzzed. The `kernel/mem` frame scrub (`zero_frame` /
`fill_frame`: zero-before-map and the zero-on-free secret scrub) routes through
it, and `kernel/core::cpuops::resolve_accelerated_ops` resolves it once against
the finalised common set and audits the choice (`CpuOpsRoutineSelected`).

**A design correction landed with it (§2.13, no staged migration):** page-zero
was listed below under `ByBenchmark`; that is wrong. A block-zero primitive is
unconditionally better when present and bit-identical, so it is chosen by
*capability* (`ByPriority`), never a benchmark — exactly as Linux selects
`DC ZVA`/ERMS by feature. `memcpy`/`memset` fall in the same bucket where a
hardware fill/copy dominates. The plan is corrected accordingly below.

Remaining: that aarch64 hardware-crypto backend; the genuine **`ByBenchmark`**
demonstration (below); and P4 (matrix + burn-down). Design fixed below.

**Open design question for the `ByBenchmark` axis (must be resolved before its
consumers land, §15.7).** The families where a benchmark genuinely decides
(the raid6/xor case: best SIMD width varies by microarch) are the framebuffer
blit/blend/fill (`lib/raster`) and the RFC-1071 IP checksum (`lib/net`) — both
**userland**. But the bounded microbenchmark measures over the Arch HAL
`CpuCycles` counter, which is **kernel-only**: there is no mechanism today for
a user-space process to obtain a per-core-type "fastest routine" decision.
Resolving this needs a deliberate design (e.g. `lib/rt` benchmarks its own
candidates at startup keyed by the delivered feature set over a cycle-counter
access path, or the kernel measures per core type and delivers the winners),
which is a separate, ABI-touching sub-plan — not started. The plan's former
XOR/parity `ByBenchmark` family is **dropped** until a real RAID/FEC/parity
consumer exists (a family with no caller is speculative/dead surface, §2.4);
`lib/cpuops`'s `ByBenchmark` policy + `BenchHarness` remain fully implemented
and host-tested against a fake counter, ready for the first real consumer.

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

**Phase dependencies.** P0 is independent (build-layer only) and can land
first or in parallel. P1 (the `cpufeatures`/`cpucycles` HAL slices) is the
prerequisite for P2 and P3 and depends on nothing but the existing HAL. P2
(the `lib/cpuops` framework + first `ByPriority` consumers) depends on P1's
`CpuFeatureSet`. P3 (the `ByBenchmark` axis) depends on P2's selector seam
*and* P1's `CpuCycles` counter. P4 (fuzz/docs/burn-down) depends on whatever
consumers P2/P3 landed. Each phase ends only when the whole-project §7 gate
is green (§2.15); the per-phase **Acceptance** blocks state the phase-local
bar on top of that.

### P0 — Build-time floor (the layer under everything) — **done**

The per-image CPU floor lives in `tools/xtask/src/floor.rs` (declared
`mod floor;` in `tools/xtask/src/main.rs`): the single source of truth for
the `-C target-cpu`/`-C target-feature` baseline each image is compiled
against. It is injected per image at the point each image's binaries are
built (never in the shared `.cargo/config.toml` `[target.*]` blocks, which
are consumed by every build for that triple and would leak a floor across
images).

**Shape.** `enum ImageKind { AArch64Generic, X86_64Iso, Riscv64Generic,
AArch64Virt }` (each carries its triple); `struct CpuFloor { target_cpu:
Option<&str>, target_features: &[&str], rationale: &str, triple: &str }`;
`floor_for_image(ImageKind) -> CpuFloor` is total over `ImageKind`.
`CpuFloor::floor_tokens()` yields the floor-only `-C target-cpu`/`-C
target-feature` tokens (empty for a baseline floor); `CpuFloor::rustflags()`
= `base_rustflags(triple)` ⧺ floor tokens; `CpuFloor::encoded_rustflags()`
is that joined by `0x1f` for `CARGO_ENCODED_RUSTFLAGS`.

**Flag precedence + the divergence guard.** `CARGO_ENCODED_RUSTFLAGS`
*replaces* (does not merge with) the config `[target.*]` `rustflags` block,
so `base_rustflags(triple)` reproduces the flags that block supplies
(`-C force-frame-pointers=yes` on the bare-metal targets; plus the x86_64
soft-crypto `--cfg`s). The `base_rustflags_match_cargo_config` unit test
parses `.cargo/config.toml` and pins the two together, so they can never
diverge (§2.2).

**Wiring.** The kernel builds (`build_platform_image` → `AArch64Generic`,
`build_virt_run_kernel` → `AArch64Virt`) set `CARGO_ENCODED_RUSTFLAGS =
floor.encoded_rustflags()`. The PIE recipe (`cross_compile_pie_elf`)
*prepends* `floor.floor_tokens()` to its link recipe, resolving the floor
purely from its `PieArch` via `ImageKind::generic_for_pie_arch` (every
bundle is the generic per-arch user-space, so the floor is a pure function
of the arch — never a value carried two ways, §2.2/§2.3). Kernel and
user-space of the generic image therefore share one floor
(`generic_image_kernel_and_pie_share_one_floor`).

**Floor values (decided, at baseline).** `AArch64Generic`: baseline
ARMv8.0-A (`target_cpu: None`, no features) — the common set of every ARMv8
SBC the universal media boots. `X86_64Iso`: `target_cpu: Some("x86-64")`
(v1) for maximum reach; raised to `x86-64-v2` only with a documented minimum
requirement. `Riscv64Generic`: the base `rv64gc` the triple implies.
`AArch64Virt`: baseline (dev kernel, not shipped hardware). Everything above
the floor is recovered per booted CPU by P1–P3 runtime dispatch, so the low
floor costs nothing at runtime. Because every floor is baseline, the injected
flags reproduce the config byte-for-byte and the images build exactly as
before (verified: `cargo xtask ci`'s image gate builds the RPi image green).

**Codegen-validation obligation for a future floor-raise.** Any floor that
raises `target-cpu`/`target-feature` above the current default must be proven
to lower on the freestanding target before adoption — the x86_64 target pins
*soft* crypto backends (`chacha20_force_soft`, `poly1305_force_soft`,
`curve25519_dalek_backend="serial"`) to dodge the freestanding-SIMD codegen
crash. If enabling a feature reintroduces that crash, **fail the build**
(§2.1); never ship broken codegen and never silently drop the feature. The
decided floors stay at baseline, so nothing raises codegen today.

**Product model (settled).** One generic floor image per architecture, not
per-board; hardware worked out at runtime via discovery (§18.1) and CPU
extensions via P1–P3 dispatch. Per-board images are a rare boot-layer escape
hatch owned by `plans/BOOTLOADER.md`, never the default.

### P1 — The `cpufeatures` Arch HAL slice (deterministic capability layer) — **done**

**Delivered.** Two closed HAL slices landed, modelled slot-for-slot on the
`memtag`/`sidechannel` slices, each with a `kernel/arch/api` conformance
vertical every port passes:

- `kernel/arch/api/src/cpufeatures.rs`: the `CpuFeatures` trait
  (`detect → CpuFeatureSet`, `core_type → CoreType`, `profile →
  FeatureProfile`), the closed `CpuFeature` enum (aarch64
  CRC32/AES/PMULL/SHA1/SHA2/SHA3/LSE/ASIMD/DIT, x86_64
  SSE2/SSSE3/SSE4.2/AVX/AVX2/AES-NI/PCLMULQDQ/SHA-NI/RDRAND/RDSEED,
  riscv64 Zbb/Zbc/Zbkc/V) with stable per-bit discriminants, the
  `CpuFeatureSet` bitset (`contains`/`contains_all`/`with`/`bits`), the
  `CoreType` per-core-type key (`raw_id` is the discriminator; `model`
  best-effort, `class` the homogeneous default), and the honest
  `FeatureProfile`/`FeatureSupport` (`Supported`/`Unsupported`/`Pending`)
  vocabulary — the last also being the reporting type a *probed* platform
  capability (e.g. the D13 watchdog's FIQ deliverability) uses.
- `kernel/arch/api/src/cpucycles.rs`: the `CpuCycles` trait
  (`cpu_cycles`, `cycles_monotonic_hint`) — its own slice, not grafted
  onto `timer.rs`.

Per-port impls (each the sole reader of its ID source; pure decoders
host-tested, register reads gated to `target_os = "none"`, host builds
report empty/unknown):

- x86_64 `cpufeatures.rs`: `CPUID` leaf 1 + leaf 7 decode, vendor +
  leaf-1 signature `CoreType`, `RDTSC` cycles (invariant-TSC hint).
- aarch64 `cpufeatures.rs`: `ID_AA64ISAR0_EL1`/`ID_AA64PFR0_EL1` decode,
  `MIDR_EL1` `CoreType`, `CNTVCT_EL0` cycles (chosen over `PMCCNTR_EL0`
  to avoid PMU-enable boot wiring).
- riscv64 `cpufeatures.rs`: `misa` V bit + pure `riscv,isa`-string
  multi-letter (`Zbb`/`Zbc`/`Zbkc`) parser, `time`-CSR cycles (reuses the
  one `read_time()` the clock uses).
- wasm32 `cpufeatures.rs`: honest `Unsupported` (a guest sees no native
  ISA), empty detection, `performance.now()` cycles.

Wired into `conformance::run_all` (new `cpu_features` handle) and every
port's `passes_arch_hal_conformance_suite`; recorded in the §17.2 HAL
enumeration and `PLAN.md`. The design that was fixed below is kept for
reference by P2/P3.

**Deliverables — the HAL trait (`kernel/arch/api/src/cpufeatures.rs`):**

- `pub struct CpuFeatureSet(u64)` — an arch-neutral bitset with named,
  arch-tagged accessors so no raw bit index leaks to consumers. Members
  cover the extensions consumers actually gate on:
  - aarch64: `CRC32`, `AES`/`PMULL`, `SHA1`, `SHA2`, `SHA3`, `LSE` (atomics),
    `ASIMD`/NEON (baseline-present but represented for completeness), `DIT`.
  - x86_64: `SSE2`, `SSSE3`, `SSE4_2` (carries CRC32/`crc32` + `POPCNT`),
    `AVX`, `AVX2`, `AES` (AES-NI), `PCLMULQDQ`, `SHA` (SHA-NI), `RDRAND`,
    `RDSEED`.
  - riscv64: the `misa`/ISA-string extensions relevant now (`Zbb`, `Zbc`,
    `Zbkc`, the vector `V` extension) — represented honestly, most
    `Unsupported`/absent on today's QEMU virt.
  - Provide `pub const fn contains(self, feature: CpuFeature) -> bool` and a
    `CpuFeature` enum naming each bit, so a `Candidate`'s required-feature
    declaration (P2) is type-checked, never a magic mask.
- `pub struct CoreType { pub model: Option<&'static str>, pub class:
  CoreClass, pub raw_id: u64 }` — the per-core-type key P2/P3 hash on;
  reuses the existing `CoreClass` (`lib.rs`) and the existing per-port model
  decoders (`cpuname.rs::name_for_midr` on aarch64, `cpuname.rs` on x86_64).
  `raw_id` is MIDR_EL1 / CPUID signature / `mvendorid:marchid:mimpid`.
- `pub trait CpuFeatures: Send + Sync { fn detect(&self, cpu: CpuId) ->
  CpuFeatureSet; fn core_type(&self, cpu: CpuId) -> CoreType; fn profile(&self)
  -> FeatureProfile; }` — object-safe, `Send + Sync` (reached from every CPU),
  detection keyed by `CpuId` because heterogeneous SMP means per-CPU answers.
- `pub struct FeatureProfile` + `pub enum FeatureSupport {
  Supported, Unsupported(&'static str), Pending(&'static str) }` and
  `validate()`/`entries()`/`is_release_ready()` — the *identical* honesty
  pattern as `memtag::TaggingProfile`/`Tagging` (§19.10 template), so a port
  that cannot yet trust a probe declares it `Unsupported`/`Pending` with a
  justification rather than fabricating a bit.
- `pub mod conformance { pub fn run_all<C: CpuFeatures + ?Sized>(port: &C) }`
  — asserts: profile validates; `detect` is stable across back-to-back calls
  for one `CpuId`; a bit reported present is consistent with the profile;
  `core_type` is total and panic-free for an out-of-range `CpuId`. Wire into
  `kernel/arch/api/src/conformance.rs::run_all` (add a `cpufeatures`
  parameter alongside `memtag`/`percpu`).
- Re-export from `kernel/arch/api/src/lib.rs` (`pub use cpufeatures::{…}`)
  and add `pub mod cpufeatures;`.

**Deliverables — the HAL cycle counter (decision: its own tiny slice,
`kernel/arch/api/src/cpucycles.rs`, not folded into `timer.rs`):** `timer.rs`
is the tickless scheduler-tick surface and must not grow a benchmarking verb
(interface creep, §2.4). New `pub trait CpuCycles: Send + Sync { fn
cpu_cycles(&self) -> u64; fn cycles_monotonic_hint(&self) -> bool; }` with a
`conformance::run_all` proving the counter is non-decreasing across a short
busy window. Generalises the existing x86_64 `Rdtsc`
(`kernel/arch/x86_64/src/apic_timer.rs`/`tsc.rs`).

**Deliverables — per-port impls** (each is the ONLY place its ID source is
read — §17.2/§17.5 `cfg-check`):

- `kernel/arch/x86_64/src/cpufeatures.rs`: `CPUID` leaves 1 (ECX/EDX) and 7
  (EBX/ECX) → the bitset; reuse `cpuname.rs` for `model`; `cpu_cycles` = the
  existing `Rdtsc`.
- `kernel/arch/aarch64/src/cpufeatures.rs`: `ID_AA64ISAR0_EL1` (AES/SHA/CRC32/
  atomics fields), `ID_AA64PFR0_EL1` (ASIMD); `core_type` from
  `cpuname.rs::name_for_midr` + `hetcore.rs` class; `cpu_cycles` =
  `PMCCNTR_EL0` (enable via `PMUSERENR`/`PMCR` at boot) or `CNTVCT_EL0`
  fallback if the PMU cycle counter is unavailable — record which in the
  profile.
- `kernel/arch/riscv64/src/cpufeatures.rs`: `misa` + the device-tree
  `riscv,isa` string (via `tairix_fdt`) for Z-extensions; `cpu_cycles` =
  `rdcycle`/`time` CSR.
- `kernel/arch/wasm32/src/cpufeatures.rs`: honest host query — most
  extensions `Unsupported("wasm host does not expose native ISA extensions")`;
  `cpu_cycles` via the host `performance.now()` binding already in
  `wasm32/src/bindings.rs`.
- Publish each port's handle where the other HAL handles are published
  (the port's `kernel_arch.rs` aggregation) so `kernel/core` can reach it at
  bring-up (P2).

**Record the new surface:** add `cpufeatures`/`cpucycles` to the §17.2 HAL
enumeration in `AGENTS.md` and to `PLAN.md`'s HAL list, and to the
`kernel/arch/api/src/lib.rs` module rustdoc (mirrors how `memtag`/`smp`
were recorded).

**Tests:**

- Host tests per port with a **fake ID source**: a decoder unit test that a
  synthetic `CPUID`/`ID_AA64ISAR0_EL1`/`misa` value yields the expected
  `CpuFeatureSet` (masking a field off is honoured — the bit disappears).
- `conformance::run_all` invoked from each port's existing `conformance`
  host test over its real handle.
- Cycle-counter conformance: monotonic within a measurement window (host
  double for the non-native side).

**Acceptance:** detection compiles and its decoders are host-tested on all
four ports; no `cfg(target_arch)` appears above the HAL (`cargo xtask
cfg-check` green); the HAL conformance vertical runs the new slice.

### P2 — The generic `lib/cpuops` framework (registry + `ByPriority` + self-verify + fail-closed baseline)

**Framework landed (design as built).** The `lib/cpuops` crate is complete and
foundational (§27), with these decisions refining the sketch below:

- The arch-neutral capability vocabulary (`CpuFeature`/`CpuFeatureSet`) lives in
  `tairix_abi::cpufeatures`, not in the HAL, so a `lib/*` crate can consume it
  without a forbidden `kernel/*` edge (§17.4); the HAL re-exports it. This is
  the one place both the ID-register *producer* (HAL) and the *consumer*
  (`lib/cpuops`) share the definition (§2.2), mirroring `tairix_abi::hwtree`.
- `FamilyId(&'static str)` is a stable string label, **not** a closed enum: the
  framework is generic over which families exist (a consumer declares its own),
  so a family enum would be speculative surface the framework does not own
  (§2.3/§2.4).
- The per-core-type key is `CoreKey(u64)` (the HAL `CoreType::raw_id`), so the
  framework needs neither `CoreType` nor `CoreClass` and stays `kernel/*`-free.
- `OpsTable` is `OpsTables<Ops>`, generic over the consumer's own struct of
  resolved fn pointers (the framework cannot name a consumer's op set), grown on
  demand per `CoreKey` (§24.1, not a fixed ceiling).
- The benchmark harness (P3) is included now — the crate is foundational-
  complete with **both** policies real — over a crate-local `CycleCounter` seam
  (`kernel/core` adapts the HAL `CpuCycles` to it) so the crate names no arch.
- Every `unwrap`/`expect`/`panic!` is kept out of the production path; the
  selector fails closed to the baseline (§2.9/§5.4).

Full host tests cover: feature-gate filtering, self-verify rejection,
fail-closed baseline (incl. no-vectors → `BaselineUnverified`), pin /
pin-rejected-illegal / pin-rejected-buggy / unknown-pin, `ByBenchmark` picking
the fastest over a deterministic fake counter (+ no-harness priority fallback,
tie-to-earliest, median outlier rejection), `OpsTables` grow-once-per-core-type,
and the `DecisionSink`.

**P2 done.** The `kernel/core` bring-up wiring finalises the migration-safe
common `CpuFeatureSet` (each core folds its own detected set into an
intersection) and, once final, resolves both P2 consumers and records each
`Decision` on the `lib/log` audit sink (`AuditEvent::CpuOpsRoutineSelected`):
the CRC-32C family (consumed by the in-kernel ARXFS `physical_checksum`) and the
crypto SHA-256 backend-availability family (`lib/crypto::backend`). The crypto
family additionally drives a fatal boot halt on a failed known-answer self-test
(`AuditEvent::CryptoSelfTestFailed`). The one thing P2 could not deliver — a
TAIRiX-fn-pointer-*routed* hardware crypto backend on `aarch64` — is blocked by
§2.12 (hand-rolling forbidden) plus the pinned audited crates (`sha2`'s aarch64
HWCAP gate is inert on `target_os="none"` and exposes no driveable override), so
it is deferred to a **vetted, driveable audited backend** (a supply-chain
decision), and the honest software answer is recorded there in the meantime —
never a candidate that would not run (§2.19).

**Scope in one line:** a new `no_std` `lib/cpuops` crate holding the whole
selection abstraction (registry, self-verify, both policies, per-core-type
keying, pin, typed log), wired once in `kernel/core` bring-up, with CRC32 and
the crypto-availability decision as its first two consumers.

**Deliverables — the crate (`lib/cpuops/`; updates §3 layout + `PLAN.md`;
`README.md` stability tier `experimental`; `no_std`, no `cfg`, no SoC name —
§2.20):**

- `pub struct Candidate<T: Copy>` — one implementation: `{ name:
  &'static str, requires: &'static [CpuFeature], selection_hint: (), impl_:
  T }` where `T` is the op's `extern "C" fn` pointer type. `requires` is
  matched against the `CpuFeatureSet` (P1).
- `pub struct Family<'a, T: Copy, In>` — the op abstraction: `{ id:
  FamilyId, candidates: &'a [Candidate<T>], baseline: Candidate<T>,
  selection: Selection, reference: fn(&In) -> RefOut, run: fn(T, &In) ->
  RefOut, vectors: &'a [In] }`. `baseline` is separate and mandatory (always
  feature-legal, always last resort — invariant 6). `FamilyId` is a stable
  enum used as the log/pin key.
- `pub enum Selection { ByPriority, ByBenchmark }` (invariant 1).
- `pub struct Selector` — the algorithm, pure and host-testable:
  1. **filter** candidates whose `requires` ⊄ the core's `CpuFeatureSet`
     (invariant 4);
  2. **self-verify** each survivor: run it over every vector and compare to
     `reference`; reject on any mismatch (invariant 5);
  3. **choose**: `ByPriority` → first verified survivor in declared order;
     `ByBenchmark` → hand survivors to the P3 `BenchHarness` (a seam in P2, a
     real harness in P3);
  4. **fail closed**: if none survive, return `baseline` (invariant 6);
     never panic, never spin.
  Returns a `Selection Decision { family: FamilyId, chosen: &'static str,
  core: CoreType, reason: DecisionReason }` — a *typed* record, not a log
  call (the crate does no I/O; the caller logs it via `lib/log`, §19.4).
- `pub struct OpsTable` — the resolved struct-of-fn-pointers consumed on the
  hot path, built per `CoreType` (invariant 9). `pub struct OpsTables` keying
  `CoreType → OpsTable`, resolved as each CPU comes up.
- `pub struct Pin` / `pub fn apply_pin(&mut Selector, FamilyId, &str)` — the
  boot-parameter override (invariant 10): pin forces a named candidate but it
  **still self-verifies** (a pinned buggy candidate is rejected, falls to
  baseline — pinning cannot defeat correctness).
- `pub trait DecisionSink { fn record(&self, d: &Decision); }` — the injected
  log seam (capability-clean; `kernel/core` supplies a `lib/log`-backed impl).

- **Foundational-complete from the first commit (§27):** both policies, the
  mandatory self-verify, per-core-type keying, pin, and the typed decision
  sink are all present now — even though P2's two consumers use only
  `ByPriority`. A one-family/one-policy slice is the §27 defect and is
  rejected.

**Deliverables — first two consumers:**

- **CRC32 family.** Portable table-driven CRC32 is the `baseline`; the
  extension candidate uses the aarch64 `crc32*` instructions / x86_64
  `crc32` (`SSE4_2`) gated on the `CRC32`/`SSE4_2` bit. The ISA-divergent
  candidate bodies live under `kernel/arch/<target>/` (one impl per bit, not
  copy-pasted — §2.21); the portable baseline and the `Family` wiring live in
  the owning crate. Identify the current CRC32/`physical_checksum` consumer:
  ARXFS `drivers/filesystem/arxfs/src/integrity.rs::physical_checksum` (a
  fast non-crypto checksum, explicitly *not* a crypto primitive) is the first
  real caller — route it through the ops table.
- **Crypto backend availability decision (capability-gated only — invariant
  8).** *No benchmark.* `lib/crypto::backend` is a `ByPriority` SHA-256 family:
  a hardware-availability candidate (offered only where the audited crate
  genuinely uses a hardware path selectable without an OS — today `x86_64`, via
  the `crypto_hw_sha256` build cfg, requiring the `ShaNi`/`Sse42`/`Ssse3`/`Sse2`
  bits `sha2` gates its SHA-NI path on) and the audited constant-time software
  baseline. Selection is *availability*, never speed, and the chosen backend
  still comes from the audited `lib/crypto` (§2.12) — the module does **not**
  fork the computation (the audited crate owns backend selection internally;
  transcribing SHA rounds over intrinsics would be hand-rolling, forbidden).
  Its mandatory self-verify is a **boot-time FIPS-180-4 known-answer self-test
  (POST)** of the live SHA-256 path; `kernel/core` halts on failure
  (`CryptoSelfTestFailed`). This proves the framework models the crypto
  carve-out — availability + self-test + audit, no benchmark near a secret —
  and honestly records `Software` on targets with no driveable hardware path.

**Deliverables — bring-up wiring (`kernel/core`):** the single point (§17.1
selection-point precedent) that, as each CPU comes up, reads the port's
`CpuFeatures` handle, builds/looks-up the `OpsTable` for that `CoreType`, and
records each `Decision` through the `lib/log` sink. Consumers fetch their
family's fn pointer from the per-CPU/per-core-type `OpsTable`.

**Tests:**

- Selector (host, fake `CpuFeatureSet` + fake vectors): filters on missing
  feature; rejects a deliberately-wrong candidate (verify catches it); falls
  back to baseline when all survivors rejected; honours a pin; a pinned
  *buggy* candidate still falls to baseline.
- Conformance: every CRC32 candidate is bit-identical to the reference across
  the fixed vector (empty, 1 byte, unaligned, page-crossing, max).
- Crypto family: selects the hardware backend only when the availability bit
  is set; never runs a benchmark (assert the `ByBenchmark` path is never
  entered for this family).

**Acceptance:** `lib/cpuops` lands complete (§27); CRC32 and crypto-avail
route through it; `kernel/core` builds per-core-type tables; whole-project
gate green; coverage ≥ 85% for the new `lib/*` crate (§7).

### P3a — Capability-gated fill/zero (`ByPriority`) — **done**

**Delivered.** The page-zero family (`lib/pagezero`) is a capability-gated
(`ByPriority`) consumer, resolved once at bring-up against the migration-safe
common feature set:

- New `DcZva` (aarch64, `DCZID_EL0.DZP`) and `Erms` (x86_64, `CPUID.7:EBX.9`)
  `CpuFeatureSet` bits + per-port detection (each the sole reader of its ID
  source; `dczva_usable` pure and host-tested).
- `lib/pagezero`: portable byte-fill baseline (`zero_portable`), aarch64
  `DC ZVA` candidate (reads the `DCZID_EL0` block size; head/aligned-middle/
  tail so it is correct for any base/length) and x86_64 ERMS `rep stosb`
  candidate, behind the `build.rs`-emitted `pagezero_<arch>` cfg (no
  `cfg(target_arch)` in source). Selected + self-verified (byte-identical to
  the fill over a fixed length/alignment vector, including that it zeroes
  *exactly* the region) + host-fuzzed, fail-closed to the baseline.
- Consumer: the `kernel/mem` frame scrub (`anon::zero_frame`,
  `spawn::fill_frame`) routes through `tairix_pagezero::zero`;
  `kernel/core::cpuops::resolve_accelerated_ops` resolves it once and audits
  the choice (`CpuOpsRoutineSelected`).

**Why `ByPriority`, not `ByBenchmark`:** a block-zero primitive is
unconditionally faster when present and bit-identical, so the choice is pure
capability (as in Linux). A single kernel-wide routine (one set-once fn
pointer, no per-call per-CPU table lookup) resolved against the *intersection*
set is correct because a kernel routine may migrate between cores.

### P3b — The `ByBenchmark` axis (bounded, deterministic boot microbenchmark)

**Status: blocked on an open design question — the harness is built, its real
consumers are not.** The `lib/cpuops` `BenchHarness` + `ByBenchmark` selection
policy are complete and host-tested against a fake counter (P2). What is *not*
done is a real consumer, because the families where a benchmark genuinely
decides are userland and the cycle counter is kernel-only (see the status
header). **Before P3b consumers land, the userland-measurement mechanism must
be designed and agreed (§15.7).**

**Scope in one line (once unblocked):** wire the userland measurement path,
then move the secret-free, bit-identical userland families onto `ByBenchmark`.

**Deliverables — the harness (`lib/cpuops/src/bench.rs`):**

- `pub struct BenchHarness<'c> { cycles: &'c dyn CpuCycles, iters: u32,
  rounds: u32 }` — takes the P1 `CpuCycles` handle by injection (the crate
  stays `no_std`/no-arch — §2.20). Fixed `iters` over a fixed, warmed input
  buffer; `rounds` measurements reduced by **median** (invariant: reject
  noise, never "loop until a threshold" — §2.1); bounded one-shot, no
  busy-wait (§2.23).
- `pub fn fastest<T: Copy, In>(&self, survivors: &[Candidate<T>], run: fn(T,
  &In) -> RefOut, warm: &In) -> usize` — returns the index of the lowest
  median cycle count; ties break to the earliest (lowest priority index) for
  determinism. Called by the `Selector::choose` `ByBenchmark` arm added in P2.
- Because measurement is inherently machine-dependent, the *only*
  nondeterminism the framework introduces is which verified-correct candidate
  wins; the pin (P2) makes even that deterministic when required (invariant
  10, §19.3 reproducibility).

**Deliverables — per-core-type measurement (invariant 9):** the bring-up
wiring (P2, `kernel/core`) benchmarks once per *distinct* `CoreType` /
cluster (big.LITTLE, Intel hybrid) as those CPUs come up, caching the result
in `OpsTables`; the boot CPU's result is never imposed on a different core
type.

**Deliverables — `ByBenchmark` consumers (secret-free, bit-identical only —
invariant 8; all pending the userland-measurement design above):**

- Framebuffer blit/blend/fill: `lib/raster/src/surface.rs`
  (`fill_rect`, `fill_round_rect`, `blit`) — route the inner fill/blend loop
  through an ops-table fn pointer; NEON/AVX candidates gated on the feature
  bit, portable baseline unchanged. **Userland** — needs the measurement path.
- RFC-1071 IP checksum: `lib/net/src/checksum.rs`
  (`internet_checksum` / `Checksum::push`/`finish`) — a SIMD folding
  candidate vs the portable ones'-complement baseline. **Userland** — needs
  the measurement path.
- `memcpy` / `memset`: where a hardware fill/copy dominates unconditionally
  these belong on the *capability* axis with page-zero (P3a), not here; a
  `ByBenchmark` memcpy is warranted only if two feature-legal SIMD widths
  genuinely trade places by microarch, and only with a real caller.
- XOR/parity: **dropped** until a real RAID/FEC/parity consumer exists (a
  family with no caller is speculative/dead surface, §2.4). Land it *with*
  that consumer.
- Crypto stays strictly on the P1/P2 capability axis — **never** here.

**Deliverables — logging + pin:** each `Decision` (family × core type) is
recorded through the `lib/log` sink with a stable event ID (§19.4); the pin
boot parameter forces a named candidate for determinism (CI, debugging,
reproducible-build validation).

**Tests:**

- Harness (host, **fake** `CpuCycles`): bounded (fixed iters/rounds),
  deterministic under the fake, picks the faster of two fake candidates,
  median rejects an injected outlier, honours a pin, tie-breaks to earliest.
- Every benchmarked candidate is bit-identical to its reference across the
  fixed vector (empty/1/unaligned/page-crossing/max) — the correctness gate
  is independent of the speed choice.
- QEMU verticals per arch: the extension-using family is chosen when the
  feature is present and the baseline when the feature is masked off (via a
  test hook that forces a reduced `CpuFeatureSet`); heterogeneous-core keying
  exercised where the emulator models it (e.g. QEMU `-cpu` big.LITTLE combos).

**Acceptance:** the four families route through `ByBenchmark`; measurement is
bounded and never busy-waits; pin makes selection deterministic; whole-project
gate green.

### P4 — Fuzzing, docs, and burn-down

**Scope in one line:** fuzz every accelerated routine against its reference,
document the framework, and update the support matrix — closing the plan.

**Deliverables — fuzzing (§19.6):**

- One `cargo-fuzz`/in-tree harness per family in `lib/cpuops` (and the arch
  candidate crates): feed arbitrary bytes/sizes/alignments to *every*
  candidate and assert bit-identity with the portable reference. A divergence
  or crash is a bug; the input enters the family's regression corpus with a
  unit test (§7).
- Add the new harnesses to `cargo xtask fuzz` (the `--quick` per-PR gate and
  the nightly soak) so they run in `cargo xtask ci`.

**Deliverables — docs (§2.8/§13):**

- New `docs/src/architecture/cpu-feature-dispatch.md` (add to
  `docs/src/SUMMARY.md`): the two axes, the crypto carve-out, per-core-type
  keying, pinning, and the build-time-floor-vs-runtime-ceiling layering
  diagram.
- The P0 image→floor rationale table added to the platform docs
  (`docs/src/platform/`).
- rustdoc on every public `lib/cpuops` and `cpufeatures`/`cpucycles` HAL item
  (the `deny(missing_docs)` in `kernel/arch/api` enforces the HAL half).

**Deliverables — matrix + burn-down:**

- Update the `README.md` feature/architecture support matrix (§13) with a
  per-target accelerated-path row (CRC32, crypto-HW, blit/memcpy).
- Flip this plan's Status to `done` with a concise done-state summary
  (§13 — plans hold current state, not history); confirm §17.2 / `PLAN.md`
  HAL enumeration lists `cpufeatures`/`cpucycles`.

**Acceptance:** every accelerated routine is fuzzed against its reference in
CI; docs build (`cargo xtask docs-check`) with no stale-symbol failures; the
support matrix reflects reality; whole-project gate green.

## Testing (§7)

Each phase's own **Tests** / **Acceptance** block above is authoritative;
this section is the cross-cutting summary of what must be green overall.

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

## Related capability — non-secure FIQ deliverability (scheduled; consumer: the D13 watchdog)

The lockup-watchdog work (`plans/WATCHDOG.md`, `plans/OPEN-DEFECTS.md` D13) is
**building** an aarch64 **non-maskable (FIQ) watchdog self-sample** to observe a
core wedged in a `DAIF.I`-masked section — the only tool that can name the D13
`stress --cpu N` wedge (an untracked IRQ-masked busy-spin the maskable IRQ
cadence cannot see). Whether Group 0 / FIQ is actually delivered to the
non-secure kernel is **platform/firmware-owned** — on the Raspberry Pi 4B
(BCM2711 / GIC-400) it depends on the armstub's `SCR_EL3.FIQ` routing and
`GICD_IGROUPR` group assignment, outside the non-secure kernel's control.

That "is Group 0 / FIQ deliverable to non-secure EL1?" question is a
**boot-time hardware/firmware capability**, so it belongs on *this* framework's
deterministic **capability** axis (probe/read once, never benchmarked —
invariants 1/8), with a fail-closed fallback to the existing buddy detection
when the capability is absent (never a broken channel). The split:

- **Detection is this plan's concern (P1).** The capability is reported through
  the P1 `cpufeatures` slice's honesty vocabulary
  (`FeatureSupport::Supported`/`Unsupported(reason)`/`Pending(note)`). The
  watchdog is a *consumer* that chooses the FIQ cadence vs buddy from it.
- **Mechanism stays in `kernel/arch/aarch64` (`plans/WATCHDOG.md`).** FIQ
  vectoring, Group-0 routing, the FIQ dispatcher arm, and the `DAIF.F`-clear
  execution discipline the self-sample requires are the port's concern, not
  this plan's.
- **Nuance (reconcile in P1):** unlike an ISA feature bit read from
  `ID_AA64ISAR0_EL1`, FIQ-deliverability is **empirically probed** (arm Group
  0/FIQ, mask `DAIF.I`, observe whether an FIQ is taken). P1 must therefore
  accommodate a *probed platform capability* reported through the same
  `FeatureSupport` type — a distinct capability, not a `CpuFeatureSet` ISA bit.

**Dependency:** the watchdog's FIQ self-sample now depends on P1 (unbuilt) as
the charter-correct home for this capability (§2.2/§2.20 forbid an ad-hoc
one-off probe). Either P1 lands first, or a minimal P1 subset sufficient to
host a probed platform capability lands with it; the full staged watchdog plan
(B0–B4, with B0 = this P1 dependency) is in `.junie/fix-details.md`.

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
