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

**Phase dependencies.** P0 is independent (build-layer only) and can land
first or in parallel. P1 (the `cpufeatures`/`cpucycles` HAL slices) is the
prerequisite for P2 and P3 and depends on nothing but the existing HAL. P2
(the `lib/cpuops` framework + first `ByPriority` consumers) depends on P1's
`CpuFeatureSet`. P3 (the `ByBenchmark` axis) depends on P2's selector seam
*and* P1's `CpuCycles` counter. P4 (fuzz/docs/burn-down) depends on whatever
consumers P2/P3 landed. Each phase ends only when the whole-project §7 gate
is green (§2.15); the per-phase **Acceptance** blocks state the phase-local
bar on top of that.

### P0 — Build-time floor (the layer under everything)

**Scope in one line:** give `tools/xtask` a single, total `image → CPU
floor` mapping and thread its `-C target-cpu`/`-C target-feature` flags into
*both* the kernel build and the PIE user-space build, without touching the
shared `.cargo/config.toml` `[target.*]` blocks.

**Why not `.cargo/config.toml`:** the `[target.aarch64-unknown-none]` block
is shared by the RPi image kernel, the QEMU-virt run kernel, and every PIE
user-space cross-build. A floor set there would leak across images and
violate the "floor is per-image" decision. The floor must be injected at the
point each image's binaries are built.

**Deliverables (files + symbols):**

- New module `tools/xtask/src/floor.rs` (add `mod floor;` to
  `tools/xtask/src/commands.rs` or `lib.rs` as the sibling modules are
  declared): the single source of truth.
  - `pub struct CpuFloor { pub target_cpu: Option<&'static str>, pub
    target_features: &'static [&'static str], pub rationale: &'static str }`.
  - `pub fn floor_for_image(image: ImageKind) -> CpuFloor` — total over a
    new `enum ImageKind { AArch64Generic, X86_64Iso, Riscv64Generic,
    AArch64Virt, /* … every shipped image */ }`. Values decided below.
  - `impl CpuFloor { pub fn rustflags(&self) -> Vec<String> }` — emits the
    `["-C","target-cpu=…","-C","target-feature=+a,+b"]` token list (empty
    vec when both are `None`/empty, i.e. a genuinely generic floor).
- **Cargo flag-precedence constraint (get this right).** Setting the
  `CARGO_ENCODED_RUSTFLAGS` / `RUSTFLAGS` env var **replaces** (does not
  merge with) the `.cargo/config.toml` `[target.*]` `rustflags` block —
  cargo takes flags from exactly one source, env outranking config. So the
  injected floor string must **also carry** the flags the shared block
  currently supplies (`-C force-frame-pointers=yes` on every bare-metal
  target; plus the x86_64 soft-backend `--cfg`s). To keep those in one
  place, hoist the per-target base flags into `floor.rs` as
  `base_rustflags(triple)` and have `CpuFloor::rustflags()` = base ⧺ floor,
  so config and the injected set can never diverge (§2.2). (The PIE recipe
  already reconstructs the whole string, so it has the same requirement.)
- Wire into the **kernel** build: `build_platform_image`
  (`tools/xtask/src/commands.rs` ~line 1462) and `build_virt_run_kernel`
  (~line 1601). Both currently rely on the `ctx.cargo()` inherited
  `.cargo/config.toml` flags. Set `CARGO_ENCODED_RUSTFLAGS` (0x1f-joined,
  = `base_rustflags(triple)` ⧺ floor tokens) on the kernel `cargo build`
  `Command` for that image.
- Wire into the **PIE** build: `cross_compile_pie_elf`
  (`tools/xtask/src/commands/pie_build.rs` line 73) already clears
  `RUSTFLAGS`/`CARGO_ENCODED_RUSTFLAGS` and sets `arch.rustflags_env_var()`
  to the PIE link recipe. Extend its signature with a `floor: &CpuFloor`
  parameter and prepend the floor tokens to that string (the PIE recipe's
  existing link flags stand in for the base set for user-space) so kernel and
  user-space in one image share the exact floor (§2.2). Update the two
  callers (`image_drivers`, `image_apps`) to pass the same
  `floor_for_image(image)` the kernel build used — thread the resolved
  `ImageKind`/`CpuFloor` down from `build_platform_image`.

**Floor values (decided).** Each image's floor is chosen from *which
SoCs/PCs it must boot*, and every one is recorded in `floor_for_image` with
its `rationale`:

- `AArch64Generic` (universal ARM media, boots RPi 4 / CM4 / OrangePi /
  other ARMv8 SBCs): floor = baseline **ARMv8.0-A**, i.e. `target_cpu:
  None`, `target_features: []` (the common set A53∩A72∩A76∩Allwinner∩… ≈
  baseline). **Not** `cortex-a72`.
- `X86_64Iso` (boots arbitrary PCs): floor = **`x86-64` (v1)**,
  `target_cpu: Some("x86-64")`, for maximum reach. Raised to `x86-64-v2`
  only if a published minimum-hardware requirement is added and documented.
- `Riscv64Generic`: floor = the base `rv64gc` the triple already implies;
  no extra features baked in.
- `AArch64Virt` / other QEMU-run kernels: a documented model floor may be
  chosen (e.g. the core QEMU `virt` models) since the image is not shipped
  hardware; record it.

Everything above the floor is recovered per booted CPU by P1–P3 runtime
dispatch, so the low floor costs nothing at runtime.

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

**Codegen validation (a real build step, not a claim).** Any floor that
raises `target-cpu`/`target-feature` above the current default must be
proven to actually lower on the freestanding target before it is adopted:
the x86_64 `[target.x86_64-unknown-none]` block already pins *soft* crypto
backends (`chacha20_force_soft`, `poly1305_force_soft`,
`curve25519_dalek_backend="serial"`) to dodge the freestanding-SIMD codegen
crash. When a floor turns a feature on, re-verify those crates still lower;
if enabling a feature reintroduces a codegen crash, **fail the build**
(§2.1), do not ship broken codegen and do not silently drop the feature.
The generic floors decided above deliberately stay at baseline, so P0 as
specified changes no codegen — but the validation step is part of the phase
so a future floor-raise cannot skip it.

**Tests (host, in `tools/xtask`):**

- `floor_for_image` is **total**: a table-driven test over every `ImageKind`
  variant asserts each resolves to a `CpuFloor` with a non-empty `rationale`
  (mirrors the existing `kernel_build_profile_matches_image_profile` test
  style at `commands.rs` ~line 1852).
- **Kernel and PIE share one floor:** a test that, for a given `ImageKind`,
  the token list injected into the kernel `cargo` env equals the token list
  prepended to the `cross_compile_pie_elf` rustflags string (§2.2 — they
  cannot skew).
- `CpuFloor::rustflags()` emits an empty vec for a generic
  (`None`/empty) floor and a well-formed `-C target-cpu=…` / `-C
  target-feature=+…` pair otherwise.

**Acceptance:** every shipped image resolves to a documented floor; the
generic images build byte-for-byte as today (baseline floor is a no-op flag
set); `cargo xtask ci` green.

### P1 — The `cpufeatures` Arch HAL slice (deterministic capability layer)

**Scope in one line:** add a new closed HAL slice that turns each target's
CPU-ID source into one arch-neutral `CpuFeatureSet`, plus a HAL cycle
counter for the P3 harness — modelled slot-for-slot on the existing
`memtag`/`sidechannel` slices.

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
  8).** *No benchmark.* A `ByPriority` family whose candidates are the
  audited `lib/crypto` hardware backend (requires AES/PMULL/SHA present) and
  the audited constant-time software backend (baseline). Selection is
  *availability*, never speed; the chosen backend still comes from
  `lib/crypto` (§2.12). This proves the framework models the crypto carve-out
  without ever letting a benchmark near a secret.

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

### P3 — The `ByBenchmark` axis (bounded, deterministic boot microbenchmark)

**Scope in one line:** turn the P2 `ByBenchmark` seam into a real bounded
one-shot microbenchmark keyed per core type, then move the secret-free,
bit-identical families onto it.

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
invariant 8):**

- `memcpy` / `memset` / page-zero: candidates (portable, NEON/`ASIMD`,
  x86_64 AVX2) behind the ops table; the baseline is the current portable
  loop. First real caller: the `kernel/mem` page-zero path.
- Framebuffer blit/blend/fill: `lib/raster/src/surface.rs`
  (`fill_rect`, `fill_round_rect`, `blit`) — route the inner fill/blend loop
  through an ops-table fn pointer; NEON/AVX candidates gated on the feature
  bit, portable baseline unchanged.
- RFC-1071 IP checksum: `lib/net/src/checksum.rs`
  (`internet_checksum` / `Checksum::push`/`finish`) — a SIMD folding
  candidate vs the portable ones'-complement baseline.
- XOR/parity: the baseline word-XOR with a SIMD candidate (a future RAID/FEC
  consumer — land the family; `ByBenchmark` proven on it).
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
